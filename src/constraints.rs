//! Constraint-system elaboration scaffold.
//!
//! This module is intentionally non-invasive: it extracts a typed view of
//! transactions, fields, keeps, attributes, `when` subtype bodies, and
//! relations from the parsed AST, but codegen still owns current behavior.
//! Follow-on solver work should lower from these schemas instead of emitting
//! Z3 directly from raw syntax.

use std::collections::BTreeMap;

use crate::ast::{
    AttrArg, BinaryOp, BuiltinTy, CallArg, DistEntry, Expr, ExprKind, Item, Param, RelationBody,
    SourceFile, TxnBodyItem, TypeArg, TypeExpr, UnaryOp,
};
use crate::lexer::Span;

#[derive(Debug, Clone)]
pub struct ConstraintElaboration {
    pub transactions: Vec<TxnSchema>,
    pub relations: Vec<RelationSchema>,
    pub errors: Vec<ElaborationError>,
}

impl ConstraintElaboration {
    pub fn transaction(&self, name: &str) -> Option<&TxnSchema> {
        self.transactions.iter().find(|t| t.name == name)
    }

    pub fn relation(&self, name: &str) -> Option<&RelationSchema> {
        self.relations.iter().find(|r| r.name == name)
    }
}

#[derive(Debug, Clone)]
pub struct TxnSchema {
    pub name: String,
    pub span: Span,
    pub fields: Vec<TxnFieldSchema>,
    pub keeps: Vec<ConstraintClause>,
    pub when_subtypes: Vec<WhenSubtypeSchema>,
}

#[derive(Debug, Clone)]
pub struct TxnFieldSchema {
    pub name: String,
    pub span: Span,
    pub ty: FieldTypeSchema,
    pub non_random: bool,
    pub has_default: bool,
    pub attrs: Vec<FieldAttrSchema>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldTypeSchema {
    pub class: FieldTypeClass,
    pub width: Option<u32>,
    pub signedness: Signedness,
    pub enum_domain: Option<EnumDomainSchema>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldTypeClass {
    UInt,
    SInt,
    Bits,
    Bool,
    Bit,
    Int,
    Enum,
    Named,
    UnsupportedBuiltin(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signedness {
    Unsigned,
    Signed,
    NotNumeric,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDomainSchema {
    pub name: String,
    pub variants: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FieldAttrSchema {
    pub name: String,
    pub span: Span,
    pub args: Vec<FieldAttrArgSchema>,
}

#[derive(Debug, Clone)]
pub enum FieldAttrArgSchema {
    Expr(Expr),
    WithinScope(String),
    Dist(Vec<DistEntry>),
}

#[derive(Debug, Clone)]
pub struct ConstraintClause {
    pub origin: ConstraintOrigin,
    pub expr: Expr,
    pub ir: Option<ConstraintExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintOrigin {
    TransactionKeep {
        transaction: String,
        span: Span,
    },
    RandomizeWith {
        span: Span,
    },
    RelationExpansion {
        relation: String,
        span: Span,
    },
    FieldAttribute {
        field: String,
        attr: String,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct WhenSubtypeSchema {
    pub discriminant: Expr,
    pub span: Span,
    pub fields: Vec<TxnFieldSchema>,
    pub keeps: Vec<ConstraintClause>,
    pub when_subtypes: Vec<WhenSubtypeSchema>,
}

#[derive(Debug, Clone)]
pub struct RelationSchema {
    pub name: String,
    pub span: Span,
    pub params: Vec<RelationParamSchema>,
    pub body: RelationBodySchema,
}

#[derive(Debug, Clone)]
pub struct RelationParamSchema {
    pub name: String,
    pub ty: Option<FieldTypeSchema>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum RelationBodySchema {
    Block(Vec<ConstraintClause>),
    Alias(ConstraintClause),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElaborationError {
    pub span: Span,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintExpr {
    Ident(String),
    FieldRef {
        root: String,
        field: String,
    },
    IntLiteral(String),
    BoolLiteral(bool),
    Unary {
        op: ConstraintUnaryOp,
        expr: Box<ConstraintExpr>,
    },
    Binary {
        op: ConstraintBinaryOp,
        lhs: Box<ConstraintExpr>,
        rhs: Box<ConstraintExpr>,
    },
    Membership {
        expr: Box<ConstraintExpr>,
        set: Box<ConstraintExpr>,
    },
    Range {
        lo: Option<Box<ConstraintExpr>>,
        hi: Option<Box<ConstraintExpr>>,
    },
    Set(Vec<ConstraintExpr>),
    RelationCall {
        name: String,
        args: Vec<ConstraintExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintUnaryOp {
    Neg,
    LogicalNot,
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    LogicalAnd,
    LogicalOr,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

pub fn elaborate_constraints(file: &SourceFile) -> ConstraintElaboration {
    let enum_domains = collect_enum_domains(file);
    let mut transactions = Vec::new();
    let mut relations = Vec::new();
    let mut errors = Vec::new();

    for item in &file.items {
        match item {
            Item::Transaction(t) => {
                let (fields, keeps, when_subtypes) =
                    elaborate_txn_items(&t.name.name, &t.body, &enum_domains, &mut errors);
                transactions.push(TxnSchema {
                    name: t.name.name.clone(),
                    span: t.span,
                    fields,
                    keeps,
                    when_subtypes,
                });
            }
            Item::Relation(r) => {
                let params = r
                    .params
                    .iter()
                    .map(|p| elaborate_relation_param(p, &enum_domains))
                    .collect();
                let body = match &r.body {
                    RelationBody::Block(exprs) => RelationBodySchema::Block(
                        exprs
                            .iter()
                            .map(|expr| ConstraintClause {
                                origin: ConstraintOrigin::RelationExpansion {
                                    relation: r.name.name.clone(),
                                    span: expr.span,
                                },
                                expr: expr.clone(),
                                ir: lower_constraint_expr(expr, &mut errors),
                            })
                            .collect(),
                    ),
                    RelationBody::Alias(expr) => RelationBodySchema::Alias(ConstraintClause {
                        origin: ConstraintOrigin::RelationExpansion {
                            relation: r.name.name.clone(),
                            span: expr.span,
                        },
                        expr: expr.clone(),
                        ir: lower_constraint_expr(expr, &mut errors),
                    }),
                };
                relations.push(RelationSchema {
                    name: r.name.name.clone(),
                    span: r.span,
                    params,
                    body,
                });
            }
            _ => {}
        }
    }

    ConstraintElaboration {
        transactions,
        relations,
        errors,
    }
}

fn collect_enum_domains(file: &SourceFile) -> BTreeMap<String, EnumDomainSchema> {
    let mut domains = BTreeMap::new();
    for item in &file.items {
        if let Item::Enum(e) = item {
            domains.insert(
                e.name.name.clone(),
                EnumDomainSchema {
                    name: e.name.name.clone(),
                    variants: e.variants.iter().map(|v| v.name.clone()).collect(),
                },
            );
        }
    }
    domains
}

fn elaborate_txn_items(
    txn_name: &str,
    items: &[TxnBodyItem],
    enum_domains: &BTreeMap<String, EnumDomainSchema>,
    errors: &mut Vec<ElaborationError>,
) -> (
    Vec<TxnFieldSchema>,
    Vec<ConstraintClause>,
    Vec<WhenSubtypeSchema>,
) {
    let mut fields = Vec::new();
    let mut keeps = Vec::new();
    let mut when_subtypes = Vec::new();

    for item in items {
        match item {
            TxnBodyItem::Field(f) => fields.push(TxnFieldSchema {
                name: f.name.name.clone(),
                span: f.span,
                ty: elaborate_field_type(&f.ty, enum_domains, errors),
                non_random: f.non_random,
                has_default: f.default.is_some(),
                attrs: f
                    .attrs
                    .iter()
                    .map(|a| FieldAttrSchema {
                        name: a.name.name.clone(),
                        span: a.span,
                        args: a
                            .args
                            .iter()
                            .map(|arg| match arg {
                                AttrArg::Expr(e) => FieldAttrArgSchema::Expr(e.clone()),
                                AttrArg::WithinScope(scope) => {
                                    FieldAttrArgSchema::WithinScope(scope.name.clone())
                                }
                                AttrArg::Dist(entries) => FieldAttrArgSchema::Dist(entries.clone()),
                            })
                            .collect(),
                    })
                    .collect(),
            }),
            TxnBodyItem::Keep(k) => keeps.push(ConstraintClause {
                origin: ConstraintOrigin::TransactionKeep {
                    transaction: txn_name.to_string(),
                    span: k.span,
                },
                expr: k.expr.clone(),
                ir: lower_constraint_expr(&k.expr, errors),
            }),
            TxnBodyItem::When(w) => {
                let (fields, keeps, nested) =
                    elaborate_txn_items(txn_name, &w.items, enum_domains, errors);
                when_subtypes.push(WhenSubtypeSchema {
                    discriminant: w.discriminant.clone(),
                    span: w.span,
                    fields,
                    keeps,
                    when_subtypes: nested,
                });
            }
        }
    }

    (fields, keeps, when_subtypes)
}

fn elaborate_relation_param(
    param: &Param,
    enum_domains: &BTreeMap<String, EnumDomainSchema>,
) -> RelationParamSchema {
    RelationParamSchema {
        name: param.name.name.clone(),
        ty: param
            .ty
            .as_ref()
            .map(|ty| elaborate_field_type(ty, enum_domains, &mut Vec::new())),
        span: param.span,
    }
}

fn elaborate_field_type(
    ty: &TypeExpr,
    enum_domains: &BTreeMap<String, EnumDomainSchema>,
    _errors: &mut Vec<ElaborationError>,
) -> FieldTypeSchema {
    match ty {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap => FieldTypeSchema {
                class: FieldTypeClass::UInt,
                width: type_arg_width(args),
                signedness: Signedness::Unsigned,
                enum_domain: None,
            },
            BuiltinTy::SInt | BuiltinTy::SIntCap => FieldTypeSchema {
                class: FieldTypeClass::SInt,
                width: type_arg_width(args),
                signedness: Signedness::Signed,
                enum_domain: None,
            },
            BuiltinTy::Bits => FieldTypeSchema {
                class: FieldTypeClass::Bits,
                width: type_arg_width(args),
                signedness: Signedness::Unsigned,
                enum_domain: None,
            },
            BuiltinTy::Bool | BuiltinTy::BoolLower => FieldTypeSchema {
                class: FieldTypeClass::Bool,
                width: Some(1),
                signedness: Signedness::NotNumeric,
                enum_domain: None,
            },
            BuiltinTy::Bit => FieldTypeSchema {
                class: FieldTypeClass::Bit,
                width: Some(1),
                signedness: Signedness::Unsigned,
                enum_domain: None,
            },
            BuiltinTy::Int => FieldTypeSchema {
                class: FieldTypeClass::Int,
                width: Some(32),
                signedness: Signedness::Signed,
                enum_domain: None,
            },
            other => FieldTypeSchema {
                class: FieldTypeClass::UnsupportedBuiltin(format!("{other:?}")),
                width: None,
                signedness: Signedness::Unknown,
                enum_domain: None,
            },
        },
        TypeExpr::Named { name, .. } => {
            let type_name = name
                .segments
                .last()
                .map(|segment| segment.name.clone())
                .unwrap_or_default();
            if let Some(domain) = enum_domains.get(&type_name) {
                FieldTypeSchema {
                    class: FieldTypeClass::Enum,
                    width: enum_width(domain.variants.len()),
                    signedness: Signedness::Unsigned,
                    enum_domain: Some(domain.clone()),
                }
            } else {
                FieldTypeSchema {
                    class: FieldTypeClass::Named,
                    width: None,
                    signedness: Signedness::Unknown,
                    enum_domain: None,
                }
            }
        }
    }
}

fn lower_constraint_expr(
    expr: &Expr,
    errors: &mut Vec<ElaborationError>,
) -> Option<ConstraintExpr> {
    match lower_constraint_expr_inner(expr) {
        Ok(ir) => Some(ir),
        Err(message) => {
            errors.push(ElaborationError {
                span: expr.span,
                message,
            });
            None
        }
    }
}

fn lower_constraint_expr_inner(expr: &Expr) -> Result<ConstraintExpr, String> {
    match &*expr.kind {
        ExprKind::Ident(id) => Ok(ConstraintExpr::Ident(id.name.clone())),
        ExprKind::Field { target, name } => {
            let target = lower_constraint_expr_inner(target)?;
            match target {
                ConstraintExpr::Ident(root) => Ok(ConstraintExpr::FieldRef {
                    root,
                    field: name.name.clone(),
                }),
                _ => Err(format!(
                    "constraint field access `{}` has unsupported target shape",
                    name.name
                )),
            }
        }
        ExprKind::Int(s) => Ok(ConstraintExpr::IntLiteral(s.clone())),
        ExprKind::Bool(b) => Ok(ConstraintExpr::BoolLiteral(*b)),
        ExprKind::Paren(inner) => lower_constraint_expr_inner(inner),
        ExprKind::Unary { op, expr } => Ok(ConstraintExpr::Unary {
            op: lower_unary_op(*op),
            expr: Box::new(lower_constraint_expr_inner(expr)?),
        }),
        ExprKind::Binary { op, lhs, rhs } => {
            if matches!(op, BinaryOp::In | BinaryOp::Inside) {
                return Ok(ConstraintExpr::Membership {
                    expr: Box::new(lower_constraint_expr_inner(lhs)?),
                    set: Box::new(lower_constraint_expr_inner(rhs)?),
                });
            }
            let op = lower_binary_op(*op).ok_or_else(|| {
                format!("constraint operator `{op:?}` is not supported by typed lowering")
            })?;
            Ok(ConstraintExpr::Binary {
                op,
                lhs: Box::new(lower_constraint_expr_inner(lhs)?),
                rhs: Box::new(lower_constraint_expr_inner(rhs)?),
            })
        }
        ExprKind::Membership { expr, set } => Ok(ConstraintExpr::Membership {
            expr: Box::new(lower_constraint_expr_inner(expr)?),
            set: Box::new(lower_constraint_expr_inner(set)?),
        }),
        ExprKind::RangeLit { lo, hi } => Ok(ConstraintExpr::Range {
            lo: lo
                .as_ref()
                .map(|e| lower_constraint_expr_inner(e).map(Box::new))
                .transpose()?,
            hi: hi
                .as_ref()
                .map(|e| lower_constraint_expr_inner(e).map(Box::new))
                .transpose()?,
        }),
        ExprKind::SetLit(items) => Ok(ConstraintExpr::Set(
            items
                .iter()
                .map(lower_constraint_expr_inner)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        ExprKind::Call { callee, args } => {
            let ExprKind::Ident(id) = &*callee.kind else {
                return Err("constraint call callee must be a relation name".into());
            };
            let lowered_args = args
                .iter()
                .map(|arg| match arg {
                    CallArg::Expr(e) => lower_constraint_expr_inner(e),
                    CallArg::Named { value, .. } => lower_constraint_expr_inner(value),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ConstraintExpr::RelationCall {
                name: id.name.clone(),
                args: lowered_args,
            })
        }
        other => Err(format!(
            "constraint expression `{:?}` is not supported by typed lowering",
            std::mem::discriminant(other)
        )),
    }
}

fn lower_unary_op(op: UnaryOp) -> ConstraintUnaryOp {
    match op {
        UnaryOp::Neg => ConstraintUnaryOp::Neg,
        UnaryOp::Not | UnaryOp::NotKw => ConstraintUnaryOp::LogicalNot,
        UnaryOp::BitNot => ConstraintUnaryOp::BitNot,
    }
}

fn lower_binary_op(op: BinaryOp) -> Option<ConstraintBinaryOp> {
    match op {
        BinaryOp::Add => Some(ConstraintBinaryOp::Add),
        BinaryOp::Sub => Some(ConstraintBinaryOp::Sub),
        BinaryOp::Mul => Some(ConstraintBinaryOp::Mul),
        BinaryOp::Div => Some(ConstraintBinaryOp::Div),
        BinaryOp::Mod => Some(ConstraintBinaryOp::Mod),
        BinaryOp::Eq => Some(ConstraintBinaryOp::Eq),
        BinaryOp::Ne => Some(ConstraintBinaryOp::Ne),
        BinaryOp::Lt => Some(ConstraintBinaryOp::Lt),
        BinaryOp::Le => Some(ConstraintBinaryOp::Le),
        BinaryOp::Gt => Some(ConstraintBinaryOp::Gt),
        BinaryOp::Ge => Some(ConstraintBinaryOp::Ge),
        BinaryOp::AndAnd | BinaryOp::AndKw => Some(ConstraintBinaryOp::LogicalAnd),
        BinaryOp::OrOr | BinaryOp::OrKw => Some(ConstraintBinaryOp::LogicalOr),
        BinaryOp::BitAnd => Some(ConstraintBinaryOp::BitAnd),
        BinaryOp::BitOr => Some(ConstraintBinaryOp::BitOr),
        BinaryOp::BitXor => Some(ConstraintBinaryOp::BitXor),
        BinaryOp::Shl => Some(ConstraintBinaryOp::Shl),
        BinaryOp::Shr => Some(ConstraintBinaryOp::Shr),
        _ => None,
    }
}

fn enum_width(variant_count: usize) -> Option<u32> {
    if variant_count <= 1 {
        Some(1)
    } else {
        Some(usize::BITS - (variant_count - 1).leading_zeros())
    }
}

fn type_arg_width(args: &[TypeArg]) -> Option<u32> {
    let arg = args.first()?;
    match arg {
        TypeArg::Expr(e) => match &*e.kind {
            crate::ast::ExprKind::Int(s) => parse_u32_literal(s),
            _ => None,
        },
        _ => None,
    }
}

fn parse_u32_literal(raw: &str) -> Option<u32> {
    let s = raw.replace('_', "");
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else if let Some(bin) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        u32::from_str_radix(bin, 2).ok()
    } else {
        s.parse().ok()
    }
}
