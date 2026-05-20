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
//!
//! Explicitly out of scope for this phase (structured `LowerError`,
//! never a panic):
//!   - relation application (`relation_name(args)`) — Phase 2b
//!   - field-method calls (`items.len()`)             — Phase 2b
//!   - foreach / ForAll                                 — Phase 2c
//!   - `when` subtype guards                            — Phase 7
//!
//! Errors are collected (up to `MAX_ERRORS`) and returned as
//! `Err(Vec<LowerError>)`; on a clean lowering the result is
//! `Ok(CTypedProblem)`.

use std::collections::BTreeMap;

use crate::ast::{BinaryOp as AstBinaryOp, Expr, ExprKind, UnaryOp as AstUnaryOp};
use crate::constraints::typed::*;
use crate::constraints::{
    ConstraintElaboration, ConstraintOrigin, EnumDomainSchema, EnumVariantSchema,
    FieldTypeClass, FieldTypeSchema, TxnFieldSchema, TxnSchema,
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
    WidthMismatch { op: &'static str, lhs_width: u32, rhs_width: u32, span: Span },
    /// Operand signednesses disagree on an op that requires matching signs.
    SignednessMismatch { op: &'static str, lhs_sign: Sign, rhs_sign: Sign, span: Span },
    /// Bare identifier resolves to neither a field nor an enum variant.
    UnresolvedIdent { name: String, span: Span },
    /// A field-path lookup against `FieldEnv` failed.
    FieldNotFound { path: String, span: Span },
    /// Operands of a logical op are not both Bool.
    NonBoolLogical { op: &'static str, found: CType, span: Span },
    /// Operand of a bitwise op is not BV.
    NonBvBitwise { op: &'static str, found: CType, span: Span },
    /// Shift amount has wrong type / sign.
    BadShiftAmount { found: CType, span: Span },
    /// Element of a `Set` literal does not match the inferred elem type.
    SetElemTypeMismatch { expected: CType, found: CType, span: Span },
    /// `in` rhs is neither a Set nor a Range.
    InRhsNotSetOrRange { found: CType, span: Span },
    /// An AST construct is not supported in v1 typed lowering.
    UnsupportedV1 { feature: &'static str, span: Span },
    /// Generic catch-all for AST nodes the constraint sub-language does
    /// not allow at all (e.g. string literals, fork-call, time literals).
    DisallowedInConstraint { what: &'static str, span: Span },
    /// Failed to parse an integer literal (malformed source).
    IntParseFailed { source: String, span: Span },
}

// ─── Lowering context ───────────────────────────────────────────────

struct LowerCtx<'a> {
    /// Kept for Phase 2b (relation expansion) and Phase 7 (when-subtype
    /// guards) — both reach back into the full elaboration.
    #[allow(dead_code)]
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
    };

    // Re-bind `env.enums` after the variant lookup is built — both use
    // the same id assignment, so id 0 in the lookup matches `env.enums[0]`.
    ctx.env.enums = collect_enum_entries(elab);

    let mut clauses = Vec::new();

    // 1. Transaction-level `keep` clauses.
    for keep in &target.keeps {
        if ctx.at_error_cap() {
            break;
        }
        let expr = lower_top_clause(&mut ctx, &keep.expr);
        let assertion_name = mint_clause_name(&mut ctx, &keep.expr);
        clauses.push(CTypedClause {
            origin: keep.origin.clone(),
            expr,
            assertion_name,
        });
    }

    // 2. `randomize-with` body (if present).
    if let Some(body) = randomize_with_body {
        for expr in body {
            if ctx.at_error_cap() {
                break;
            }
            let lowered = lower_top_clause(&mut ctx, expr);
            let assertion_name = mint_clause_name(&mut ctx, expr);
            clauses.push(CTypedClause {
                origin: ConstraintOrigin::RandomizeWith { span: expr.span },
                expr: lowered,
                assertion_name,
            });
        }
    }

    // 3. solve_order: not extracted from AST yet — left as None until
    //    the AST carries a `solve_order(...)` parse node (today it's a
    //    field attribute the runtime reads ad-hoc).

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
        solve_order: None,
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

    let walk_fields = |fields: &[TxnFieldSchema], record_domain: &mut dyn FnMut(&EnumDomainSchema)| {
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

        // Not yet supported (Phase 2b/2c).  Structured error; no panic.
        ExprKind::Call { callee, .. } => {
            // Distinguish relation call vs field-method call for better
            // diagnostics.
            let feature = match &*callee.kind {
                ExprKind::Field { .. } => "field-method call (e.g. .len())",
                _ => "relation application",
            };
            ctx.record_error(LowerError::UnsupportedV1 { feature, span });
            ctx.bottom(span, CExprKind::BoolLit(false))
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
    // 1. Enum variant?  Bare names in constraint bodies typically
    //    resolve to enum variants when they aren't field paths.
    if let Some(&(domain, variant_idx)) = ctx.enum_variant_lookup.get(name) {
        return CTypedExpr::new(
            CExprKind::EnumLit { domain, variant_idx },
            CType::Enum { domain },
            span,
        );
    }
    // 2. Bare field of the target record?
    let path = FieldPath::single(name);
    if let Some(info) = ctx.env.lookup(&path) {
        return CTypedExpr::new(CExprKind::FieldRef(path), info.ty.clone(), span);
    }
    // 3. Unresolved.
    ctx.record_error(LowerError::UnresolvedIdent {
        name: name.to_string(),
        span,
    });
    ctx.bottom(span, CExprKind::FieldRef(FieldPath::single(name)))
}

fn lower_field_access(
    ctx: &mut LowerCtx<'_>,
    target: &Expr,
    name: &str,
    span: Span,
) -> CTypedExpr {
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
                op: if matches!(cop, CUnaryOp::Neg) { "-" } else { "~" },
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
    CTypedExpr::new(CExprKind::BvLit { value }, CType::BV { width, sign }, lit.span)
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
            CType::BV { width: lw, sign: ls }
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
                if let (CType::Enum { domain: d1 }, CType::Enum { domain: d2 }) =
                    (&lhs.ty, &rhs.ty)
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
            CType::BV { width: lw, sign: ls }
        }
    }
}

fn lower_membership(
    ctx: &mut LowerCtx<'_>,
    elem: &Expr,
    rhs: &Expr,
    span: Span,
) -> CTypedExpr {
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
        (ExprKind::SetLit(items), Some(hint)) => lower_set_lit_with_hint(ctx, items, hint, rhs.span),
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
    let (radix, body): (u32, &str) =
        if let Some(idx) = normalized.find('\'') {
            let rest = &normalized[idx + 1..];
            let (radix_char, rest) = rest.split_at(rest.chars().next().map_or(0, |c| c.len_utf8()));
            match radix_char {
                "h" | "H" => (16, rest),
                "d" | "D" => (10, rest),
                "b" | "B" => (2, rest),
                "o" | "O" => (8, rest),
                _ => (10, &normalized[idx + 1..]),
            }
        } else if let Some(stripped) = normalized.strip_prefix("0x").or_else(|| normalized.strip_prefix("0X")) {
            (16, stripped)
        } else if let Some(stripped) = normalized.strip_prefix("0b").or_else(|| normalized.strip_prefix("0B")) {
            (2, stripped)
        } else if let Some(stripped) = normalized.strip_prefix("0o").or_else(|| normalized.strip_prefix("0O")) {
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

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse_expr_fragment, parse_source};

    fn elaborate(src: &str) -> ConstraintElaboration {
        let file = parse_source(src).expect("parse_source failed in test");
        crate::constraints::elaborate_constraints(&file)
    }

    fn lower_with_body(
        src: &str,
        clauses: &[&str],
    ) -> Result<CTypedProblem, Vec<LowerError>> {
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
        assert!(display.contains("==") && display.contains("24:u8"), "got: {display}");
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
            err.iter()
                .any(|e| matches!(e, LowerError::BvLitOutOfRange { width: 8, value: 256, .. })),
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
            err.iter().any(|e| matches!(e, LowerError::UnresolvedIdent { name, .. } if name == "nope")),
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
            err.iter().any(|e| matches!(e, LowerError::NonBoolLogical { op: "clause", .. })),
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
        let names: std::collections::HashSet<&str> =
            problem.constraints.iter().map(|c| c.assertion_name.as_str()).collect();
        assert_eq!(names.len(), problem.constraints.len());
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
