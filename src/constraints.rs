//! Constraint-system elaboration scaffold.
//!
//! This module is intentionally non-invasive: it extracts a typed view of
//! transactions, fields, keeps, attributes, `when` subtype bodies, and
//! relations from the parsed AST, but codegen still owns current behavior.
//! Follow-on solver work should lower from these schemas instead of emitting
//! Z3 directly from raw syntax.

use std::collections::BTreeMap;

use crate::ast::{
    AttrArg, BuiltinTy, DistEntry, Expr, Item, Param, RelationBody, SourceFile, TxnBodyItem,
    TypeArg, TypeExpr,
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

#[derive(Debug, Clone)]
pub enum ConstraintExpr {
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
                            })
                            .collect(),
                    ),
                    RelationBody::Alias(expr) => RelationBodySchema::Alias(ConstraintClause {
                        origin: ConstraintOrigin::RelationExpansion {
                            relation: r.name.name.clone(),
                            span: expr.span,
                        },
                        expr: expr.clone(),
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
