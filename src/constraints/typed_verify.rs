//! Typed constraint IR verifier (Phase 3).
//!
//! The lowering layer owns user-facing type diagnostics.  This verifier is
//! the internal trust boundary between typed lowering and future solver
//! backends: it rejects malformed `CTypedProblem`s even if they were
//! hand-built or produced by a buggy lowering pass.

use std::collections::{BTreeMap, BTreeSet};

use crate::constraints::typed::*;
use crate::lexer::Span;

pub const MAX_VERIFY_ERRORS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    BottomType {
        span: Span,
    },
    TypeMismatch {
        context: &'static str,
        expected: CType,
        found: CType,
        span: Span,
    },
    FieldNotInEnv {
        path: FieldPath,
        span: Span,
    },
    FieldTypeMismatch {
        path: FieldPath,
        expected: CType,
        found: CType,
        span: Span,
    },
    EnumDomainNotFound {
        domain: EnumDomainId,
        span: Span,
    },
    EnumVariantOutOfRange {
        domain: EnumDomainId,
        variant_idx: u32,
        span: Span,
    },
    ClauseNotBool {
        assertion_name: String,
        found: CType,
        span: Span,
    },
    UnaryOperandType {
        op: CUnaryOp,
        expected: &'static str,
        found: CType,
        span: Span,
    },
    BinaryOperandType {
        op: CBinaryOp,
        expected: &'static str,
        lhs: CType,
        rhs: CType,
        span: Span,
    },
    BinaryWidthMismatch {
        op: CBinaryOp,
        lhs_width: u32,
        rhs_width: u32,
        span: Span,
    },
    BinarySignMismatch {
        op: CBinaryOp,
        lhs_sign: Sign,
        rhs_sign: Sign,
        span: Span,
    },
    SetElemTypeMismatch {
        expected: CType,
        found: CType,
        span: Span,
    },
    MembershipRhsType {
        found: CType,
        span: Span,
    },
    LocalNotInScope {
        local: LocalId,
        span: Span,
    },
    LocalTypeMismatch {
        local: LocalId,
        expected: CType,
        found: CType,
        span: Span,
    },
    SolveOrderFieldNotFound {
        path: FieldPath,
        span: Span,
    },
    DuplicateSolveOrderField {
        path: FieldPath,
        span: Span,
    },
    EmptyAssertionName {
        span: Span,
    },
}

pub fn verify_constraint_problem(problem: &CTypedProblem) -> Result<(), Vec<VerifyError>> {
    let mut verifier = Verifier {
        problem,
        errors: Vec::new(),
        locals: BTreeMap::new(),
    };
    verifier.verify_problem();
    if verifier.errors.is_empty() {
        Ok(())
    } else {
        Err(verifier.errors)
    }
}

struct Verifier<'a> {
    problem: &'a CTypedProblem,
    errors: Vec<VerifyError>,
    locals: BTreeMap<LocalId, CType>,
}

impl<'a> Verifier<'a> {
    fn verify_problem(&mut self) {
        for (path, info) in &self.problem.env.fields {
            self.check_type_concrete(&info.ty, Span::default());
            if path.0.is_empty() {
                self.push(VerifyError::FieldNotInEnv {
                    path: path.clone(),
                    span: Span::default(),
                });
            }
        }

        for clause in &self.problem.constraints {
            if clause.assertion_name.is_empty() {
                self.push(VerifyError::EmptyAssertionName {
                    span: clause.expr.span,
                });
            }
            self.verify_expr(&clause.expr);
            if clause.expr.ty != CType::Bool {
                self.push(VerifyError::ClauseNotBool {
                    assertion_name: clause.assertion_name.clone(),
                    found: clause.expr.ty.clone(),
                    span: clause.expr.span,
                });
            }
        }

        if let Some(order) = &self.problem.solve_order {
            let mut seen = BTreeSet::new();
            for path in order {
                if !self.problem.env.fields.contains_key(path) {
                    self.push(VerifyError::SolveOrderFieldNotFound {
                        path: path.clone(),
                        span: self.problem_span(),
                    });
                }
                if !seen.insert(path.clone()) {
                    self.push(VerifyError::DuplicateSolveOrderField {
                        path: path.clone(),
                        span: self.problem_span(),
                    });
                }
            }
        }
    }

    fn verify_expr(&mut self, expr: &CTypedExpr) {
        self.check_type_concrete(&expr.ty, expr.span);
        match &expr.kind {
            CExprKind::BvLit { value } => {
                let Some((width, _)) = expr.ty.as_bv() else {
                    self.push_type_mismatch("bv literal", CType::uint(1), expr);
                    return;
                };
                if width == 0 || literal_exceeds_width(*value, width) {
                    self.push(VerifyError::TypeMismatch {
                        context: "bv literal width",
                        expected: CType::uint(width.max(1)),
                        found: expr.ty.clone(),
                        span: expr.span,
                    });
                }
            }
            CExprKind::BoolLit(_) => {
                if expr.ty != CType::Bool {
                    self.push_type_mismatch("bool literal", CType::Bool, expr);
                }
            }
            CExprKind::EnumLit {
                domain,
                variant_idx,
            } => {
                self.verify_enum_lit(expr, *domain, *variant_idx);
            }
            CExprKind::FieldRef(path) => {
                self.verify_field_ref(expr, path);
            }
            CExprKind::LocalRef(local) => {
                self.verify_local_ref(expr, *local);
            }
            CExprKind::Unary { op, expr: inner } => {
                self.verify_expr(inner);
                self.verify_unary(expr, *op, inner);
            }
            CExprKind::Binary { op, lhs, rhs } => {
                self.verify_expr(lhs);
                self.verify_expr(rhs);
                self.verify_binary(expr, *op, lhs, rhs);
            }
            CExprKind::InSet { expr: item, set } => {
                self.verify_expr(item);
                self.verify_expr(set);
                self.verify_in_set(expr, item, set);
            }
            CExprKind::InRange { expr: item, lo, hi } => {
                self.verify_expr(item);
                if let Some(lo) = lo {
                    self.verify_expr(lo);
                    self.expect_same_type("range lower bound", &item.ty, lo);
                }
                if let Some(hi) = hi {
                    self.verify_expr(hi);
                    self.expect_same_type("range upper bound", &item.ty, hi);
                }
                self.expect_type("range membership result", CType::Bool, expr);
            }
            CExprKind::Set(items) => {
                self.verify_set(expr, items);
            }
            CExprKind::Range { lo, hi } => {
                self.verify_range(expr, lo.as_deref(), hi.as_deref());
            }
            CExprKind::FieldMethodCall {
                target,
                method,
                args,
            } => {
                self.verify_expr(target);
                for arg in args {
                    self.verify_expr(arg);
                }
                self.verify_field_method(expr, *method, args);
            }
            CExprKind::ForAll { var, iter, body } => {
                self.verify_expr(iter);
                let elem_ty = iterable_elem_type(&iter.ty);
                if let Some(elem_ty) = elem_ty {
                    let prior = self.locals.insert(*var, elem_ty);
                    self.verify_expr(body);
                    if let Some(old) = prior {
                        self.locals.insert(*var, old);
                    } else {
                        self.locals.remove(var);
                    }
                } else {
                    self.push(VerifyError::MembershipRhsType {
                        found: iter.ty.clone(),
                        span: iter.span,
                    });
                    self.verify_expr(body);
                }
                self.expect_type("forall body", CType::Bool, body);
                self.expect_type("forall result", CType::Bool, expr);
            }
        }
    }

    fn verify_enum_lit(&mut self, expr: &CTypedExpr, domain: EnumDomainId, variant_idx: u32) {
        match &expr.ty {
            CType::Enum { domain: ty_domain } if *ty_domain == domain => {}
            _ => self.push(VerifyError::TypeMismatch {
                context: "enum literal",
                expected: CType::Enum { domain },
                found: expr.ty.clone(),
                span: expr.span,
            }),
        }
        let Some(entry) = self.problem.env.enum_by_id(domain) else {
            self.push(VerifyError::EnumDomainNotFound {
                domain,
                span: expr.span,
            });
            return;
        };
        if variant_idx as usize >= entry.variants.len() {
            self.push(VerifyError::EnumVariantOutOfRange {
                domain,
                variant_idx,
                span: expr.span,
            });
        }
    }

    fn verify_field_ref(&mut self, expr: &CTypedExpr, path: &FieldPath) {
        let Some(info) = self.problem.env.lookup(path) else {
            self.push(VerifyError::FieldNotInEnv {
                path: path.clone(),
                span: expr.span,
            });
            return;
        };
        if info.ty != expr.ty {
            self.push(VerifyError::FieldTypeMismatch {
                path: path.clone(),
                expected: info.ty.clone(),
                found: expr.ty.clone(),
                span: expr.span,
            });
        }
    }

    fn verify_local_ref(&mut self, expr: &CTypedExpr, local: LocalId) {
        let Some(expected) = self.locals.get(&local).cloned() else {
            self.push(VerifyError::LocalNotInScope {
                local,
                span: expr.span,
            });
            return;
        };
        if expected != expr.ty {
            self.push(VerifyError::LocalTypeMismatch {
                local,
                expected,
                found: expr.ty.clone(),
                span: expr.span,
            });
        }
    }

    fn verify_unary(&mut self, expr: &CTypedExpr, op: CUnaryOp, inner: &CTypedExpr) {
        match op {
            CUnaryOp::Neg | CUnaryOp::BitNot => {
                if !inner.ty.is_bv() {
                    self.push(VerifyError::UnaryOperandType {
                        op,
                        expected: "bit-vector",
                        found: inner.ty.clone(),
                        span: expr.span,
                    });
                }
                self.expect_same_type("unary result", &inner.ty, expr);
            }
            CUnaryOp::LogicalNot => {
                if inner.ty != CType::Bool {
                    self.push(VerifyError::UnaryOperandType {
                        op,
                        expected: "bool",
                        found: inner.ty.clone(),
                        span: expr.span,
                    });
                }
                self.expect_type("logical-not result", CType::Bool, expr);
            }
        }
    }

    fn verify_binary(
        &mut self,
        expr: &CTypedExpr,
        op: CBinaryOp,
        lhs: &CTypedExpr,
        rhs: &CTypedExpr,
    ) {
        use CBinaryOp::*;
        match op {
            Add | Sub | Mul | Div | Mod | BitAnd | BitOr | BitXor => {
                if let Some((width, sign)) = self.expect_matching_bv(op, lhs, rhs, expr.span) {
                    self.expect_type("binary bv result", CType::BV { width, sign }, expr);
                }
            }
            Eq | Ne => {
                if lhs.ty == CType::Bool && rhs.ty == CType::Bool {
                    self.expect_type("bool equality result", CType::Bool, expr);
                } else if let (CType::Enum { domain: dl }, CType::Enum { domain: dr }) =
                    (&lhs.ty, &rhs.ty)
                {
                    if dl != dr {
                        self.push(VerifyError::BinaryOperandType {
                            op,
                            expected: "same enum domain",
                            lhs: lhs.ty.clone(),
                            rhs: rhs.ty.clone(),
                            span: expr.span,
                        });
                    }
                    self.expect_type("enum equality result", CType::Bool, expr);
                } else {
                    self.expect_matching_bv(op, lhs, rhs, expr.span);
                    self.expect_type("bv equality result", CType::Bool, expr);
                }
            }
            Lt | Le | Gt | Ge => {
                self.expect_matching_bv(op, lhs, rhs, expr.span);
                self.expect_type("comparison result", CType::Bool, expr);
            }
            LogicalAnd | LogicalOr => {
                if lhs.ty != CType::Bool || rhs.ty != CType::Bool {
                    self.push(VerifyError::BinaryOperandType {
                        op,
                        expected: "bool x bool",
                        lhs: lhs.ty.clone(),
                        rhs: rhs.ty.clone(),
                        span: expr.span,
                    });
                }
                self.expect_type("logical result", CType::Bool, expr);
            }
            Shl | Shr => {
                if !lhs.ty.is_bv() {
                    self.push(VerifyError::BinaryOperandType {
                        op,
                        expected: "bit-vector lhs",
                        lhs: lhs.ty.clone(),
                        rhs: rhs.ty.clone(),
                        span: expr.span,
                    });
                }
                match rhs.ty.as_bv() {
                    Some((_, Sign::Unsigned)) => {}
                    _ => self.push(VerifyError::BinaryOperandType {
                        op,
                        expected: "unsigned bit-vector shift amount",
                        lhs: lhs.ty.clone(),
                        rhs: rhs.ty.clone(),
                        span: expr.span,
                    }),
                }
                if lhs.ty.is_bv() {
                    self.expect_same_type("shift result", &lhs.ty, expr);
                }
            }
        }
    }

    fn verify_in_set(&mut self, expr: &CTypedExpr, item: &CTypedExpr, set: &CTypedExpr) {
        let Some(elem_ty) = iterable_elem_type(&set.ty) else {
            self.push(VerifyError::MembershipRhsType {
                found: set.ty.clone(),
                span: set.span,
            });
            self.expect_type("membership result", CType::Bool, expr);
            return;
        };
        if elem_ty != item.ty {
            self.push(VerifyError::SetElemTypeMismatch {
                expected: elem_ty,
                found: item.ty.clone(),
                span: expr.span,
            });
        }
        self.expect_type("membership result", CType::Bool, expr);
    }

    fn verify_set(&mut self, expr: &CTypedExpr, items: &[CTypedExpr]) {
        let CType::Set { elem } = &expr.ty else {
            self.push(VerifyError::TypeMismatch {
                context: "set literal",
                expected: CType::Set {
                    elem: Box::new(CType::Bottom),
                },
                found: expr.ty.clone(),
                span: expr.span,
            });
            return;
        };
        let elem_ty = elem.as_ref().clone();
        self.check_type_concrete(&elem_ty, expr.span);
        for item in items {
            self.verify_expr(item);
            if item.ty != elem_ty {
                self.push(VerifyError::SetElemTypeMismatch {
                    expected: elem_ty.clone(),
                    found: item.ty.clone(),
                    span: item.span,
                });
            }
        }
    }

    fn verify_range(
        &mut self,
        expr: &CTypedExpr,
        lo: Option<&CTypedExpr>,
        hi: Option<&CTypedExpr>,
    ) {
        let CType::Range { elem } = &expr.ty else {
            self.push(VerifyError::TypeMismatch {
                context: "range literal",
                expected: CType::Range {
                    elem: Box::new(CType::Bottom),
                },
                found: expr.ty.clone(),
                span: expr.span,
            });
            return;
        };
        let elem_ty = elem.as_ref().clone();
        self.check_type_concrete(&elem_ty, expr.span);
        if let Some(lo) = lo {
            self.verify_expr(lo);
            self.expect_same_type("range lower bound", &elem_ty, lo);
        }
        if let Some(hi) = hi {
            self.verify_expr(hi);
            self.expect_same_type("range upper bound", &elem_ty, hi);
        }
    }

    fn verify_field_method(
        &mut self,
        expr: &CTypedExpr,
        method: BuiltinMethod,
        args: &[CTypedExpr],
    ) {
        match method {
            BuiltinMethod::Len => {
                if !args.is_empty() {
                    self.push(VerifyError::BinaryOperandType {
                        op: CBinaryOp::Eq,
                        expected: "len() has no arguments",
                        lhs: CType::Bool,
                        rhs: CType::Bool,
                        span: expr.span,
                    });
                }
                match expr.ty.as_bv() {
                    Some((_, Sign::Unsigned)) => {}
                    _ => self.push(VerifyError::TypeMismatch {
                        context: "len() result",
                        expected: CType::uint(32),
                        found: expr.ty.clone(),
                        span: expr.span,
                    }),
                }
            }
        }
    }

    fn expect_matching_bv(
        &mut self,
        op: CBinaryOp,
        lhs: &CTypedExpr,
        rhs: &CTypedExpr,
        span: Span,
    ) -> Option<(u32, Sign)> {
        let Some((lw, ls)) = lhs.ty.as_bv() else {
            self.push(VerifyError::BinaryOperandType {
                op,
                expected: "bit-vector lhs",
                lhs: lhs.ty.clone(),
                rhs: rhs.ty.clone(),
                span,
            });
            return None;
        };
        let Some((rw, rs)) = rhs.ty.as_bv() else {
            self.push(VerifyError::BinaryOperandType {
                op,
                expected: "bit-vector rhs",
                lhs: lhs.ty.clone(),
                rhs: rhs.ty.clone(),
                span,
            });
            return None;
        };
        if lw != rw {
            self.push(VerifyError::BinaryWidthMismatch {
                op,
                lhs_width: lw,
                rhs_width: rw,
                span,
            });
            return None;
        }
        if ls != rs {
            self.push(VerifyError::BinarySignMismatch {
                op,
                lhs_sign: ls,
                rhs_sign: rs,
                span,
            });
            return None;
        }
        Some((lw, ls))
    }

    fn expect_type(&mut self, context: &'static str, expected: CType, expr: &CTypedExpr) {
        if expr.ty != expected {
            self.push(VerifyError::TypeMismatch {
                context,
                expected,
                found: expr.ty.clone(),
                span: expr.span,
            });
        }
    }

    fn expect_same_type(&mut self, context: &'static str, expected: &CType, expr: &CTypedExpr) {
        if &expr.ty != expected {
            self.push(VerifyError::TypeMismatch {
                context,
                expected: expected.clone(),
                found: expr.ty.clone(),
                span: expr.span,
            });
        }
    }

    fn push_type_mismatch(&mut self, context: &'static str, expected: CType, expr: &CTypedExpr) {
        self.push(VerifyError::TypeMismatch {
            context,
            expected,
            found: expr.ty.clone(),
            span: expr.span,
        });
    }

    fn check_type_concrete(&mut self, ty: &CType, span: Span) {
        match ty {
            CType::Bottom => self.push(VerifyError::BottomType { span }),
            CType::BV { width, .. } if *width == 0 => self.push(VerifyError::TypeMismatch {
                context: "zero-width bit-vector",
                expected: CType::uint(1),
                found: ty.clone(),
                span,
            }),
            CType::Range { elem } | CType::Set { elem } => self.check_type_concrete(elem, span),
            CType::Enum { domain } => {
                if self.problem.env.enum_by_id(*domain).is_none() {
                    self.push(VerifyError::EnumDomainNotFound {
                        domain: *domain,
                        span,
                    });
                }
            }
            _ => {}
        }
    }

    fn problem_span(&self) -> Span {
        match &self.problem.origin {
            ProblemOrigin::RandomizeSite { span, .. }
            | ProblemOrigin::BareRandomize { span, .. } => *span,
        }
    }

    fn push(&mut self, err: VerifyError) {
        if self.errors.len() < MAX_VERIFY_ERRORS {
            self.errors.push(err);
        }
    }
}

fn iterable_elem_type(ty: &CType) -> Option<CType> {
    match ty {
        CType::Set { elem } | CType::Range { elem } => Some(elem.as_ref().clone()),
        _ => None,
    }
}

fn literal_exceeds_width(value: u128, width: u32) -> bool {
    if width >= 128 {
        false
    } else {
        value > ((1u128 << width) - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constraints::{ConstraintOrigin, EnumVariantSchema};

    fn sp() -> Span {
        Span::default()
    }

    fn field_expr(name: &str, ty: CType) -> CTypedExpr {
        CTypedExpr::new(CExprKind::FieldRef(FieldPath::single(name)), ty, sp())
    }

    fn lit(value: u128, ty: CType) -> CTypedExpr {
        CTypedExpr::new(CExprKind::BvLit { value }, ty, sp())
    }

    fn bool_clause(expr: CTypedExpr) -> CTypedClause {
        CTypedClause {
            origin: ConstraintOrigin::RandomizeWith { span: sp() },
            expr,
            assertion_name: "c0".into(),
        }
    }

    fn base_problem(expr: CTypedExpr) -> CTypedProblem {
        let mut env = FieldEnv::new();
        env.fields.insert(
            FieldPath::single("addr"),
            FieldInfo {
                ty: CType::uint(8),
                non_random: false,
                has_default: false,
                attrs: vec![],
            },
        );
        CTypedProblem {
            problem_id: ConstraintProblemId(0),
            origin: ProblemOrigin::BareRandomize {
                record: "Txn".into(),
                span: sp(),
            },
            env,
            constraints: vec![bool_clause(expr)],
            solve_order: None,
        }
    }

    #[test]
    fn accepts_well_typed_problem() {
        let expr = CTypedExpr::new(
            CExprKind::Binary {
                op: CBinaryOp::Eq,
                lhs: Box::new(field_expr("addr", CType::uint(8))),
                rhs: Box::new(lit(7, CType::uint(8))),
            },
            CType::Bool,
            sp(),
        );
        verify_constraint_problem(&base_problem(expr)).expect("valid problem should verify");
    }

    #[test]
    fn rejects_non_bool_clause_and_bottom_type() {
        let problem = base_problem(CTypedExpr::new(
            CExprKind::BvLit { value: 0 },
            CType::Bottom,
            sp(),
        ));
        let errors = verify_constraint_problem(&problem).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| matches!(e, VerifyError::BottomType { .. })));
        assert!(errors
            .iter()
            .any(|e| matches!(e, VerifyError::ClauseNotBool { .. })));
    }

    #[test]
    fn rejects_field_ref_not_in_env() {
        let expr = CTypedExpr::new(
            CExprKind::Binary {
                op: CBinaryOp::Eq,
                lhs: Box::new(field_expr("missing", CType::uint(8))),
                rhs: Box::new(lit(1, CType::uint(8))),
            },
            CType::Bool,
            sp(),
        );
        let errors = verify_constraint_problem(&base_problem(expr)).unwrap_err();
        assert!(errors.iter().any(
            |e| matches!(e, VerifyError::FieldNotInEnv { path, .. } if path.dotted() == "missing")
        ));
    }

    #[test]
    fn rejects_binary_width_and_sign_mismatches() {
        let expr = CTypedExpr::new(
            CExprKind::Binary {
                op: CBinaryOp::Add,
                lhs: Box::new(field_expr("addr", CType::uint(8))),
                rhs: Box::new(lit(1, CType::sint(16))),
            },
            CType::uint(8),
            sp(),
        );
        let errors = verify_constraint_problem(&base_problem(expr)).unwrap_err();
        assert!(errors.iter().any(|e| matches!(
            e,
            VerifyError::BinaryWidthMismatch {
                lhs_width: 8,
                rhs_width: 16,
                ..
            }
        )));
    }

    #[test]
    fn rejects_enum_variant_out_of_range() {
        let domain = EnumDomainId(0);
        let mut problem = base_problem(CTypedExpr::new(
            CExprKind::Binary {
                op: CBinaryOp::Eq,
                lhs: Box::new(CTypedExpr::new(
                    CExprKind::EnumLit {
                        domain,
                        variant_idx: 2,
                    },
                    CType::Enum { domain },
                    sp(),
                )),
                rhs: Box::new(CTypedExpr::new(
                    CExprKind::EnumLit {
                        domain,
                        variant_idx: 0,
                    },
                    CType::Enum { domain },
                    sp(),
                )),
            },
            CType::Bool,
            sp(),
        ));
        problem.env.enums.push(EnumDomainEntry {
            id: domain,
            name: "Op".into(),
            variants: vec![EnumVariantSchema {
                enum_name: "Op".into(),
                variant: "READ".into(),
                index: 0,
            }],
        });
        let errors = verify_constraint_problem(&problem).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| { matches!(e, VerifyError::EnumVariantOutOfRange { variant_idx: 2, .. }) }));
    }

    #[test]
    fn rejects_missing_and_duplicate_solve_order_fields() {
        let expr = CTypedExpr::new(
            CExprKind::Binary {
                op: CBinaryOp::Eq,
                lhs: Box::new(field_expr("addr", CType::uint(8))),
                rhs: Box::new(lit(1, CType::uint(8))),
            },
            CType::Bool,
            sp(),
        );
        let mut problem = base_problem(expr);
        problem.solve_order = Some(vec![
            FieldPath::single("addr"),
            FieldPath::single("addr"),
            FieldPath::single("missing"),
        ]);
        let errors = verify_constraint_problem(&problem).unwrap_err();
        assert!(errors.iter().any(|e| {
            matches!(e, VerifyError::DuplicateSolveOrderField { path, .. } if path.dotted() == "addr")
        }));
        assert!(errors.iter().any(|e| {
            matches!(e, VerifyError::SolveOrderFieldNotFound { path, .. } if path.dotted() == "missing")
        }));
    }
}
