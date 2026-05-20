//! AST → typed constraint IR lowering (Phase 2).
//!
//! See `docs/constraint-ir-design.md` §"Layer 2 — AST → IR lowering".
//!
//! This module is non-invasive: no production code path calls
//! `lower_problem` yet.  It exists so the lowering can be unit-tested
//! and run over every fixture's randomize site via a `harc dump-…` CLI
//! before the solver backend (Phase 4) replaces the inline Z3 emission
//! in `cpp_tb.rs`.
//!
//! Scope of this phase (what is supported):
//!   - integer / boolean literals with width inference
//!   - bare-enum-variant identifiers (resolved against `FieldEnv::enums`)
//!   - field references (`p.value`, `addr`)
//!   - binary ops (arithmetic, comparison, logical, bitwise, shift)
//!   - unary ops (neg, not, bitnot)
//!   - parenthesized expressions
//!   - `in` membership against `Set` and `Range`
//!   - set and range literals
//!   - relation application, including recursive alias/block expansion
//!   - list `.len()` and foreach-list item constraints
//!
//! Explicitly out of scope for this phase (structured `LowerError`,
//! never a panic):
//!   - `when` subtype guards                            — Phase 7
//!
//! Errors are collected (up to `MAX_ERRORS`) and returned as
//! `Err(Vec<LowerError>)`; on a clean lowering the result is
//! `Ok(CTypedProblem)`.

use std::collections::BTreeMap;

use crate::ast::{BinaryOp as AstBinaryOp, CallArg, Expr, ExprKind, UnaryOp as AstUnaryOp};
use crate::constraints::typed::*;
use crate::constraints::{
    ConstraintElaboration, ConstraintOrigin, EnumDomainSchema, EnumVariantSchema,
    FieldAttrArgSchema, FieldTypeClass, FieldTypeSchema, RelationBodySchema, TxnFieldSchema,
    TxnSchema,
};
use crate::lexer::Span;

/// Maximum number of diagnostics collected before lowering bails out.
/// See `docs/constraint-ir-design.md` §"Open questions" #1.
pub const MAX_ERRORS: usize = 5;

// ─── Errors ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// Integer literal does not fit in the inferred / required width.
    BvLitOutOfRange { width: u32, value: u128, span: Span },
    /// Operand widths disagree on an op that requires matching widths.
    WidthMismatch {
        op: &'static str,
        lhs_width: u32,
        rhs_width: u32,
        span: Span,
    },
    /// Operand signednesses disagree on an op that requires matching signs.
    SignednessMismatch {
        op: &'static str,
        lhs_sign: Sign,
        rhs_sign: Sign,
        span: Span,
    },
    /// Bare identifier resolves to neither a field nor an enum variant.
    UnresolvedIdent { name: String, span: Span },
    /// A field-path lookup against `FieldEnv` failed.
    FieldNotFound { path: String, span: Span },
    /// Operands of a logical op are not both Bool.
    NonBoolLogical {
        op: &'static str,
        found: CType,
        span: Span,
    },
    /// Operand of a bitwise op is not BV.
    NonBvBitwise {
        op: &'static str,
        found: CType,
        span: Span,
    },
    /// Shift amount has wrong type / sign.
    BadShiftAmount { found: CType, span: Span },
    /// Element of a `Set` literal does not match the inferred elem type.
    SetElemTypeMismatch {
        expected: CType,
        found: CType,
        span: Span,
    },
    /// `in` rhs is neither a Set nor a Range.
    InRhsNotSetOrRange { found: CType, span: Span },
    /// An AST construct is not supported in v1 typed lowering.
    UnsupportedV1 { feature: &'static str, span: Span },
    /// Relation call names a relation that does not exist.
    UnknownRelation { name: String, span: Span },
    /// Relation call supplies the wrong number of arguments.
    RelationArityMismatch {
        name: String,
        expected: usize,
        found: usize,
        span: Span,
    },
    /// Relation expansion is recursive.
    RecursiveRelation { name: String, span: Span },
    /// Generic catch-all for AST nodes the constraint sub-language does
    /// not allow at all (e.g. string literals, fork-call, time literals).
    DisallowedInConstraint { what: &'static str, span: Span },
    /// Failed to parse an integer literal (malformed source).
    IntParseFailed { source: String, span: Span },
}

// ─── Lowering context ───────────────────────────────────────────────

struct LowerCtx<'a> {
    /// Full elaboration context used for relation expansion now and
    /// `when` subtype guards in a later phase.
    elab: &'a ConstraintElaboration,
    /// Kept for diagnostic messages that name the target record.
    #[allow(dead_code)]
    target: &'a TxnSchema,
    env: FieldEnv,
    errors: Vec<LowerError>,
    /// Variant name → (domain id, variant index).  Built from `elab`'s
    /// enum domains.  Unambiguous globally per HARC spec (variants live
    /// in a single global namespace).
    enum_variant_lookup: BTreeMap<String, (EnumDomainId, u32)>,
    next_clause_seq: u32,
    next_local_id: u32,
    locals: BTreeMap<String, (LocalId, CType)>,
}

impl<'a> LowerCtx<'a> {
    fn record_error(&mut self, e: LowerError) {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(e);
        }
    }

    fn at_error_cap(&self) -> bool {
        self.errors.len() >= MAX_ERRORS
    }

    fn bottom(&self, span: Span, kind: CExprKind) -> CTypedExpr {
        CTypedExpr::new(kind, CType::Bottom, span)
    }
}

// ─── Public entry ───────────────────────────────────────────────────

/// Lower a constraint problem rooted at `target`, merging the
/// transaction's `keep` clauses with an optional `randomize-with` body.
///
/// Per the design doc, errors are accumulated up to `MAX_ERRORS` and
/// returned as `Err`.  A clean lowering returns `Ok(CTypedProblem)`.
pub fn lower_problem(
    elab: &ConstraintElaboration,
    target: &TxnSchema,
    randomize_with_body: Option<&[Expr]>,
    site_span: Span,
    problem_id: ConstraintProblemId,
) -> Result<CTypedProblem, Vec<LowerError>> {
    let mut ctx = LowerCtx {
        elab,
        target,
        env: build_field_env(elab, target),
        errors: Vec::new(),
        enum_variant_lookup: build_enum_variant_lookup(elab),
        next_clause_seq: 0,
        next_local_id: 0,
        locals: BTreeMap::new(),
    };

    // Re-bind `env.enums` after the variant lookup is built — both use
    // the same id assignment, so id 0 in the lookup matches `env.enums[0]`.
    ctx.env.enums = collect_enum_entries(elab);

    let mut clauses = Vec::new();
    let mut solve_order: Option<Vec<FieldPath>> = None;

    // 1. Transaction-level `keep` clauses.
    for keep in &target.keeps {
        if ctx.at_error_cap() {
            break;
        }
        for (origin, expr_ast) in
            expand_top_level_clause(&mut ctx, &keep.expr, keep.origin.clone(), &mut Vec::new())
        {
            if ctx.at_error_cap() {
                break;
            }
            let expr = lower_top_clause(&mut ctx, &expr_ast);
            let assertion_name = mint_clause_name(&mut ctx, &expr_ast);
            clauses.push(CTypedClause {
                origin,
                expr,
                assertion_name,
            });
        }
    }

    // 2. `randomize-with` body (if present).
    if let Some(body) = randomize_with_body {
        for expr in body {
            if ctx.at_error_cap() {
                break;
            }
            if let ExprKind::SolveOrder { args } = &*expr.kind {
                merge_solve_order_directive(&mut ctx, args, &mut solve_order, expr.span);
                continue;
            }
            for (origin, expr_ast) in expand_top_level_clause(
                &mut ctx,
                expr,
                ConstraintOrigin::RandomizeWith { span: expr.span },
                &mut Vec::new(),
            ) {
                if ctx.at_error_cap() {
                    break;
                }
                let lowered = lower_top_clause(&mut ctx, &expr_ast);
                let assertion_name = mint_clause_name(&mut ctx, &expr_ast);
                clauses.push(CTypedClause {
                    origin,
                    expr: lowered,
                    assertion_name,
                });
            }
        }
    }

    // 3. Field attributes that produce hard constraints. `[range]` is
    //    solver-visible; `[dist]`, `[unique]`, and policy attributes
    //    remain metadata for later runtime/sampling phases.
    for field in &target.fields {
        if ctx.at_error_cap() {
            break;
        }
        for clause in lower_field_attr_constraints(&mut ctx, field) {
            if ctx.at_error_cap() {
                break;
            }
            clauses.push(clause);
        }
    }

    let origin = match randomize_with_body {
        Some(_) => ProblemOrigin::RandomizeSite {
            record: target.name.clone(),
            span: site_span,
        },
        None => ProblemOrigin::BareRandomize {
            record: target.name.clone(),
            span: site_span,
        },
    };

    let problem = CTypedProblem {
        problem_id,
        origin,
        env: ctx.env,
        constraints: clauses,
        solve_order,
    };

    if ctx.errors.is_empty() {
        Ok(problem)
    } else {
        Err(ctx.errors)
    }
}

// ─── FieldEnv construction ─────────────────────────────────────────

fn build_field_env(elab: &ConstraintElaboration, target: &TxnSchema) -> FieldEnv {
    let mut env = FieldEnv::new();
    let enum_entries = collect_enum_entries(elab);
    let domain_id_by_name: BTreeMap<&str, EnumDomainId> = enum_entries
        .iter()
        .map(|e| (e.name.as_str(), e.id))
        .collect();
    for f in &target.fields {
        let path = FieldPath(f.path.clone());
        let ty = ctype_from_field_schema(&f.ty, &domain_id_by_name);
        env.fields.insert(
            path,
            FieldInfo {
                ty,
                non_random: f.non_random,
                has_default: f.has_default,
                attrs: f.attrs.clone(),
            },
        );
    }
    env.enums = enum_entries;
    env
}

fn ctype_from_field_schema(
    schema: &FieldTypeSchema,
    domain_id_by_name: &BTreeMap<&str, EnumDomainId>,
) -> CType {
    match schema.class {
        FieldTypeClass::UInt | FieldTypeClass::Bits => {
            schema.width.map(CType::uint).unwrap_or(CType::Bottom)
        }
        FieldTypeClass::SInt => schema.width.map(CType::sint).unwrap_or(CType::Bottom),
        FieldTypeClass::Bool => CType::Bool,
        FieldTypeClass::Bit => CType::uint(1),
        FieldTypeClass::Int => CType::sint(32),
        FieldTypeClass::List => {
            let Some(list) = &schema.list else {
                return CType::Bottom;
            };
            CType::List {
                elem: Box::new(ctype_from_field_schema(&list.elem, domain_id_by_name)),
                max_len: list.max_len,
            }
        }
        FieldTypeClass::Enum => {
            let dom = schema
                .enum_domain
                .as_ref()
                .and_then(|d| domain_id_by_name.get(d.name.as_str()).copied());
            match dom {
                Some(id) => CType::Enum { domain: id },
                None => CType::Bottom,
            }
        }
        FieldTypeClass::Named | FieldTypeClass::UnsupportedBuiltin(_) => CType::Bottom,
    }
}

fn collect_enum_entries(elab: &ConstraintElaboration) -> Vec<EnumDomainEntry> {
    let mut seen: BTreeMap<String, Vec<EnumVariantSchema>> = BTreeMap::new();
    let mut record_domain = |dom: &EnumDomainSchema| {
        if !seen.contains_key(&dom.name) {
            let variants = dom
                .variants
                .iter()
                .enumerate()
                .map(|(i, v)| EnumVariantSchema {
                    enum_name: dom.name.clone(),
                    variant: v.clone(),
                    index: i,
                })
                .collect();
            seen.insert(dom.name.clone(), variants);
        }
    };

    let walk_fields = |fields: &[TxnFieldSchema],
                       record_domain: &mut dyn FnMut(&EnumDomainSchema)| {
        for f in fields {
            if let Some(dom) = &f.ty.enum_domain {
                record_domain(dom);
            }
        }
    };

    let mut sink = |d: &EnumDomainSchema| record_domain(d);
    for txn in elab.structs.iter().chain(elab.transactions.iter()) {
        walk_fields(&txn.fields, &mut sink);
    }

    seen.into_iter()
        .enumerate()
        .map(|(i, (name, variants))| EnumDomainEntry {
            id: EnumDomainId(i as u32),
            name,
            variants,
        })
        .collect()
}

fn build_enum_variant_lookup(
    elab: &ConstraintElaboration,
) -> BTreeMap<String, (EnumDomainId, u32)> {
    let entries = collect_enum_entries(elab);
    let mut lookup = BTreeMap::new();
    for entry in &entries {
        for (idx, v) in entry.variants.iter().enumerate() {
            lookup.insert(v.variant.clone(), (entry.id, idx as u32));
        }
    }
    lookup
}

// ─── Expression lowering ────────────────────────────────────────────

fn lower_top_clause(ctx: &mut LowerCtx<'_>, expr: &Expr) -> CTypedExpr {
    let lowered = lower_expr(ctx, expr);
    // Clauses must be Bool.
    if !lowered.ty.is_bool() && !lowered.ty.is_bottom() {
        ctx.record_error(LowerError::NonBoolLogical {
            op: "clause",
            found: lowered.ty.clone(),
            span: expr.span,
        });
        return ctx.bottom(expr.span, lowered.kind);
    }
    lowered
}

fn lower_expr(ctx: &mut LowerCtx<'_>, expr: &Expr) -> CTypedExpr {
    let span = expr.span;
    match &*expr.kind {
        ExprKind::Paren(inner) => lower_expr(ctx, inner),

        ExprKind::Int(text) => match parse_int_literal(text) {
            Some(value) => CTypedExpr::new(
                CExprKind::BvLit { value },
                CType::uint(default_unsigned_width(value)),
                span,
            ),
            None => {
                ctx.record_error(LowerError::IntParseFailed {
                    source: text.clone(),
                    span,
                });
                ctx.bottom(span, CExprKind::BvLit { value: 0 })
            }
        },

        ExprKind::Bool(b) => CTypedExpr::new(CExprKind::BoolLit(*b), CType::Bool, span),

        ExprKind::Ident(id) => lower_ident(ctx, &id.name, span),

        ExprKind::Field { target, name } => lower_field_access(ctx, target, &name.name, span),

        ExprKind::Unary { op, expr } => lower_unary(ctx, *op, expr, span),

        ExprKind::Binary { op, lhs, rhs } => lower_binary(ctx, *op, lhs, rhs, span),

        ExprKind::Membership { expr, set } => lower_membership(ctx, expr, set, span),

        ExprKind::RangeLit { lo, hi } => lower_range_lit(ctx, lo.as_ref(), hi.as_ref(), span),

        ExprKind::SetLit(items) => lower_set_lit(ctx, items, span),

        // Constructs not allowed in constraints.
        ExprKind::String(_) => {
            ctx.record_error(LowerError::DisallowedInConstraint {
                what: "string literal",
                span,
            });
            ctx.bottom(span, CExprKind::BoolLit(false))
        }
        ExprKind::Float(_) | ExprKind::Time(_) => {
            ctx.record_error(LowerError::DisallowedInConstraint {
                what: "non-integer numeric literal",
                span,
            });
            ctx.bottom(span, CExprKind::BvLit { value: 0 })
        }

        // Calls are limited to relation applications for now. Field
        // methods and other call shapes remain structured errors; no panic.
        ExprKind::Call { callee, .. } => match &*callee.kind {
            ExprKind::Field { target, name } => {
                lower_field_method_call(ctx, target, &name.name, expr, span)
            }
            ExprKind::Ident(id) => {
                match expand_relation_call_as_expr(ctx, &id.name, expr, &mut Vec::new()) {
                    Some(expanded) => lower_expr(ctx, &expanded),
                    None => ctx.bottom(span, CExprKind::BoolLit(false)),
                }
            }
            _ => {
                ctx.record_error(LowerError::UnsupportedV1 {
                    feature: "constraint call callee",
                    span,
                });
                ctx.bottom(span, CExprKind::BoolLit(false))
            }
        },

        ExprKind::ForEachConstraint { var, iter, body } => {
            lower_foreach_constraint(ctx, &var.name, iter, body, span)
        }

        // Everything else in ExprKind that doesn't belong inside a
        // constraint body: ImplicitSelf, Index, BitSlice, ForkCall, Cast,
        // and a long tail of TB-only constructs.  Structured error.
        _ => {
            ctx.record_error(LowerError::DisallowedInConstraint {
                what: "expression form",
                span,
            });
            ctx.bottom(span, CExprKind::BoolLit(false))
        }
    }
}

fn lower_ident(ctx: &mut LowerCtx<'_>, name: &str, span: Span) -> CTypedExpr {
    // 1. Foreach local?
    if let Some((local, ty)) = ctx.locals.get(name) {
        return CTypedExpr::new(CExprKind::LocalRef(*local), ty.clone(), span);
    }
    // 2. Enum variant?  Bare names in constraint bodies typically
    //    resolve to enum variants when they aren't field paths.
    if let Some(&(domain, variant_idx)) = ctx.enum_variant_lookup.get(name) {
        return CTypedExpr::new(
            CExprKind::EnumLit {
                domain,
                variant_idx,
            },
            CType::Enum { domain },
            span,
        );
    }
    // 3. Bare field of the target record?
    let path = FieldPath::single(name);
    if let Some(info) = ctx.env.lookup(&path) {
        return CTypedExpr::new(CExprKind::FieldRef(path), info.ty.clone(), span);
    }
    // 4. Unresolved.
    ctx.record_error(LowerError::UnresolvedIdent {
        name: name.to_string(),
        span,
    });
    ctx.bottom(span, CExprKind::FieldRef(FieldPath::single(name)))
}

fn lower_field_access(ctx: &mut LowerCtx<'_>, target: &Expr, name: &str, span: Span) -> CTypedExpr {
    let parts = collect_dotted(target);
    let mut path_parts = match parts {
        Some(p) => p,
        None => {
            ctx.record_error(LowerError::DisallowedInConstraint {
                what: "field access target",
                span,
            });
            return ctx.bottom(span, CExprKind::FieldRef(FieldPath::single(name)));
        }
    };
    path_parts.push(name.to_string());
    let full = FieldPath(path_parts);
    if let Some(info) = ctx.env.lookup(&full) {
        return CTypedExpr::new(CExprKind::FieldRef(full.clone()), info.ty.clone(), span);
    }
    // Try the path without its first segment — `randomize(p) with p.addr`
    // declares the target's name on the lhs; the schema only knows about
    // fields under the target, not the target's local name.
    if full.0.len() >= 2 {
        let suffix = FieldPath(full.0[1..].to_vec());
        if let Some(info) = ctx.env.lookup(&suffix) {
            return CTypedExpr::new(CExprKind::FieldRef(suffix), info.ty.clone(), span);
        }
    }
    ctx.record_error(LowerError::FieldNotFound {
        path: full.dotted(),
        span,
    });
    ctx.bottom(span, CExprKind::FieldRef(full))
}

fn lower_field_method_call(
    ctx: &mut LowerCtx<'_>,
    target: &Expr,
    method: &str,
    call: &Expr,
    span: Span,
) -> CTypedExpr {
    let ExprKind::Call { args, .. } = &*call.kind else {
        return ctx.bottom(span, CExprKind::BoolLit(false));
    };
    let target_expr = lower_expr(ctx, target);
    let lowered_args: Vec<CTypedExpr> = args
        .iter()
        .map(|arg| match arg {
            CallArg::Expr(e) | CallArg::Named { value: e, .. } => lower_expr(ctx, e),
        })
        .collect();

    match method {
        "len" => {
            if !matches!(target_expr.ty, CType::List { .. }) {
                ctx.record_error(LowerError::UnsupportedV1 {
                    feature: "len() on non-list field",
                    span,
                });
                return CTypedExpr::new(
                    CExprKind::FieldMethodCall {
                        target: Box::new(target_expr),
                        method: BuiltinMethod::Len,
                        args: lowered_args,
                    },
                    CType::Bottom,
                    span,
                );
            }
            if !lowered_args.is_empty() {
                ctx.record_error(LowerError::UnsupportedV1 {
                    feature: "len() arguments",
                    span,
                });
            }
            CTypedExpr::new(
                CExprKind::FieldMethodCall {
                    target: Box::new(target_expr),
                    method: BuiltinMethod::Len,
                    args: lowered_args,
                },
                CType::uint(32),
                span,
            )
        }
        _ => {
            ctx.record_error(LowerError::UnsupportedV1 {
                feature: "field method",
                span,
            });
            ctx.bottom(span, CExprKind::BoolLit(false))
        }
    }
}

fn lower_foreach_constraint(
    ctx: &mut LowerCtx<'_>,
    var: &str,
    iter: &Expr,
    body: &[Expr],
    span: Span,
) -> CTypedExpr {
    let iter_expr = lower_expr(ctx, iter);
    let elem_ty = match &iter_expr.ty {
        CType::List { elem, .. } | CType::Set { elem } | CType::Range { elem } => {
            elem.as_ref().clone()
        }
        other => {
            let body_ty = if other.is_bottom() {
                CType::Bottom
            } else {
                CType::Bool
            };
            ctx.record_error(LowerError::UnsupportedV1 {
                feature: "foreach over non-iterable constraint expression",
                span: iter.span,
            });
            let local = LocalId(ctx.next_local_id);
            ctx.next_local_id += 1;
            return CTypedExpr::new(
                CExprKind::ForAll {
                    var: local,
                    iter: Box::new(iter_expr),
                    body: Box::new(CTypedExpr::new(CExprKind::BoolLit(false), body_ty, span)),
                },
                CType::Bottom,
                span,
            );
        }
    };

    let local = LocalId(ctx.next_local_id);
    ctx.next_local_id += 1;
    let prior = ctx.locals.insert(var.to_string(), (local, elem_ty));
    let body_exprs: Vec<CTypedExpr> = body.iter().map(|e| lower_top_clause(ctx, e)).collect();
    if let Some(old) = prior {
        ctx.locals.insert(var.to_string(), old);
    } else {
        ctx.locals.remove(var);
    }

    let body_expr = and_join_typed(body_exprs, span);
    CTypedExpr::new(
        CExprKind::ForAll {
            var: local,
            iter: Box::new(iter_expr),
            body: Box::new(body_expr),
        },
        CType::Bool,
        span,
    )
}

fn and_join_typed(exprs: Vec<CTypedExpr>, span: Span) -> CTypedExpr {
    let mut iter = exprs.into_iter();
    let Some(mut acc) = iter.next() else {
        return CTypedExpr::new(CExprKind::BoolLit(true), CType::Bool, span);
    };
    for next in iter {
        let ty = if acc.ty.is_bool() && next.ty.is_bool() {
            CType::Bool
        } else {
            CType::Bottom
        };
        acc = CTypedExpr::new(
            CExprKind::Binary {
                op: CBinaryOp::LogicalAnd,
                lhs: Box::new(acc),
                rhs: Box::new(next),
            },
            ty,
            span,
        );
    }
    acc
}

fn collect_dotted(expr: &Expr) -> Option<Vec<String>> {
    match &*expr.kind {
        ExprKind::Ident(id) => Some(vec![id.name.clone()]),
        ExprKind::Field { target, name } => {
            let mut acc = collect_dotted(target)?;
            acc.push(name.name.clone());
            Some(acc)
        }
        ExprKind::Paren(inner) => collect_dotted(inner),
        _ => None,
    }
}

fn lower_unary(ctx: &mut LowerCtx<'_>, op: AstUnaryOp, expr: &Expr, span: Span) -> CTypedExpr {
    let inner = lower_expr(ctx, expr);
    let cop = match op {
        AstUnaryOp::Neg => CUnaryOp::Neg,
        AstUnaryOp::Not | AstUnaryOp::NotKw => CUnaryOp::LogicalNot,
        AstUnaryOp::BitNot => CUnaryOp::BitNot,
    };
    // Type rules: Neg/BitNot need BV; Not needs Bool.
    let result_ty = match (cop, &inner.ty) {
        (_, CType::Bottom) => CType::Bottom,
        (CUnaryOp::Neg, CType::BV { .. }) | (CUnaryOp::BitNot, CType::BV { .. }) => {
            inner.ty.clone()
        }
        (CUnaryOp::Neg, other) | (CUnaryOp::BitNot, other) => {
            ctx.record_error(LowerError::NonBvBitwise {
                op: if matches!(cop, CUnaryOp::Neg) {
                    "-"
                } else {
                    "~"
                },
                found: other.clone(),
                span,
            });
            CType::Bottom
        }
        (CUnaryOp::LogicalNot, CType::Bool) => CType::Bool,
        (CUnaryOp::LogicalNot, other) => {
            ctx.record_error(LowerError::NonBoolLogical {
                op: "!",
                found: other.clone(),
                span,
            });
            CType::Bottom
        }
    };
    CTypedExpr::new(
        CExprKind::Unary {
            op: cop,
            expr: Box::new(inner),
        },
        result_ty,
        span,
    )
}

fn lower_binary(
    ctx: &mut LowerCtx<'_>,
    op: AstBinaryOp,
    lhs_ast: &Expr,
    rhs_ast: &Expr,
    span: Span,
) -> CTypedExpr {
    // `In` is its own ExprKind in some grammars; AstBinaryOp::In/Inside
    // also appears.  Treat both as membership.
    if matches!(op, AstBinaryOp::In | AstBinaryOp::Inside) {
        return lower_membership(ctx, lhs_ast, rhs_ast, span);
    }

    let cop = match map_binary_op(op) {
        Some(o) => o,
        None => {
            ctx.record_error(LowerError::UnsupportedV1 {
                feature: "binary operator not supported in v1 constraints",
                span,
            });
            return ctx.bottom(span, CExprKind::BoolLit(false));
        }
    };

    let mut lhs = lower_expr(ctx, lhs_ast);
    let mut rhs = lower_expr(ctx, rhs_ast);

    // Width coercion for literal/concrete pairings on BV ops.
    if is_bv_op(cop) {
        let lhs_is_lit = matches!(lhs.kind, CExprKind::BvLit { .. });
        let rhs_is_lit = matches!(rhs.kind, CExprKind::BvLit { .. });
        match (lhs_is_lit, rhs_is_lit, lhs.ty.as_bv(), rhs.ty.as_bv()) {
            (true, false, _, Some((w, s))) => {
                lhs = coerce_literal(ctx, lhs, CType::BV { width: w, sign: s });
            }
            (false, true, Some((w, s)), _) => {
                rhs = coerce_literal(ctx, rhs, CType::BV { width: w, sign: s });
            }
            _ => {}
        }
    }

    let result_ty = type_check_binary(ctx, cop, &lhs, &rhs, span);
    CTypedExpr::new(
        CExprKind::Binary {
            op: cop,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
        result_ty,
        span,
    )
}

fn coerce_literal(ctx: &mut LowerCtx<'_>, lit: CTypedExpr, target: CType) -> CTypedExpr {
    let CExprKind::BvLit { value } = lit.kind else {
        return lit;
    };
    let (width, sign) = match target {
        CType::BV { width, sign } => (width, sign),
        _ => return lit,
    };
    let max_unsigned = if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    };
    let fits = match sign {
        Sign::Unsigned => value <= max_unsigned,
        Sign::Signed => {
            // Treat the source literal as a non-negative magnitude; the
            // signed range is [0, 2^(width-1)-1] for direct
            // representation.  Negative literals come in via UnaryOp::Neg
            // and get range-checked at that boundary.
            if width == 0 {
                value == 0
            } else if width >= 128 {
                true
            } else {
                value < (1u128 << (width - 1))
            }
        }
    };
    if !fits {
        ctx.record_error(LowerError::BvLitOutOfRange {
            width,
            value,
            span: lit.span,
        });
        return CTypedExpr::new(CExprKind::BvLit { value }, CType::Bottom, lit.span);
    }
    CTypedExpr::new(
        CExprKind::BvLit { value },
        CType::BV { width, sign },
        lit.span,
    )
}

fn coerce_expr_to_bv_hint(ctx: &mut LowerCtx<'_>, expr: CTypedExpr, hint: &CType) -> CTypedExpr {
    if matches!(expr.kind, CExprKind::BvLit { .. }) && expr.ty.as_bv() != hint.as_bv() {
        return coerce_literal(ctx, expr, hint.clone());
    }

    let CExprKind::Unary { op, expr: inner } = expr.kind else {
        return expr;
    };
    if !matches!(op, CUnaryOp::Neg) || !matches!(inner.kind, CExprKind::BvLit { .. }) {
        return CTypedExpr::new(CExprKind::Unary { op, expr: inner }, expr.ty, expr.span);
    }
    if !matches!(
        hint,
        CType::BV {
            sign: Sign::Signed,
            ..
        }
    ) {
        return CTypedExpr::new(CExprKind::Unary { op, expr: inner }, expr.ty, expr.span);
    }
    let coerced = coerce_literal(ctx, *inner, hint.clone());
    CTypedExpr::new(
        CExprKind::Unary {
            op,
            expr: Box::new(coerced),
        },
        hint.clone(),
        expr.span,
    )
}

fn lower_field_attr_constraints(
    ctx: &mut LowerCtx<'_>,
    field: &TxnFieldSchema,
) -> Vec<CTypedClause> {
    let mut clauses = Vec::new();
    let path = FieldPath(field.path.clone());
    let Some(info) = ctx.env.lookup(&path) else {
        return clauses;
    };
    let field_ty = info.ty.clone();
    if !field_ty.is_bv() {
        return clauses;
    }

    for attr in &field.attrs {
        if attr.name != "range" {
            continue;
        }
        let (Some(FieldAttrArgSchema::Expr(lo)), Some(FieldAttrArgSchema::Expr(hi))) =
            (attr.args.first(), attr.args.get(1))
        else {
            ctx.record_error(LowerError::UnsupportedV1 {
                feature: "range attribute arguments",
                span: attr.span,
            });
            continue;
        };

        let field_expr = CTypedExpr::new(
            CExprKind::FieldRef(path.clone()),
            field_ty.clone(),
            field.span,
        );
        let lo_lowered = lower_expr(ctx, lo);
        let hi_lowered = lower_expr(ctx, hi);
        let lo_expr = coerce_expr_to_bv_hint(ctx, lo_lowered, &field_ty);
        let hi_expr = coerce_expr_to_bv_hint(ctx, hi_lowered, &field_ty);
        let expr = CTypedExpr::new(
            CExprKind::InRange {
                expr: Box::new(field_expr),
                lo: Some(Box::new(lo_expr)),
                hi: Some(Box::new(hi_expr)),
            },
            CType::Bool,
            attr.span,
        );
        let assertion_name = mint_clause_name(ctx, lo);
        clauses.push(CTypedClause {
            origin: ConstraintOrigin::FieldAttribute {
                field: field.name.clone(),
                attr: attr.name.clone(),
                span: attr.span,
            },
            expr,
            assertion_name,
        });
    }

    clauses
}

fn merge_solve_order_directive(
    ctx: &mut LowerCtx<'_>,
    args: &[Expr],
    solve_order: &mut Option<Vec<FieldPath>>,
    span: Span,
) {
    if args.len() < 2 {
        ctx.record_error(LowerError::UnsupportedV1 {
            feature: "solve_order with fewer than two fields",
            span,
        });
        return;
    }

    let mut fields = Vec::new();
    for arg in args {
        match solve_order_field_path(ctx, arg) {
            Some(path) => fields.push(path),
            None => ctx.record_error(LowerError::UnsupportedV1 {
                feature: "solve_order argument",
                span: arg.span,
            }),
        }
    }

    if fields.is_empty() {
        return;
    }
    solve_order.get_or_insert_with(Vec::new).extend(fields);
}

fn solve_order_field_path(ctx: &mut LowerCtx<'_>, expr: &Expr) -> Option<FieldPath> {
    let mut parts = collect_dotted(expr)?;
    if parts.is_empty() {
        return None;
    }

    let full = FieldPath(parts.clone());
    if ctx.env.lookup(&full).is_some() {
        return Some(full);
    }
    let full_dotted = full.dotted();
    if parts.len() >= 2 {
        let suffix = FieldPath(parts.split_off(1));
        if ctx.env.lookup(&suffix).is_some() {
            return Some(suffix);
        }
    }

    ctx.record_error(LowerError::FieldNotFound {
        path: full_dotted,
        span: expr.span,
    });
    None
}

fn type_check_binary(
    ctx: &mut LowerCtx<'_>,
    op: CBinaryOp,
    lhs: &CTypedExpr,
    rhs: &CTypedExpr,
    span: Span,
) -> CType {
    if lhs.ty.is_bottom() || rhs.ty.is_bottom() {
        return CType::Bottom;
    }
    use CBinaryOp::*;
    match op {
        // Arithmetic + bitwise: BV × BV → BV, matching widths + signs.
        Add | Sub | Mul | Div | Mod | BitAnd | BitOr | BitXor => {
            let (lw, ls) = match lhs.ty.as_bv() {
                Some(t) => t,
                None => {
                    ctx.record_error(LowerError::NonBvBitwise {
                        op: bin_op_str(op),
                        found: lhs.ty.clone(),
                        span,
                    });
                    return CType::Bottom;
                }
            };
            let (rw, rs) = match rhs.ty.as_bv() {
                Some(t) => t,
                None => {
                    ctx.record_error(LowerError::NonBvBitwise {
                        op: bin_op_str(op),
                        found: rhs.ty.clone(),
                        span,
                    });
                    return CType::Bottom;
                }
            };
            if lw != rw {
                ctx.record_error(LowerError::WidthMismatch {
                    op: bin_op_str(op),
                    lhs_width: lw,
                    rhs_width: rw,
                    span,
                });
                return CType::Bottom;
            }
            if ls != rs {
                ctx.record_error(LowerError::SignednessMismatch {
                    op: bin_op_str(op),
                    lhs_sign: ls,
                    rhs_sign: rs,
                    span,
                });
                return CType::Bottom;
            }
            CType::BV {
                width: lw,
                sign: ls,
            }
        }
        // Comparison: BV × BV → Bool (matching widths + signs); also
        // permits Bool × Bool and Enum × Enum (same domain) for Eq/Ne.
        Eq | Ne | Lt | Le | Gt | Ge => {
            // Bool == Bool, Bool != Bool — only Eq/Ne.
            if matches!(op, Eq | Ne) && lhs.ty.is_bool() && rhs.ty.is_bool() {
                return CType::Bool;
            }
            // Enum == Enum (same domain).
            if matches!(op, Eq | Ne) {
                if let (CType::Enum { domain: d1 }, CType::Enum { domain: d2 }) = (&lhs.ty, &rhs.ty)
                {
                    if d1 == d2 {
                        return CType::Bool;
                    }
                    ctx.record_error(LowerError::SignednessMismatch {
                        op: bin_op_str(op),
                        lhs_sign: Sign::Unsigned,
                        rhs_sign: Sign::Unsigned,
                        span,
                    });
                    return CType::Bottom;
                }
            }
            // BV × BV.
            let (lw, ls) = match lhs.ty.as_bv() {
                Some(t) => t,
                None => {
                    ctx.record_error(LowerError::NonBvBitwise {
                        op: bin_op_str(op),
                        found: lhs.ty.clone(),
                        span,
                    });
                    return CType::Bottom;
                }
            };
            let (rw, rs) = match rhs.ty.as_bv() {
                Some(t) => t,
                None => {
                    ctx.record_error(LowerError::NonBvBitwise {
                        op: bin_op_str(op),
                        found: rhs.ty.clone(),
                        span,
                    });
                    return CType::Bottom;
                }
            };
            if lw != rw {
                ctx.record_error(LowerError::WidthMismatch {
                    op: bin_op_str(op),
                    lhs_width: lw,
                    rhs_width: rw,
                    span,
                });
                return CType::Bottom;
            }
            if ls != rs {
                ctx.record_error(LowerError::SignednessMismatch {
                    op: bin_op_str(op),
                    lhs_sign: ls,
                    rhs_sign: rs,
                    span,
                });
                return CType::Bottom;
            }
            CType::Bool
        }
        LogicalAnd | LogicalOr => {
            if !lhs.ty.is_bool() {
                ctx.record_error(LowerError::NonBoolLogical {
                    op: bin_op_str(op),
                    found: lhs.ty.clone(),
                    span,
                });
                return CType::Bottom;
            }
            if !rhs.ty.is_bool() {
                ctx.record_error(LowerError::NonBoolLogical {
                    op: bin_op_str(op),
                    found: rhs.ty.clone(),
                    span,
                });
                return CType::Bottom;
            }
            CType::Bool
        }
        Shl | Shr => {
            let (lw, ls) = match lhs.ty.as_bv() {
                Some(t) => t,
                None => {
                    ctx.record_error(LowerError::NonBvBitwise {
                        op: bin_op_str(op),
                        found: lhs.ty.clone(),
                        span,
                    });
                    return CType::Bottom;
                }
            };
            // Shift amount must be unsigned BV (any width).
            match rhs.ty.as_bv() {
                Some((_, Sign::Unsigned)) => {}
                _ => {
                    ctx.record_error(LowerError::BadShiftAmount {
                        found: rhs.ty.clone(),
                        span,
                    });
                    return CType::Bottom;
                }
            }
            CType::BV {
                width: lw,
                sign: ls,
            }
        }
    }
}

fn lower_membership(ctx: &mut LowerCtx<'_>, elem: &Expr, rhs: &Expr, span: Span) -> CTypedExpr {
    let elem_lowered = lower_expr(ctx, elem);

    // If the elem has a concrete BV type, use it to constrain literals
    // inside the rhs set/range.  Without this, `v in {1, 2, 4}` would
    // infer each literal to its minimum-fit width and then the
    // membership type-check would reject the mismatch against `v`'s
    // declared width.
    let elem_hint: Option<CType> = if elem_lowered.ty.is_bv() {
        Some(elem_lowered.ty.clone())
    } else {
        None
    };
    let rhs_lowered = match (&*rhs.kind, &elem_hint) {
        (ExprKind::SetLit(items), Some(hint)) => {
            lower_set_lit_with_hint(ctx, items, hint, rhs.span)
        }
        (ExprKind::RangeLit { lo, hi }, Some(hint)) => {
            lower_range_lit_with_hint(ctx, lo.as_ref(), hi.as_ref(), hint, rhs.span)
        }
        _ => lower_expr(ctx, rhs),
    };

    let result_ty = match &rhs_lowered.ty {
        CType::Set { elem: set_elem } => {
            if **set_elem != elem_lowered.ty && !elem_lowered.ty.is_bottom() {
                ctx.record_error(LowerError::SetElemTypeMismatch {
                    expected: (**set_elem).clone(),
                    found: elem_lowered.ty.clone(),
                    span,
                });
                CType::Bottom
            } else {
                CType::Bool
            }
        }
        CType::Range { elem: range_elem } => {
            if **range_elem != elem_lowered.ty && !elem_lowered.ty.is_bottom() {
                ctx.record_error(LowerError::SetElemTypeMismatch {
                    expected: (**range_elem).clone(),
                    found: elem_lowered.ty.clone(),
                    span,
                });
                CType::Bottom
            } else {
                CType::Bool
            }
        }
        CType::Bottom => CType::Bottom,
        other => {
            ctx.record_error(LowerError::InRhsNotSetOrRange {
                found: other.clone(),
                span,
            });
            CType::Bottom
        }
    };

    CTypedExpr::new(
        CExprKind::InSet {
            expr: Box::new(elem_lowered),
            set: Box::new(rhs_lowered),
        },
        result_ty,
        span,
    )
}

fn lower_set_lit_with_hint(
    ctx: &mut LowerCtx<'_>,
    items: &[Expr],
    hint: &CType,
    span: Span,
) -> CTypedExpr {
    let lowered: Vec<CTypedExpr> = items
        .iter()
        .map(|e| {
            let lo = lower_expr(ctx, e);
            if matches!(lo.kind, CExprKind::BvLit { .. }) && lo.ty.as_bv() != hint.as_bv() {
                coerce_literal(ctx, lo, hint.clone())
            } else {
                lo
            }
        })
        .collect();
    CTypedExpr::new(
        CExprKind::Set(lowered),
        CType::Set {
            elem: Box::new(hint.clone()),
        },
        span,
    )
}

fn lower_range_lit_with_hint(
    ctx: &mut LowerCtx<'_>,
    lo: Option<&Expr>,
    hi: Option<&Expr>,
    hint: &CType,
    span: Span,
) -> CTypedExpr {
    let coerce = |ctx: &mut LowerCtx<'_>, e: &Expr| -> Box<CTypedExpr> {
        let lo = lower_expr(ctx, e);
        if matches!(lo.kind, CExprKind::BvLit { .. }) && lo.ty.as_bv() != hint.as_bv() {
            Box::new(coerce_literal(ctx, lo, hint.clone()))
        } else {
            Box::new(lo)
        }
    };
    let lo_l = lo.map(|e| coerce(ctx, e));
    let hi_l = hi.map(|e| coerce(ctx, e));
    CTypedExpr::new(
        CExprKind::Range { lo: lo_l, hi: hi_l },
        CType::Range {
            elem: Box::new(hint.clone()),
        },
        span,
    )
}

fn lower_set_lit(ctx: &mut LowerCtx<'_>, items: &[Expr], span: Span) -> CTypedExpr {
    let mut lowered: Vec<CTypedExpr> = items.iter().map(|e| lower_expr(ctx, e)).collect();
    // Determine elem type from the first non-bottom item; coerce
    // literal-typed items to it.
    let elem_ty = lowered
        .iter()
        .find(|e| !e.ty.is_bottom() && !matches!(e.kind, CExprKind::BvLit { .. }))
        .map(|e| e.ty.clone())
        .or_else(|| lowered.first().map(|e| e.ty.clone()))
        .unwrap_or(CType::Bottom);
    // Coerce numeric literals to elem_ty if elem_ty is BV.
    if let CType::BV { .. } = &elem_ty {
        for item in &mut lowered {
            if matches!(item.kind, CExprKind::BvLit { .. }) && item.ty.as_bv() != elem_ty.as_bv() {
                *item = coerce_literal(ctx, item.clone(), elem_ty.clone());
            }
        }
    }
    CTypedExpr::new(
        CExprKind::Set(lowered),
        CType::Set {
            elem: Box::new(elem_ty),
        },
        span,
    )
}

fn lower_range_lit(
    ctx: &mut LowerCtx<'_>,
    lo: Option<&Expr>,
    hi: Option<&Expr>,
    span: Span,
) -> CTypedExpr {
    let lo_l = lo.map(|e| Box::new(lower_expr(ctx, e)));
    let hi_l = hi.map(|e| Box::new(lower_expr(ctx, e)));
    // Element type = first concrete endpoint, else Bottom.
    let elem_ty = lo_l
        .as_ref()
        .filter(|e| !e.ty.is_bottom())
        .map(|e| e.ty.clone())
        .or_else(|| {
            hi_l.as_ref()
                .filter(|e| !e.ty.is_bottom())
                .map(|e| e.ty.clone())
        })
        .unwrap_or(CType::Bottom);
    CTypedExpr::new(
        CExprKind::Range { lo: lo_l, hi: hi_l },
        CType::Range {
            elem: Box::new(elem_ty),
        },
        span,
    )
}

// ─── Helpers ────────────────────────────────────────────────────────

fn parse_int_literal(text: &str) -> Option<u128> {
    let normalized = text.replace('_', "");
    // Strip optional sized prefix like "8'h2A" / "8'd42" / "8'b1010" /
    // "8'o17".  Spec syntax for HARC literals; matches what the lexer
    // emits.
    let (radix, body): (u32, &str) = if let Some(idx) = normalized.find('\'') {
        let rest = &normalized[idx + 1..];
        let (radix_char, rest) = rest.split_at(rest.chars().next().map_or(0, |c| c.len_utf8()));
        match radix_char {
            "h" | "H" => (16, rest),
            "d" | "D" => (10, rest),
            "b" | "B" => (2, rest),
            "o" | "O" => (8, rest),
            _ => (10, &normalized[idx + 1..]),
        }
    } else if let Some(stripped) = normalized
        .strip_prefix("0x")
        .or_else(|| normalized.strip_prefix("0X"))
    {
        (16, stripped)
    } else if let Some(stripped) = normalized
        .strip_prefix("0b")
        .or_else(|| normalized.strip_prefix("0B"))
    {
        (2, stripped)
    } else if let Some(stripped) = normalized
        .strip_prefix("0o")
        .or_else(|| normalized.strip_prefix("0O"))
    {
        (8, stripped)
    } else {
        (10, normalized.as_str())
    };
    u128::from_str_radix(body, radix).ok()
}

fn default_unsigned_width(value: u128) -> u32 {
    // Smallest unsigned width that fits.  Clamped to at least 1.
    if value == 0 {
        1
    } else {
        128 - value.leading_zeros()
    }
}

fn map_binary_op(op: AstBinaryOp) -> Option<CBinaryOp> {
    use AstBinaryOp::*;
    Some(match op {
        Add => CBinaryOp::Add,
        Sub => CBinaryOp::Sub,
        Mul => CBinaryOp::Mul,
        Div => CBinaryOp::Div,
        Mod => CBinaryOp::Mod,
        Eq => CBinaryOp::Eq,
        Ne => CBinaryOp::Ne,
        Lt => CBinaryOp::Lt,
        Le => CBinaryOp::Le,
        Gt => CBinaryOp::Gt,
        Ge => CBinaryOp::Ge,
        AndAnd | AndKw => CBinaryOp::LogicalAnd,
        OrOr | OrKw => CBinaryOp::LogicalOr,
        BitAnd => CBinaryOp::BitAnd,
        BitOr => CBinaryOp::BitOr,
        BitXor => CBinaryOp::BitXor,
        Shl => CBinaryOp::Shl,
        Shr => CBinaryOp::Shr,
        _ => return None,
    })
}

fn is_bv_op(op: CBinaryOp) -> bool {
    use CBinaryOp::*;
    matches!(
        op,
        Add | Sub
            | Mul
            | Div
            | Mod
            | Eq
            | Ne
            | Lt
            | Le
            | Gt
            | Ge
            | BitAnd
            | BitOr
            | BitXor
            | Shl
            | Shr
    )
}

fn bin_op_str(op: CBinaryOp) -> &'static str {
    use CBinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        LogicalAnd => "&&",
        LogicalOr => "||",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
    }
}

fn mint_clause_name(ctx: &mut LowerCtx<'_>, _expr: &Expr) -> String {
    // Stable per-problem assertion name.  Tied to encounter order; that
    // is enough for unsat-core mapping back to ConstraintOrigin.  Z3 only
    // requires uniqueness within one problem.
    let seq = ctx.next_clause_seq;
    ctx.next_clause_seq += 1;
    format!("c_{seq}")
}

// ─── Relation Expansion ────────────────────────────────────────────

fn expand_top_level_clause(
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    default_origin: ConstraintOrigin,
    stack: &mut Vec<String>,
) -> Vec<(ConstraintOrigin, Expr)> {
    if let ExprKind::Call { callee, .. } = &*expr.kind {
        if let ExprKind::Ident(id) = &*callee.kind {
            if let Some(expanded) = expand_top_level_relation_call(ctx, &id.name, expr, stack) {
                return expanded;
            }
        }
    }

    vec![(default_origin, expand_relation_subtree(ctx, expr, stack))]
}

fn expand_top_level_relation_call(
    ctx: &mut LowerCtx<'_>,
    name: &str,
    call: &Expr,
    stack: &mut Vec<String>,
) -> Option<Vec<(ConstraintOrigin, Expr)>> {
    let rel = match ctx.elab.relation(name) {
        Some(rel) => rel.clone(),
        None => {
            ctx.record_error(LowerError::UnknownRelation {
                name: name.to_string(),
                span: call.span,
            });
            return Some(Vec::new());
        }
    };
    let ExprKind::Call { args, .. } = &*call.kind else {
        return None;
    };
    if rel.params.len() != args.len() {
        ctx.record_error(LowerError::RelationArityMismatch {
            name: name.to_string(),
            expected: rel.params.len(),
            found: args.len(),
            span: call.span,
        });
        return Some(Vec::new());
    }
    let subst = build_relation_subst(&rel.params, args);

    if stack.iter().any(|n| n == name) {
        ctx.record_error(LowerError::RecursiveRelation {
            name: name.to_string(),
            span: call.span,
        });
        return Some(Vec::new());
    }
    stack.push(name.to_string());

    let mut out = Vec::new();
    match &rel.body {
        RelationBodySchema::Block(clauses) => {
            for clause in clauses {
                let substituted = substitute_relation_idents(&clause.expr, &subst);
                out.extend(expand_top_level_clause(
                    ctx,
                    &substituted,
                    clause.origin.clone(),
                    stack,
                ));
            }
        }
        RelationBodySchema::Alias(clause) => {
            let substituted = substitute_relation_idents(&clause.expr, &subst);
            out.extend(expand_top_level_clause(
                ctx,
                &substituted,
                clause.origin.clone(),
                stack,
            ));
        }
    }

    stack.pop();
    Some(out)
}

fn expand_relation_call_as_expr(
    ctx: &mut LowerCtx<'_>,
    name: &str,
    call: &Expr,
    stack: &mut Vec<String>,
) -> Option<Expr> {
    let expanded = expand_top_level_relation_call(ctx, name, call, stack)?;
    let exprs: Vec<Expr> = expanded.into_iter().map(|(_, expr)| expr).collect();
    Some(and_join(&exprs, call.span))
}

fn build_relation_subst(
    params: &[crate::constraints::RelationParamSchema],
    args: &[CallArg],
) -> BTreeMap<String, Expr> {
    let mut subst = BTreeMap::new();
    for (param, arg) in params.iter().zip(args.iter()) {
        let expr = match arg {
            CallArg::Expr(expr) => expr.clone(),
            CallArg::Named { value, .. } => value.clone(),
        };
        subst.insert(param.name.clone(), expr);
    }
    subst
}

fn expand_relation_subtree(ctx: &mut LowerCtx<'_>, expr: &Expr, stack: &mut Vec<String>) -> Expr {
    let span = expr.span;
    if let ExprKind::Call { callee, .. } = &*expr.kind {
        if let ExprKind::Ident(id) = &*callee.kind {
            if ctx.elab.relation(&id.name).is_some() {
                if let Some(expanded) = expand_relation_call_as_expr(ctx, &id.name, expr, stack) {
                    return expanded;
                }
            }
        }
    }

    let kind = match &*expr.kind {
        ExprKind::Field { target, name } => ExprKind::Field {
            target: expand_relation_subtree(ctx, target, stack),
            name: name.clone(),
        },
        ExprKind::Index { target, index } => ExprKind::Index {
            target: expand_relation_subtree(ctx, target, stack),
            index: expand_relation_subtree(ctx, index, stack),
        },
        ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
            target: expand_relation_subtree(ctx, target, stack),
            hi: expand_relation_subtree(ctx, hi, stack),
            lo: expand_relation_subtree(ctx, lo, stack),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: expand_relation_subtree(ctx, callee, stack),
            args: args
                .iter()
                .map(|arg| match arg {
                    CallArg::Expr(e) => CallArg::Expr(expand_relation_subtree(ctx, e, stack)),
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: expand_relation_subtree(ctx, value, stack),
                    },
                })
                .collect(),
        },
        ExprKind::ForEachConstraint { var, iter, body } => ExprKind::ForEachConstraint {
            var: var.clone(),
            iter: expand_relation_subtree(ctx, iter, stack),
            body: body
                .iter()
                .map(|e| expand_relation_subtree(ctx, e, stack))
                .collect(),
        },
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: expand_relation_subtree(ctx, expr, stack),
            ty: ty.clone(),
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: expand_relation_subtree(ctx, expr, stack),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: expand_relation_subtree(ctx, lhs, stack),
            rhs: expand_relation_subtree(ctx, rhs, stack),
        },
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::Ternary {
            cond: expand_relation_subtree(ctx, cond, stack),
            then_branch: expand_relation_subtree(ctx, then_branch, stack),
            else_branch: expand_relation_subtree(ctx, else_branch, stack),
        },
        ExprKind::Paren(inner) => ExprKind::Paren(expand_relation_subtree(ctx, inner, stack)),
        ExprKind::Membership { expr, set } => ExprKind::Membership {
            expr: expand_relation_subtree(ctx, expr, stack),
            set: expand_relation_subtree(ctx, set, stack),
        },
        ExprKind::SetLit(items) => ExprKind::SetLit(
            items
                .iter()
                .map(|e| expand_relation_subtree(ctx, e, stack))
                .collect(),
        ),
        ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
            lo: lo.as_ref().map(|e| expand_relation_subtree(ctx, e, stack)),
            hi: hi.as_ref().map(|e| expand_relation_subtree(ctx, e, stack)),
        },
        other => other.clone(),
    };
    Expr::new(kind, span)
}

fn substitute_relation_idents(expr: &Expr, subst: &BTreeMap<String, Expr>) -> Expr {
    let span = expr.span;
    let kind = match &*expr.kind {
        ExprKind::Ident(id) => {
            if let Some(replacement) = subst.get(&id.name) {
                return Expr::new((*replacement.kind).clone(), span);
            }
            ExprKind::Ident(id.clone())
        }
        ExprKind::Field { target, name } => ExprKind::Field {
            target: substitute_relation_idents(target, subst),
            name: name.clone(),
        },
        ExprKind::Index { target, index } => ExprKind::Index {
            target: substitute_relation_idents(target, subst),
            index: substitute_relation_idents(index, subst),
        },
        ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
            target: substitute_relation_idents(target, subst),
            hi: substitute_relation_idents(hi, subst),
            lo: substitute_relation_idents(lo, subst),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: substitute_relation_idents(callee, subst),
            args: args
                .iter()
                .map(|arg| match arg {
                    CallArg::Expr(e) => CallArg::Expr(substitute_relation_idents(e, subst)),
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: substitute_relation_idents(value, subst),
                    },
                })
                .collect(),
        },
        ExprKind::ForEachConstraint { var, iter, body } => {
            let mut scoped = subst.clone();
            scoped.remove(&var.name);
            ExprKind::ForEachConstraint {
                var: var.clone(),
                iter: substitute_relation_idents(iter, subst),
                body: body
                    .iter()
                    .map(|e| substitute_relation_idents(e, &scoped))
                    .collect(),
            }
        }
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: substitute_relation_idents(expr, subst),
            ty: ty.clone(),
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: substitute_relation_idents(expr, subst),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: substitute_relation_idents(lhs, subst),
            rhs: substitute_relation_idents(rhs, subst),
        },
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::Ternary {
            cond: substitute_relation_idents(cond, subst),
            then_branch: substitute_relation_idents(then_branch, subst),
            else_branch: substitute_relation_idents(else_branch, subst),
        },
        ExprKind::Paren(inner) => ExprKind::Paren(substitute_relation_idents(inner, subst)),
        ExprKind::Membership { expr, set } => ExprKind::Membership {
            expr: substitute_relation_idents(expr, subst),
            set: substitute_relation_idents(set, subst),
        },
        ExprKind::SetLit(items) => ExprKind::SetLit(
            items
                .iter()
                .map(|e| substitute_relation_idents(e, subst))
                .collect(),
        ),
        ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
            lo: lo.as_ref().map(|e| substitute_relation_idents(e, subst)),
            hi: hi.as_ref().map(|e| substitute_relation_idents(e, subst)),
        },
        other => other.clone(),
    };
    Expr::new(kind, span)
}

fn and_join(exprs: &[Expr], span: Span) -> Expr {
    if exprs.is_empty() {
        return Expr::new(ExprKind::Bool(true), span);
    }
    let mut iter = exprs.iter().cloned();
    let mut acc = iter.next().expect("checked non-empty");
    for next in iter {
        let joined_span = acc.span.merge(next.span);
        acc = Expr::new(
            ExprKind::Binary {
                op: AstBinaryOp::AndAnd,
                lhs: acc,
                rhs: next,
            },
            joined_span,
        );
    }
    acc
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse_expr_fragment, parse_source};

    fn elaborate(src: &str) -> ConstraintElaboration {
        let file = parse_source(src).expect("parse_source failed in test");
        crate::constraints::elaborate_constraints(&file)
    }

    fn lower_with_body(src: &str, clauses: &[&str]) -> Result<CTypedProblem, Vec<LowerError>> {
        let elab = elaborate(src);
        let txn = elab
            .transactions
            .first()
            .expect("test source must declare a transaction")
            .clone();
        let body_exprs: Vec<Expr> = clauses
            .iter()
            .map(|c| parse_expr_fragment(c).expect("parse_expr_fragment failed"))
            .collect();
        lower_problem(
            &elab,
            &txn,
            Some(&body_exprs),
            Span::default(),
            ConstraintProblemId(0),
        )
    }

    fn lower_bare_txn(src: &str) -> Result<CTypedProblem, Vec<LowerError>> {
        let elab = elaborate(src);
        let txn = elab
            .transactions
            .first()
            .expect("test source must declare a transaction")
            .clone();
        lower_problem(&elab, &txn, None, Span::default(), ConstraintProblemId(0))
    }

    #[test]
    fn lowers_simple_eq() {
        let src = r#"
transaction RegPair
  addr : uint<8>
  value : uint<32>
end transaction RegPair
"#;
        let problem = lower_with_body(src, &["p.addr == 24"]).expect("lowering should succeed");
        assert_eq!(problem.constraints.len(), 1);
        let clause = &problem.constraints[0];
        assert_eq!(clause.expr.ty, CType::Bool);
        let display = format!("{}", clause.expr);
        // FieldRef may resolve via either the bare-field or the
        // strip-target-prefix path; both yield "addr" or "p.addr"
        // textually.  Check the operator + literal coercion shape.
        assert!(
            display.contains("==") && display.contains("24:u8"),
            "got: {display}"
        );
    }

    #[test]
    fn coerces_literal_to_field_width() {
        let src = r#"
transaction X
  v : uint<8>
end transaction X
"#;
        let problem = lower_with_body(src, &["v == 200"]).expect("ok");
        let display = format!("{}", problem.constraints[0].expr);
        assert!(display.contains("200:u8"), "got: {display}");
    }

    #[test]
    fn rejects_literal_overflowing_field_width() {
        let src = r#"
transaction X
  v : uint<8>
end transaction X
"#;
        let err = lower_with_body(src, &["v == 256"]).expect_err("should fail: 256 > u8::MAX");
        assert!(
            err.iter().any(|e| matches!(
                e,
                LowerError::BvLitOutOfRange {
                    width: 8,
                    value: 256,
                    ..
                }
            )),
            "errors: {err:?}"
        );
    }

    #[test]
    fn rejects_field_not_found() {
        let src = r#"
transaction X
  v : uint<8>
end transaction X
"#;
        let err = lower_with_body(src, &["nope == 1"]).expect_err("should fail: no `nope`");
        assert!(
            err.iter()
                .any(|e| matches!(e, LowerError::UnresolvedIdent { name, .. } if name == "nope")),
            "errors: {err:?}"
        );
    }

    #[test]
    fn rejects_clause_that_is_not_bool() {
        // A bare field expression as a clause is not Bool — should
        // produce a NonBoolLogical("clause", ...).
        let src = r#"
transaction X
  v : uint<8>
end transaction X
"#;
        let err = lower_with_body(src, &["v"]).expect_err("should fail: clause is not bool");
        assert!(
            err.iter()
                .any(|e| matches!(e, LowerError::NonBoolLogical { op: "clause", .. })),
            "errors: {err:?}"
        );
    }

    #[test]
    fn lowers_axilite_style_problem() {
        let src = r#"
transaction RegPair
  addr  : uint<8>
  value : uint<32>
end transaction RegPair
"#;
        let problem = lower_with_body(
            src,
            &[
                "p.addr == 24",
                "p.value > 65536",
                "p.value < 2147483648",
                "(p.value & 3) == 0",
            ],
        )
        .expect("lowering should succeed");
        assert_eq!(problem.constraints.len(), 4);
        for c in &problem.constraints {
            assert_eq!(c.expr.ty, CType::Bool, "clause not Bool: {}", c.expr);
        }
        // Every assertion name is unique.
        let names: std::collections::HashSet<&str> = problem
            .constraints
            .iter()
            .map(|c| c.assertion_name.as_str())
            .collect();
        assert_eq!(names.len(), problem.constraints.len());
    }

    #[test]
    fn lowers_alias_relation_call() {
        let src = r#"
transaction Req
  addr : uint<32>
end transaction Req

relation Aligned(r: Req) = r.addr % 4 == 0
"#;
        let problem = lower_with_body(src, &["Aligned(p)"]).expect("relation should inline");
        assert_eq!(problem.constraints.len(), 1);
        assert_eq!(problem.constraints[0].expr.ty, CType::Bool);
        assert!(matches!(
            problem.constraints[0].origin,
            ConstraintOrigin::RelationExpansion { ref relation, .. } if relation == "Aligned"
        ));
        let display = format!("{}", problem.constraints[0].expr);
        assert!(
            display.contains("%") && display.contains("addr"),
            "got: {display}"
        );
    }

    #[test]
    fn lowers_block_relation_call_to_multiple_clauses() {
        let src = r#"
transaction Req
  addr : uint<32>
  len  : uint<8>
end transaction Req

relation Bounded(r: Req)
  r.len in [1..16]
  r.addr % 4 == 0
end relation Bounded
"#;
        let problem = lower_with_body(src, &["Bounded(p)"]).expect("relation should inline");
        assert_eq!(problem.constraints.len(), 2);
        assert!(
            problem
                .constraints
                .iter()
                .all(|c| c.expr.ty == CType::Bool
                    && matches!(c.origin, ConstraintOrigin::RelationExpansion { ref relation, .. } if relation == "Bounded")),
            "{:#?}",
            problem.constraints
        );
    }

    #[test]
    fn lowers_nested_relation_call_inside_expression() {
        let src = r#"
transaction Req
  addr : uint<32>
  len  : uint<8>
end transaction Req

relation Bounded(r: Req)
  r.len in [1..16]
  r.addr % 4 == 0
end relation Bounded

relation HighAddr(r: Req) = r.addr >= 0x10000
relation Legal(r: Req) = Bounded(r) && HighAddr(r)
"#;
        let problem = lower_with_body(src, &["Legal(p)"]).expect("relation should inline");
        assert_eq!(problem.constraints.len(), 1);
        assert_eq!(problem.constraints[0].expr.ty, CType::Bool);
        let display = format!("{}", problem.constraints[0].expr);
        assert!(
            display.contains("&&") && display.contains("len") && display.contains("addr"),
            "got: {display}"
        );
    }

    #[test]
    fn lowers_list_len_method() {
        let src = r#"
transaction Packet
  items : list<uint<8>>
end transaction Packet
"#;
        let problem = lower_with_body(src, &["p.items.len() <= 4"]).expect("len should lower");
        assert_eq!(problem.constraints.len(), 1);
        assert_eq!(problem.constraints[0].expr.ty, CType::Bool);
        let display = format!("{}", problem.constraints[0].expr);
        assert!(
            display.contains(".len()") && display.contains("4:u32"),
            "got: {display}"
        );
    }

    #[test]
    fn lowers_foreach_list_item_constraints() {
        let src = r#"
transaction Packet
  items : list<uint<8>>
  keep for item in items
    item <= 10
  end for
end transaction Packet
"#;
        let problem = lower_bare_txn(src).expect("foreach should lower");
        assert_eq!(problem.constraints.len(), 1);
        assert_eq!(problem.constraints[0].expr.ty, CType::Bool);
        let CExprKind::ForAll { iter, body, .. } = &problem.constraints[0].expr.kind else {
            panic!("expected ForAll, got {}", problem.constraints[0].expr);
        };
        assert!(matches!(iter.ty, CType::List { .. }), "iter: {iter}");
        assert_eq!(body.ty, CType::Bool);
        let display = format!("{}", body);
        assert!(
            display.contains("%0:u8") && display.contains("10:u8"),
            "got: {display}"
        );
    }

    #[test]
    fn lowers_range_attribute_to_field_attribute_clause() {
        let src = r#"
transaction Packet
  len : uint<8> with [range(1, 16)]
end transaction Packet
"#;
        let problem = lower_bare_txn(src).expect("range attr should lower");
        assert_eq!(problem.constraints.len(), 1);
        let clause = &problem.constraints[0];
        assert!(matches!(
            clause.origin,
            ConstraintOrigin::FieldAttribute {
                ref field,
                ref attr,
                ..
            } if field == "len" && attr == "range"
        ));
        let CExprKind::InRange { expr, lo, hi } = &clause.expr.kind else {
            panic!("expected range constraint, got {}", clause.expr);
        };
        assert_eq!(expr.ty, CType::uint(8));
        assert_eq!(lo.as_ref().expect("lo").ty, CType::uint(8));
        assert_eq!(hi.as_ref().expect("hi").ty, CType::uint(8));
        assert_eq!(clause.expr.ty, CType::Bool);
    }

    #[test]
    fn lowers_signed_range_attribute_endpoints() {
        let src = r#"
transaction Packet
  delta : sint<8> with [range(-4, 4)]
end transaction Packet
"#;
        let problem = lower_bare_txn(src).expect("signed range attr should lower");
        assert_eq!(problem.constraints.len(), 1);
        let CExprKind::InRange { expr, lo, hi } = &problem.constraints[0].expr.kind else {
            panic!(
                "expected range constraint, got {}",
                problem.constraints[0].expr
            );
        };
        assert_eq!(expr.ty, CType::sint(8));
        assert_eq!(lo.as_ref().expect("lo").ty, CType::sint(8));
        assert_eq!(hi.as_ref().expect("hi").ty, CType::sint(8));
    }

    #[test]
    fn lowers_solve_order_directive_to_problem_metadata() {
        let src = r#"
transaction Packet
  addr : uint<32>
  len  : uint<8>
end transaction Packet
"#;
        let problem =
            lower_with_body(src, &["solve_order(p.addr, p.len)", "p.addr >= 4"]).expect("ok");
        assert_eq!(problem.constraints.len(), 1);
        let order = problem.solve_order.expect("solve_order metadata");
        assert_eq!(
            order.iter().map(FieldPath::dotted).collect::<Vec<_>>(),
            vec!["addr".to_string(), "len".to_string()]
        );
    }

    #[test]
    fn rejects_non_field_solve_order_argument() {
        let src = r#"
transaction Packet
  addr : uint<32>
  len  : uint<8>
end transaction Packet
"#;
        let err = lower_with_body(src, &["solve_order(p.addr + 1, p.len)"])
            .expect_err("solve_order argument must be a field");
        assert!(
            err.iter().any(|e| matches!(
                e,
                LowerError::UnsupportedV1 {
                    feature: "solve_order argument",
                    ..
                }
            )),
            "errors: {err:?}"
        );
    }

    #[test]
    fn rejects_unknown_solve_order_field() {
        let src = r#"
transaction Packet
  addr : uint<32>
  len  : uint<8>
end transaction Packet
"#;
        let err = lower_with_body(src, &["solve_order(p.addr, p.nope)"])
            .expect_err("unknown solve_order field");
        assert!(
            err.iter()
                .any(|e| matches!(e, LowerError::FieldNotFound { path, .. } if path == "p.nope")),
            "errors: {err:?}"
        );
    }

    #[test]
    fn rejects_relation_arity_mismatch() {
        let src = r#"
transaction Req
  addr : uint<32>
end transaction Req

relation Aligned(r: Req) = r.addr % 4 == 0
"#;
        let err = lower_with_body(src, &["Aligned()"]).expect_err("arity mismatch");
        assert!(
            err.iter().any(|e| matches!(
                e,
                LowerError::RelationArityMismatch {
                    name,
                    expected: 1,
                    found: 0,
                    ..
                } if name == "Aligned"
            )),
            "errors: {err:?}"
        );
    }

    #[test]
    fn rejects_recursive_relation_expansion() {
        let src = r#"
transaction Req
  addr : uint<32>
end transaction Req

relation A(r: Req) = B(r)
relation B(r: Req) = A(r)
"#;
        let err = lower_with_body(src, &["A(p)"]).expect_err("recursive relation");
        assert!(
            err.iter().any(|e| matches!(
                e,
                LowerError::RecursiveRelation { name, .. } if name == "A"
            )),
            "errors: {err:?}"
        );
    }

    #[test]
    fn lowers_set_membership() {
        let src = r#"
transaction X
  v : uint<8>
end transaction X
"#;
        let problem = lower_with_body(src, &["v in {1, 2, 4}"]).expect("ok");
        assert_eq!(problem.constraints.len(), 1);
        assert_eq!(problem.constraints[0].expr.ty, CType::Bool);
    }

    #[test]
    fn parses_hex_decimal_and_binary_literals() {
        assert_eq!(parse_int_literal("0x1A"), Some(26));
        assert_eq!(parse_int_literal("0b1010"), Some(10));
        assert_eq!(parse_int_literal("42"), Some(42));
        assert_eq!(parse_int_literal("1_000"), Some(1000));
    }

    #[test]
    fn collected_errors_are_capped() {
        // Two unresolved idents — both should make it in (under cap of 5).
        let src = r#"
transaction X
  v : uint<8>
end transaction X
"#;
        let err = lower_with_body(src, &["nope1 == 1", "nope2 == 1"]).expect_err("should fail");
        assert!(err.len() >= 2, "errors: {err:?}");
    }
}
