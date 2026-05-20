//! Constraint-system elaboration scaffold.
//!
//! This module is intentionally non-invasive: it extracts a typed view of
//! transactions, fields, keeps, attributes, `when` subtype bodies, and
//! relations from the parsed AST, but codegen still owns current behavior.
//! Follow-on solver work should lower from these schemas instead of emitting
//! Z3 directly from raw syntax.

pub mod typed;

use std::collections::BTreeMap;

use crate::ast::{
    AttrArg, BinaryOp, BuiltinTy, CallArg, DistEntry, Expr, ExprKind, Field, Item, Param,
    RelationBody, SourceFile, TxnBodyItem, TypeArg, TypeExpr, UnaryOp,
};
use crate::lexer::Span;

#[derive(Debug, Clone)]
pub struct ConstraintElaboration {
    pub structs: Vec<TxnSchema>,
    pub transactions: Vec<TxnSchema>,
    pub relations: Vec<RelationSchema>,
    pub errors: Vec<ElaborationError>,
}

impl ConstraintElaboration {
    pub fn transaction(&self, name: &str) -> Option<&TxnSchema> {
        self.transactions.iter().find(|t| t.name == name)
    }

    pub fn struct_schema(&self, name: &str) -> Option<&TxnSchema> {
        self.structs.iter().find(|s| s.name == name)
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
    pub path: Vec<String>,
    pub span: Span,
    pub ty: FieldTypeSchema,
    pub non_random: bool,
    pub has_default: bool,
    pub attrs: Vec<FieldAttrSchema>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldTypeSchema {
    pub class: FieldTypeClass,
    pub type_name: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumVariantSchema {
    pub enum_name: String,
    pub variant: String,
    pub index: usize,
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
    pub refs: ConstraintRefs,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConstraintRefs {
    pub fields: Vec<ConstraintFieldRef>,
    pub enum_variants: Vec<EnumVariantSchema>,
    pub relation_calls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintFieldRef {
    pub root: Option<String>,
    pub field: String,
    pub ty: FieldTypeSchema,
    pub non_random: bool,
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
    FieldMethodCall {
        target: Box<ConstraintExpr>,
        method: String,
        args: Vec<ConstraintExpr>,
    },
    ForEach {
        var: String,
        iter: Box<ConstraintExpr>,
        body: Vec<ConstraintExpr>,
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
    let enum_variants = collect_enum_variants(&enum_domains);
    let record_bodies = collect_record_bodies(file);
    let record_fields = collect_record_fields(file);
    let mut structs = Vec::new();
    let mut transactions = Vec::new();
    let mut relations = Vec::new();
    let mut errors = Vec::new();

    for item in &file.items {
        match item {
            Item::Struct(s) => {
                let (fields, keeps, when_subtypes) = elaborate_txn_items(
                    &s.name.name,
                    &s.body,
                    &enum_domains,
                    &record_bodies,
                    &record_fields,
                    &mut errors,
                );
                structs.push(TxnSchema {
                    name: s.name.name.clone(),
                    span: s.span,
                    fields,
                    keeps,
                    when_subtypes,
                });
            }
            Item::Transaction(t) => {
                let (fields, keeps, when_subtypes) = elaborate_txn_items(
                    &t.name.name,
                    &t.body,
                    &enum_domains,
                    &record_bodies,
                    &record_fields,
                    &mut errors,
                );
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
                                refs: ConstraintRefs::default(),
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
                        refs: ConstraintRefs::default(),
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

    validate_constraint_refs(
        &mut structs,
        &mut transactions,
        &mut relations,
        &enum_variants,
        &mut errors,
    );

    ConstraintElaboration {
        structs,
        transactions,
        relations,
        errors,
    }
}

fn collect_record_bodies(file: &SourceFile) -> BTreeMap<String, Vec<TxnBodyItem>> {
    let mut records = BTreeMap::new();
    for item in &file.items {
        match item {
            Item::Struct(s) => {
                records.insert(s.name.name.clone(), s.body.clone());
            }
            Item::Transaction(t) => {
                records.insert(t.name.name.clone(), t.body.clone());
            }
            _ => {}
        }
    }
    records
}

fn collect_record_fields(file: &SourceFile) -> BTreeMap<String, Vec<Field>> {
    collect_record_bodies(file)
        .into_iter()
        .map(|(name, body)| (name, direct_fields(&body)))
        .collect()
}

fn collect_enum_variants(
    domains: &BTreeMap<String, EnumDomainSchema>,
) -> BTreeMap<String, EnumVariantSchema> {
    let mut variants = BTreeMap::new();
    for domain in domains.values() {
        for (index, variant) in domain.variants.iter().enumerate() {
            variants
                .entry(variant.clone())
                .or_insert_with(|| EnumVariantSchema {
                    enum_name: domain.name.clone(),
                    variant: variant.clone(),
                    index,
                });
        }
    }
    variants
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
    record_bodies: &BTreeMap<String, Vec<TxnBodyItem>>,
    record_fields: &BTreeMap<String, Vec<Field>>,
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
            TxnBodyItem::Field(f) => flatten_field_schema(
                f,
                Vec::new(),
                enum_domains,
                record_fields,
                errors,
                &mut fields,
            ),
            TxnBodyItem::Keep(k) => keeps.push(ConstraintClause {
                origin: ConstraintOrigin::TransactionKeep {
                    transaction: txn_name.to_string(),
                    span: k.span,
                },
                expr: k.expr.clone(),
                ir: lower_constraint_expr(&k.expr, errors),
                refs: ConstraintRefs::default(),
            }),
            TxnBodyItem::When(w) => {
                let (fields, keeps, nested) = elaborate_txn_items(
                    txn_name,
                    &w.items,
                    enum_domains,
                    record_bodies,
                    record_fields,
                    errors,
                );
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

    keeps.extend(collect_nested_record_keeps(
        txn_name,
        items,
        enum_domains,
        record_bodies,
        record_fields,
        errors,
    ));

    (fields, keeps, when_subtypes)
}

fn direct_fields(items: &[TxnBodyItem]) -> Vec<Field> {
    items
        .iter()
        .filter_map(|item| match item {
            TxnBodyItem::Field(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

fn flatten_field_schema(
    field: &Field,
    mut prefix: Vec<String>,
    enum_domains: &BTreeMap<String, EnumDomainSchema>,
    record_fields: &BTreeMap<String, Vec<Field>>,
    errors: &mut Vec<ElaborationError>,
    out: &mut Vec<TxnFieldSchema>,
) {
    prefix.push(field.name.name.clone());
    if let Some(record_name) = record_type_name(&field.ty) {
        if !enum_domains.contains_key(record_name) {
            if let Some(fields) = record_fields.get(record_name) {
                for child in fields {
                    flatten_field_schema(
                        child,
                        prefix.clone(),
                        enum_domains,
                        record_fields,
                        errors,
                        out,
                    );
                }
                return;
            }
        }
    }
    out.push(elaborate_field_schema(field, prefix, enum_domains, errors));
}

fn elaborate_field_schema(
    field: &Field,
    path: Vec<String>,
    enum_domains: &BTreeMap<String, EnumDomainSchema>,
    errors: &mut Vec<ElaborationError>,
) -> TxnFieldSchema {
    TxnFieldSchema {
        name: path.join("."),
        path,
        span: field.span,
        ty: elaborate_field_type(&field.ty, enum_domains, errors),
        non_random: field.non_random,
        has_default: field.default.is_some(),
        attrs: field
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
    }
}

fn collect_nested_record_keeps(
    txn_name: &str,
    items: &[TxnBodyItem],
    enum_domains: &BTreeMap<String, EnumDomainSchema>,
    record_bodies: &BTreeMap<String, Vec<TxnBodyItem>>,
    record_fields: &BTreeMap<String, Vec<Field>>,
    errors: &mut Vec<ElaborationError>,
) -> Vec<ConstraintClause> {
    let mut keeps = Vec::new();
    for field in direct_fields(items) {
        let Some(record_name) = record_type_name(&field.ty) else {
            continue;
        };
        if enum_domains.contains_key(record_name) {
            continue;
        }
        let Some(child_body) = record_bodies.get(record_name) else {
            continue;
        };
        let child_keeps = collect_direct_and_nested_record_keeps(
            record_name,
            child_body,
            enum_domains,
            record_bodies,
            record_fields,
            errors,
        );
        if child_keeps.is_empty() {
            continue;
        }
        let child_field_names: std::collections::BTreeSet<String> = record_fields
            .get(record_name)
            .into_iter()
            .flat_map(|fields| fields.iter().map(|field| field.name.name.clone()))
            .collect();
        let prefix = Expr::new(ExprKind::Ident(field.name.clone()), field.name.span);
        for keep in child_keeps {
            let expr = prefix_record_keep_expr(&keep.expr, &prefix, &child_field_names);
            keeps.push(ConstraintClause {
                origin: ConstraintOrigin::TransactionKeep {
                    transaction: txn_name.to_string(),
                    span: expr.span,
                },
                ir: lower_constraint_expr(&expr, errors),
                refs: ConstraintRefs::default(),
                expr,
            });
        }
    }
    keeps
}

fn collect_direct_and_nested_record_keeps(
    record_name: &str,
    items: &[TxnBodyItem],
    enum_domains: &BTreeMap<String, EnumDomainSchema>,
    record_bodies: &BTreeMap<String, Vec<TxnBodyItem>>,
    record_fields: &BTreeMap<String, Vec<Field>>,
    errors: &mut Vec<ElaborationError>,
) -> Vec<ConstraintClause> {
    let mut keeps = Vec::new();
    for item in items {
        if let TxnBodyItem::Keep(k) = item {
            keeps.push(ConstraintClause {
                origin: ConstraintOrigin::TransactionKeep {
                    transaction: record_name.to_string(),
                    span: k.span,
                },
                expr: k.expr.clone(),
                ir: lower_constraint_expr(&k.expr, errors),
                refs: ConstraintRefs::default(),
            });
        }
    }
    keeps.extend(collect_nested_record_keeps(
        record_name,
        items,
        enum_domains,
        record_bodies,
        record_fields,
        errors,
    ));
    keeps
}

fn record_type_name(ty: &TypeExpr) -> Option<&str> {
    let TypeExpr::Named { name, .. } = ty else {
        return None;
    };
    name.segments.last().map(|segment| segment.name.as_str())
}

fn prefix_record_keep_expr(
    expr: &Expr,
    prefix: &Expr,
    field_names: &std::collections::BTreeSet<String>,
) -> Expr {
    let kind = match &*expr.kind {
        ExprKind::Ident(id) if field_names.contains(&id.name) => ExprKind::Field {
            target: prefix.clone(),
            name: id.clone(),
        },
        ExprKind::Field { target, name } => ExprKind::Field {
            target: prefix_record_keep_expr(target, prefix, field_names),
            name: name.clone(),
        },
        ExprKind::Index { target, index } => ExprKind::Index {
            target: prefix_record_keep_expr(target, prefix, field_names),
            index: prefix_record_keep_expr(index, prefix, field_names),
        },
        ExprKind::BitSlice { target, hi, lo } => ExprKind::BitSlice {
            target: prefix_record_keep_expr(target, prefix, field_names),
            hi: prefix_record_keep_expr(hi, prefix, field_names),
            lo: prefix_record_keep_expr(lo, prefix, field_names),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: prefix_record_keep_expr(callee, prefix, field_names),
            args: args
                .iter()
                .map(|arg| match arg {
                    CallArg::Expr(e) => {
                        CallArg::Expr(prefix_record_keep_expr(e, prefix, field_names))
                    }
                    CallArg::Named { name, value } => CallArg::Named {
                        name: name.clone(),
                        value: prefix_record_keep_expr(value, prefix, field_names),
                    },
                })
                .collect(),
        },
        ExprKind::ForEachConstraint { var, iter, body } => {
            let mut inner_fields = field_names.clone();
            inner_fields.remove(&var.name);
            ExprKind::ForEachConstraint {
                var: var.clone(),
                iter: prefix_record_keep_expr(iter, prefix, field_names),
                body: body
                    .iter()
                    .map(|e| prefix_record_keep_expr(e, prefix, &inner_fields))
                    .collect(),
            }
        }
        ExprKind::Cast { expr, ty } => ExprKind::Cast {
            expr: prefix_record_keep_expr(expr, prefix, field_names),
            ty: ty.clone(),
        },
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op: *op,
            expr: prefix_record_keep_expr(expr, prefix, field_names),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op: *op,
            lhs: prefix_record_keep_expr(lhs, prefix, field_names),
            rhs: prefix_record_keep_expr(rhs, prefix, field_names),
        },
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::Ternary {
            cond: prefix_record_keep_expr(cond, prefix, field_names),
            then_branch: prefix_record_keep_expr(then_branch, prefix, field_names),
            else_branch: prefix_record_keep_expr(else_branch, prefix, field_names),
        },
        ExprKind::Paren(inner) => {
            ExprKind::Paren(prefix_record_keep_expr(inner, prefix, field_names))
        }
        ExprKind::Membership { expr, set } => ExprKind::Membership {
            expr: prefix_record_keep_expr(expr, prefix, field_names),
            set: prefix_record_keep_expr(set, prefix, field_names),
        },
        ExprKind::SetLit(items) => ExprKind::SetLit(
            items
                .iter()
                .map(|e| prefix_record_keep_expr(e, prefix, field_names))
                .collect(),
        ),
        ExprKind::RangeLit { lo, hi } => ExprKind::RangeLit {
            lo: lo
                .as_ref()
                .map(|e| prefix_record_keep_expr(e, prefix, field_names)),
            hi: hi
                .as_ref()
                .map(|e| prefix_record_keep_expr(e, prefix, field_names)),
        },
        _ => return expr.clone(),
    };
    Expr::new(kind, expr.span)
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
                type_name: None,
                width: type_arg_width(args),
                signedness: Signedness::Unsigned,
                enum_domain: None,
            },
            BuiltinTy::SInt | BuiltinTy::SIntCap => FieldTypeSchema {
                class: FieldTypeClass::SInt,
                type_name: None,
                width: type_arg_width(args),
                signedness: Signedness::Signed,
                enum_domain: None,
            },
            BuiltinTy::Bits => FieldTypeSchema {
                class: FieldTypeClass::Bits,
                type_name: None,
                width: type_arg_width(args),
                signedness: Signedness::Unsigned,
                enum_domain: None,
            },
            BuiltinTy::Bool | BuiltinTy::BoolLower => FieldTypeSchema {
                class: FieldTypeClass::Bool,
                type_name: None,
                width: Some(1),
                signedness: Signedness::NotNumeric,
                enum_domain: None,
            },
            BuiltinTy::Bit => FieldTypeSchema {
                class: FieldTypeClass::Bit,
                type_name: None,
                width: Some(1),
                signedness: Signedness::Unsigned,
                enum_domain: None,
            },
            BuiltinTy::Int => FieldTypeSchema {
                class: FieldTypeClass::Int,
                type_name: None,
                width: Some(32),
                signedness: Signedness::Signed,
                enum_domain: None,
            },
            other => FieldTypeSchema {
                class: FieldTypeClass::UnsupportedBuiltin(format!("{other:?}")),
                type_name: None,
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
                    type_name: Some(type_name),
                    width: enum_width(domain.variants.len()),
                    signedness: Signedness::Unsigned,
                    enum_domain: Some(domain.clone()),
                }
            } else {
                FieldTypeSchema {
                    class: FieldTypeClass::Named,
                    type_name: Some(type_name),
                    width: None,
                    signedness: Signedness::Unknown,
                    enum_domain: None,
                }
            }
        }
    }
}

fn validate_constraint_refs(
    structs: &mut [TxnSchema],
    transactions: &mut [TxnSchema],
    relations: &mut [RelationSchema],
    enum_variants: &BTreeMap<String, EnumVariantSchema>,
    errors: &mut Vec<ElaborationError>,
) {
    let record_index: BTreeMap<String, TxnSchema> = structs
        .iter()
        .chain(transactions.iter())
        .map(|record| (record.name.clone(), record.clone()))
        .collect();
    let relation_names: Vec<String> = relations.iter().map(|r| r.name.clone()).collect();

    for record in structs {
        let ctx = RefValidationCtx {
            transaction: record_index.get(&record.name),
            params: BTreeMap::new(),
            records: &record_index,
            relation_names: &relation_names,
            enum_variants,
        };
        for keep in &mut record.keeps {
            validate_clause_refs(keep, &ctx, errors);
        }
        validate_when_refs(&mut record.when_subtypes, &ctx, errors);
    }

    for txn in transactions {
        let ctx = RefValidationCtx {
            transaction: record_index.get(&txn.name),
            params: BTreeMap::new(),
            records: &record_index,
            relation_names: &relation_names,
            enum_variants,
        };
        for keep in &mut txn.keeps {
            validate_clause_refs(keep, &ctx, errors);
        }
        validate_when_refs(&mut txn.when_subtypes, &ctx, errors);
    }

    for relation in relations {
        let params = relation
            .params
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();
        let ctx = RefValidationCtx {
            transaction: None,
            params,
            records: &record_index,
            relation_names: &relation_names,
            enum_variants,
        };
        match &mut relation.body {
            RelationBodySchema::Block(clauses) => {
                for clause in clauses {
                    validate_clause_refs(clause, &ctx, errors);
                }
            }
            RelationBodySchema::Alias(clause) => validate_clause_refs(clause, &ctx, errors),
        }
    }
}

fn validate_when_refs(
    when_subtypes: &mut [WhenSubtypeSchema],
    ctx: &RefValidationCtx<'_>,
    errors: &mut Vec<ElaborationError>,
) {
    for subtype in when_subtypes {
        for keep in &mut subtype.keeps {
            validate_clause_refs(keep, ctx, errors);
        }
        validate_when_refs(&mut subtype.when_subtypes, ctx, errors);
    }
}

struct RefValidationCtx<'a> {
    transaction: Option<&'a TxnSchema>,
    params: BTreeMap<String, Option<FieldTypeSchema>>,
    records: &'a BTreeMap<String, TxnSchema>,
    relation_names: &'a [String],
    enum_variants: &'a BTreeMap<String, EnumVariantSchema>,
}

fn validate_clause_refs(
    clause: &mut ConstraintClause,
    ctx: &RefValidationCtx<'_>,
    errors: &mut Vec<ElaborationError>,
) {
    let Some(ir) = &clause.ir else {
        return;
    };
    let mut refs = ConstraintRefs::default();
    collect_constraint_refs(ir, clause.expr.span, ctx, &mut refs, errors);
    clause.refs = refs;
}

fn collect_constraint_refs(
    ir: &ConstraintExpr,
    span: Span,
    ctx: &RefValidationCtx<'_>,
    refs: &mut ConstraintRefs,
    errors: &mut Vec<ElaborationError>,
) {
    let locals = BTreeMap::new();
    collect_constraint_refs_with_locals(ir, span, ctx, &locals, refs, errors);
}

fn collect_constraint_refs_with_locals(
    ir: &ConstraintExpr,
    span: Span,
    ctx: &RefValidationCtx<'_>,
    locals: &BTreeMap<String, ()>,
    refs: &mut ConstraintRefs,
    errors: &mut Vec<ElaborationError>,
) {
    match ir {
        ConstraintExpr::Ident(name) => {
            if locals.contains_key(name) {
                return;
            }
            if ctx.params.contains_key(name) {
                return;
            }
            if let Some(field) = ctx.transaction.and_then(|txn| find_txn_field(txn, name)) {
                refs.fields.push(ConstraintFieldRef {
                    root: None,
                    field: name.clone(),
                    ty: field.ty.clone(),
                    non_random: field.non_random,
                });
            } else if let Some(variant) = ctx.enum_variants.get(name) {
                refs.enum_variants.push(variant.clone());
            } else {
                errors.push(ElaborationError {
                    span,
                    message: format!("constraint references unknown name `{name}`"),
                });
            }
        }
        ConstraintExpr::FieldRef { root, field } => {
            if locals.contains_key(root) {
                return;
            }
            if let Some(param_ty) = ctx.params.get(root) {
                let Some(type_name) = param_ty.as_ref().and_then(|ty| ty.type_name.as_deref())
                else {
                    errors.push(ElaborationError {
                        span,
                        message: format!(
                            "constraint parameter `{root}` has no transaction type for field `{field}`"
                        ),
                    });
                    return;
                };
                if let Some(txn) = ctx.records.get(type_name) {
                    if let Some(field_schema) = find_txn_field(txn, field) {
                        refs.fields.push(ConstraintFieldRef {
                            root: Some(root.clone()),
                            field: field.clone(),
                            ty: field_schema.ty.clone(),
                            non_random: field_schema.non_random,
                        });
                    } else {
                        errors.push(ElaborationError {
                            span,
                            message: format!(
                                "constraint references unknown field `{field}` on `{type_name}`"
                            ),
                        });
                    }
                } else {
                    errors.push(ElaborationError {
                        span,
                        message: format!(
                            "constraint parameter `{root}` references unknown transaction `{type_name}`"
                        ),
                    });
                }
            } else if let Some(txn) = ctx.transaction {
                let nested_field = format!("{root}.{field}");
                if let Some(field_schema) = find_txn_field(txn, &nested_field) {
                    refs.fields.push(ConstraintFieldRef {
                        root: Some(root.clone()),
                        field: nested_field,
                        ty: field_schema.ty.clone(),
                        non_random: field_schema.non_random,
                    });
                } else if let Some(field_schema) = find_txn_field(txn, field) {
                    refs.fields.push(ConstraintFieldRef {
                        root: Some(root.clone()),
                        field: field.clone(),
                        ty: field_schema.ty.clone(),
                        non_random: field_schema.non_random,
                    });
                } else {
                    errors.push(ElaborationError {
                        span,
                        message: format!("constraint references unknown field `{field}`"),
                    });
                }
            } else {
                errors.push(ElaborationError {
                    span,
                    message: format!("constraint references unknown parameter `{root}`"),
                });
            }
        }
        ConstraintExpr::Unary { expr, .. } => {
            collect_constraint_refs_with_locals(expr, span, ctx, locals, refs, errors)
        }
        ConstraintExpr::Binary { lhs, rhs, .. } => {
            collect_constraint_refs_with_locals(lhs, span, ctx, locals, refs, errors);
            collect_constraint_refs_with_locals(rhs, span, ctx, locals, refs, errors);
        }
        ConstraintExpr::Membership { expr, set } => {
            collect_constraint_refs_with_locals(expr, span, ctx, locals, refs, errors);
            collect_constraint_refs_with_locals(set, span, ctx, locals, refs, errors);
        }
        ConstraintExpr::Range { lo, hi } => {
            if let Some(lo) = lo {
                collect_constraint_refs_with_locals(lo, span, ctx, locals, refs, errors);
            }
            if let Some(hi) = hi {
                collect_constraint_refs_with_locals(hi, span, ctx, locals, refs, errors);
            }
        }
        ConstraintExpr::Set(items) => {
            for item in items {
                collect_constraint_refs_with_locals(item, span, ctx, locals, refs, errors);
            }
        }
        ConstraintExpr::RelationCall { name, args } => {
            if ctx.relation_names.iter().any(|relation| relation == name) {
                refs.relation_calls.push(name.clone());
            } else {
                errors.push(ElaborationError {
                    span,
                    message: format!("constraint calls unknown relation `{name}`"),
                });
            }
            for arg in args {
                collect_constraint_refs_with_locals(arg, span, ctx, locals, refs, errors);
            }
        }
        ConstraintExpr::FieldMethodCall { target, args, .. } => {
            collect_constraint_refs_with_locals(target, span, ctx, locals, refs, errors);
            for arg in args {
                collect_constraint_refs_with_locals(arg, span, ctx, locals, refs, errors);
            }
        }
        ConstraintExpr::ForEach { var, iter, body } => {
            collect_constraint_refs_with_locals(iter, span, ctx, locals, refs, errors);
            let mut inner_locals = locals.clone();
            inner_locals.insert(var.clone(), ());
            for clause in body {
                collect_constraint_refs_with_locals(clause, span, ctx, &inner_locals, refs, errors);
            }
        }
        ConstraintExpr::IntLiteral(_) | ConstraintExpr::BoolLiteral(_) => {}
    }
}

fn find_txn_field<'a>(txn: &'a TxnSchema, name: &str) -> Option<&'a TxnFieldSchema> {
    txn.fields
        .iter()
        .chain(txn.when_subtypes.iter().flat_map(|subtype| {
            subtype
                .fields
                .iter()
                .chain(subtype.when_subtypes.iter().flat_map(flatten_when_fields))
        }))
        .find(|field| field.name == name)
}

fn flatten_when_fields(
    subtype: &WhenSubtypeSchema,
) -> Box<dyn Iterator<Item = &TxnFieldSchema> + '_> {
    Box::new(
        subtype
            .fields
            .iter()
            .chain(subtype.when_subtypes.iter().flat_map(flatten_when_fields)),
    )
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
                ConstraintExpr::FieldRef { root, field } => Ok(ConstraintExpr::FieldRef {
                    root,
                    field: format!("{field}.{}", name.name),
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
            let lowered_args = args
                .iter()
                .map(|arg| match arg {
                    CallArg::Expr(e) => lower_constraint_expr_inner(e),
                    CallArg::Named { value, .. } => lower_constraint_expr_inner(value),
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let ExprKind::Field { target, name } = &*callee.kind {
                return Ok(ConstraintExpr::FieldMethodCall {
                    target: Box::new(lower_constraint_expr_inner(target)?),
                    method: name.name.clone(),
                    args: lowered_args,
                });
            }
            let ExprKind::Ident(id) = &*callee.kind else {
                return Err("constraint call callee must be a relation name".into());
            };
            Ok(ConstraintExpr::RelationCall {
                name: id.name.clone(),
                args: lowered_args,
            })
        }
        ExprKind::ForEachConstraint { var, iter, body } => Ok(ConstraintExpr::ForEach {
            var: var.name.clone(),
            iter: Box::new(lower_constraint_expr_inner(iter)?),
            body: body
                .iter()
                .map(lower_constraint_expr_inner)
                .collect::<Result<Vec<_>, _>>()?,
        }),
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
