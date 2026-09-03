//! Typed constraint IR.
//!
//! See `docs/constraint-ir-design.md` for the full design.  This module
//! defines the IR types — Phase 1 of the migration plan.  Lowering from
//! AST (`typed_lower.rs`, Phase 2) and the solver backend (`src/solver`,
//! Phase 4) live in separate modules and consume what is defined here.
//!
//! Phase 1 is intentionally self-contained: no production code path
//! constructs `CTypedProblem`s yet.  This module provides the type
//! surface plus `Display` impls + hand-built unit tests so the IR can
//! be reviewed independently.

use std::collections::BTreeMap;
use std::fmt;

use crate::constraints::{ConstraintOrigin, EnumVariantSchema, FieldAttrSchema, RelationSchema};
use crate::lexer::Span;

// ─── ID newtypes ────────────────────────────────────────────────────

/// Stable handle for a constraint problem.  Assigned at compile time
/// per `(TxnSchema, randomize-with site)` pair; the runtime layer uses
/// it to key into the problem table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConstraintProblemId(pub u64);

/// Handle into `FieldEnv::enums`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnumDomainId(pub u32);

/// Handle into `FieldEnv::relations`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelationId(pub u32);

/// Loop-bound variable inside a `ForAll`.  Scoped to one
/// `CTypedProblem`; not a TB-IR `LocalId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalId(pub u32);

// ─── Field paths ────────────────────────────────────────────────────

/// Field path (e.g. `["hdr", "addr"]` for `hdr.addr`).  Canonical
/// dotted-path representation used by both the IR and the solver
/// backend.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldPath(pub Vec<String>);

impl FieldPath {
    pub fn single(name: impl Into<String>) -> Self {
        Self(vec![name.into()])
    }

    pub fn of(root: impl Into<String>, field: impl Into<String>) -> Self {
        Self(vec![root.into(), field.into()])
    }

    pub fn dotted(&self) -> String {
        self.0.join(".")
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dotted())
    }
}

// ─── Types ──────────────────────────────────────────────────────────

/// Signedness in the typed IR.
///
/// Distinct from `constraints::Signedness` (which has `NotNumeric` /
/// `Unknown` for pre-typed states) — every `CType::BV` carries one of
/// exactly two variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Sign {
    Unsigned,
    Signed,
}

impl fmt::Display for Sign {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sign::Unsigned => f.write_str("u"),
            Sign::Signed => f.write_str("s"),
        }
    }
}

/// Resolved type carried on every `CTypedExpr`.
///
/// `Bottom` may appear transiently during lowering but is rejected by
/// `verify` before the IR reaches any solver backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CType {
    /// Covers `uint<N>`, `sint<N>`, `bits<N>`, `bit`.
    BV {
        width: u32,
        sign: Sign,
    },
    Bool,
    Enum {
        domain: EnumDomainId,
    },
    /// Value-typed range (e.g. the rhs of `x in [lo .. hi]`).
    Range {
        elem: Box<CType>,
    },
    /// Bag of values of homogeneous type (rhs of `x in [a, b, c]`).
    Set {
        elem: Box<CType>,
    },
    /// Aggregate list field. The optional max length is source metadata
    /// used by later solver lowering; `len()` currently returns `u32`.
    List {
        elem: Box<CType>,
        max_len: Option<usize>,
    },
    /// Type-error marker.  Verifier rejects any problem containing
    /// `Bottom`.
    Bottom,
}

impl CType {
    pub fn bv(width: u32, sign: Sign) -> Self {
        CType::BV { width, sign }
    }

    pub fn uint(width: u32) -> Self {
        CType::BV {
            width,
            sign: Sign::Unsigned,
        }
    }

    pub fn sint(width: u32) -> Self {
        CType::BV {
            width,
            sign: Sign::Signed,
        }
    }

    pub fn enum_(domain: EnumDomainId) -> Self {
        CType::Enum { domain }
    }

    pub fn is_bv(&self) -> bool {
        matches!(self, CType::BV { .. })
    }

    pub fn is_bool(&self) -> bool {
        matches!(self, CType::Bool)
    }

    pub fn is_bottom(&self) -> bool {
        matches!(self, CType::Bottom)
    }

    /// Returns `Some((width, sign))` if `self` is a `BV`.
    pub fn as_bv(&self) -> Option<(u32, Sign)> {
        match self {
            CType::BV { width, sign } => Some((*width, *sign)),
            _ => None,
        }
    }
}

impl fmt::Display for CType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CType::BV { width, sign } => write!(f, "{sign}{width}"),
            CType::Bool => f.write_str("bool"),
            CType::Enum { domain } => write!(f, "enum#{}", domain.0),
            CType::Range { elem } => write!(f, "range<{elem}>"),
            CType::Set { elem } => write!(f, "set<{elem}>"),
            CType::List { elem, max_len } => match max_len {
                Some(n) => write!(f, "list<{elem}, max={n}>"),
                None => write!(f, "list<{elem}>"),
            },
            CType::Bottom => f.write_str("⊥"),
        }
    }
}

// ─── Operators ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CUnaryOp {
    Neg,
    LogicalNot,
    BitNot,
}

impl fmt::Display for CUnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CUnaryOp::Neg => f.write_str("-"),
            CUnaryOp::LogicalNot => f.write_str("!"),
            CUnaryOp::BitNot => f.write_str("~"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CBinaryOp {
    // Arithmetic on BV.
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    // Comparison on BV → Bool.  Signed/unsigned dispatch happens in
    // the solver backend based on operand types.
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    // Logical on Bool × Bool → Bool.
    LogicalAnd,
    LogicalOr,
    // Bitwise on BV × BV → BV.
    BitAnd,
    BitOr,
    BitXor,
    // Shifts: BV(w, s) × BV(w', Unsigned) → BV(w, s).
    Shl,
    Shr,
}

impl fmt::Display for CBinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CBinaryOp::Add => "+",
            CBinaryOp::Sub => "-",
            CBinaryOp::Mul => "*",
            CBinaryOp::Div => "/",
            CBinaryOp::Mod => "%",
            CBinaryOp::Eq => "==",
            CBinaryOp::Ne => "!=",
            CBinaryOp::Lt => "<",
            CBinaryOp::Le => "<=",
            CBinaryOp::Gt => ">",
            CBinaryOp::Ge => ">=",
            CBinaryOp::LogicalAnd => "&&",
            CBinaryOp::LogicalOr => "||",
            CBinaryOp::BitAnd => "&",
            CBinaryOp::BitOr => "|",
            CBinaryOp::BitXor => "^",
            CBinaryOp::Shl => "<<",
            CBinaryOp::Shr => ">>",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinMethod {
    /// `aggregate.len()` — currently used for list-valued fields.
    Len,
}

impl fmt::Display for BuiltinMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuiltinMethod::Len => f.write_str("len"),
        }
    }
}

// ─── Expression IR ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CTypedExpr {
    pub kind: CExprKind,
    pub ty: CType,
    pub span: Span,
}

impl CTypedExpr {
    pub fn new(kind: CExprKind, ty: CType, span: Span) -> Self {
        Self { kind, ty, span }
    }

    /// Convenience: walk children for verifier / pretty-printing.
    pub fn children(&self) -> Vec<&CTypedExpr> {
        match &self.kind {
            CExprKind::BvLit { .. }
            | CExprKind::BoolLit(_)
            | CExprKind::EnumLit { .. }
            | CExprKind::FieldRef(_)
            | CExprKind::LocalRef(_) => vec![],
            CExprKind::Unary { expr, .. } => vec![expr.as_ref()],
            CExprKind::Binary { lhs, rhs, .. } => vec![lhs.as_ref(), rhs.as_ref()],
            CExprKind::InSet { expr, set } => vec![expr.as_ref(), set.as_ref()],
            CExprKind::InRange { expr, lo, hi } => {
                let mut v = vec![expr.as_ref()];
                if let Some(lo) = lo {
                    v.push(lo.as_ref());
                }
                if let Some(hi) = hi {
                    v.push(hi.as_ref());
                }
                v
            }
            CExprKind::Set(items) => items.iter().collect(),
            CExprKind::Range { lo, hi } => {
                let mut v = Vec::new();
                if let Some(lo) = lo {
                    v.push(lo.as_ref());
                }
                if let Some(hi) = hi {
                    v.push(hi.as_ref());
                }
                v
            }
            CExprKind::FieldMethodCall { target, args, .. } => {
                let mut v = vec![target.as_ref()];
                v.extend(args.iter());
                v
            }
            CExprKind::ForAll { iter, body, .. } => vec![iter.as_ref(), body.as_ref()],
        }
    }
}

impl fmt::Display for CTypedExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Compact `expr:type` form; suitable for diffs and golden files.
        write!(f, "{}:{}", self.kind, self.ty)
    }
}

/// IR expression node.
///
/// `RelationApply` is intentionally absent: relation calls are
/// monomorphised during lowering, so by the time a `CTypedExpr` exists
/// in a `CTypedProblem` there are no unexpanded relations.  The
/// verifier rejects any leftover relation references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CExprKind {
    /// Numeric literal with width inferred at lowering.  Value is held
    /// as `u128`; the `ty` field carries width + sign.
    BvLit {
        value: u128,
    },
    BoolLit(bool),
    EnumLit {
        domain: EnumDomainId,
        variant_idx: u32,
    },
    FieldRef(FieldPath),
    LocalRef(LocalId),
    Unary {
        op: CUnaryOp,
        expr: Box<CTypedExpr>,
    },
    Binary {
        op: CBinaryOp,
        lhs: Box<CTypedExpr>,
        rhs: Box<CTypedExpr>,
    },
    InSet {
        expr: Box<CTypedExpr>,
        set: Box<CTypedExpr>, // ty: Set{elem}
    },
    InRange {
        expr: Box<CTypedExpr>,
        lo: Option<Box<CTypedExpr>>,
        hi: Option<Box<CTypedExpr>>,
    },
    Set(Vec<CTypedExpr>),
    Range {
        lo: Option<Box<CTypedExpr>>,
        hi: Option<Box<CTypedExpr>>,
    },
    FieldMethodCall {
        target: Box<CTypedExpr>,
        method: BuiltinMethod,
        args: Vec<CTypedExpr>,
    },
    /// Universally-quantified body — only valid as a top-level clause.
    /// `iter` must be a statically-bounded range or `.len()`-bounded
    /// aggregate; the lowering pass enforces this.
    ForAll {
        var: LocalId,
        iter: Box<CTypedExpr>,
        body: Box<CTypedExpr>,
    },
}

impl fmt::Display for CExprKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CExprKind::BvLit { value } => write!(f, "{value}"),
            CExprKind::BoolLit(b) => write!(f, "{b}"),
            CExprKind::EnumLit {
                domain,
                variant_idx,
            } => {
                write!(f, "enum#{}.{variant_idx}", domain.0)
            }
            CExprKind::FieldRef(p) => write!(f, "{p}"),
            CExprKind::LocalRef(l) => write!(f, "%{}", l.0),
            CExprKind::Unary { op, expr } => write!(f, "({op}{expr})"),
            CExprKind::Binary { op, lhs, rhs } => write!(f, "({lhs} {op} {rhs})"),
            CExprKind::InSet { expr, set } => write!(f, "({expr} in {set})"),
            CExprKind::InRange { expr, lo, hi } => {
                f.write_str("(")?;
                expr.fmt(f)?;
                f.write_str(" in [")?;
                if let Some(lo) = lo {
                    write!(f, "{lo}")?;
                }
                f.write_str("..")?;
                if let Some(hi) = hi {
                    write!(f, "{hi}")?;
                }
                f.write_str("])")
            }
            CExprKind::Set(items) => {
                f.write_str("{")?;
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{it}")?;
                }
                f.write_str("}")
            }
            CExprKind::Range { lo, hi } => {
                f.write_str("[")?;
                if let Some(lo) = lo {
                    write!(f, "{lo}")?;
                }
                f.write_str("..")?;
                if let Some(hi) = hi {
                    write!(f, "{hi}")?;
                }
                f.write_str("]")
            }
            CExprKind::FieldMethodCall {
                target,
                method,
                args,
            } => {
                write!(f, "{target}.{method}(")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(")")
            }
            CExprKind::ForAll { var, iter, body } => {
                write!(f, "(forall %{} in {iter}: {body})", var.0)
            }
        }
    }
}

// ─── Clauses and problems ───────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CTypedClause {
    pub origin: ConstraintOrigin,
    /// Must be `CType::Bool`.  Verifier enforces.
    pub expr: CTypedExpr,
    /// Stable name for Z3 `(! _ :named …)` so UNSAT cores map back to
    /// `ConstraintOrigin`.
    pub assertion_name: String,
}

impl fmt::Display for CTypedClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {} := {}",
            self.assertion_name,
            origin_short(&self.origin),
            self.expr
        )
    }
}

fn origin_short(o: &ConstraintOrigin) -> String {
    match o {
        ConstraintOrigin::TransactionKeep { transaction, .. } => {
            format!("keep@{transaction}")
        }
        ConstraintOrigin::RandomizeWith { .. } => "randomize_with".to_string(),
        ConstraintOrigin::RandomizeSoft { weight, .. } => format!("soft@w{weight}"),
        ConstraintOrigin::RelationExpansion { relation, .. } => format!("rel@{relation}"),
        ConstraintOrigin::FieldAttribute { field, attr, .. } => format!("attr[{attr}]@{field}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CTypedSoftClause {
    pub origin: ConstraintOrigin,
    /// Must be `CType::Bool`.  Verifier enforces.
    pub expr: CTypedExpr,
    pub assertion_name: String,
    pub weight: u32,
}

impl fmt::Display for CTypedSoftClause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] soft weight {} {} := {}",
            self.assertion_name,
            self.weight,
            origin_short(&self.origin),
            self.expr
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProblemOrigin {
    /// `randomize(t) with ...` call site.
    RandomizeSite { record: String, span: Span },
    /// Synthesized for transaction-bare `randomize(t)` (no `with` block).
    BareRandomize { record: String, span: Span },
}

impl fmt::Display for ProblemOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProblemOrigin::RandomizeSite { record, .. } => write!(f, "randomize({record}) with"),
            ProblemOrigin::BareRandomize { record, .. } => write!(f, "randomize({record})"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub ty: CType,
    pub non_random: bool,
    pub has_default: bool,
    pub attrs: Vec<FieldAttrSchema>,
}

#[derive(Debug, Clone, Default)]
pub struct FieldEnv {
    pub fields: BTreeMap<FieldPath, FieldInfo>,
    /// Indexed by `EnumDomainId.0`.  Cloned from the layer-1 schema at
    /// problem construction so the solver does not need to reach back
    /// into the elaboration.
    pub enums: Vec<EnumDomainEntry>,
    /// Indexed by `RelationId.0`.  Present for diagnostics only;
    /// relations are inlined by the time a `CTypedProblem` is verified.
    pub relations: Vec<RelationSchema>,
}

#[derive(Debug, Clone)]
pub struct EnumDomainEntry {
    pub id: EnumDomainId,
    pub name: String,
    pub variants: Vec<EnumVariantSchema>,
}

impl FieldEnv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lookup(&self, path: &FieldPath) -> Option<&FieldInfo> {
        self.fields.get(path)
    }

    pub fn enum_by_id(&self, id: EnumDomainId) -> Option<&EnumDomainEntry> {
        self.enums.iter().find(|e| e.id == id)
    }
}

#[derive(Debug, Clone)]
pub struct CTypedProblem {
    pub problem_id: ConstraintProblemId,
    pub origin: ProblemOrigin,
    pub env: FieldEnv,
    pub constraints: Vec<CTypedClause>,
    pub soft_constraints: Vec<CTypedSoftClause>,
    /// `solve_order(a, b, c)` — fields are fixed in this order during
    /// solving; ordinary unconstrained fields sample after the last
    /// fixed field.  `None` = no user-specified order; runtime picks.
    pub solve_order: Option<Vec<FieldPath>>,
}

impl fmt::Display for CTypedProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "problem #{} {}", self.problem_id.0, self.origin)?;
        for (path, info) in &self.env.fields {
            let attr_tag = if info.non_random { " !" } else { "" };
            writeln!(f, "  field {path}: {}{attr_tag}", info.ty)?;
        }
        for c in &self.constraints {
            writeln!(f, "  clause {c}")?;
        }
        for c in &self.soft_constraints {
            writeln!(f, "  soft {c}")?;
        }
        if let Some(order) = &self.solve_order {
            let order_str: Vec<String> = order.iter().map(|p| p.dotted()).collect();
            writeln!(f, "  solve_order = [{}]", order_str.join(", "))?;
        }
        Ok(())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Span;

    fn sp() -> Span {
        // Minimal span; tests don't care about source positions.
        Span::default()
    }

    fn bvlit(value: u128, width: u32, sign: Sign) -> CTypedExpr {
        CTypedExpr::new(CExprKind::BvLit { value }, CType::BV { width, sign }, sp())
    }

    fn field(name: &str, ty: CType) -> CTypedExpr {
        CTypedExpr::new(CExprKind::FieldRef(FieldPath::single(name)), ty, sp())
    }

    #[test]
    fn ctype_constructors_and_predicates() {
        let u8t = CType::uint(8);
        let s32t = CType::sint(32);
        assert!(u8t.is_bv());
        assert!(!u8t.is_bool());
        assert_eq!(u8t.as_bv(), Some((8, Sign::Unsigned)));
        assert_eq!(s32t.as_bv(), Some((32, Sign::Signed)));
        assert!(CType::Bool.is_bool());
        assert!(CType::Bottom.is_bottom());
    }

    #[test]
    fn ctype_display_is_compact() {
        assert_eq!(format!("{}", CType::uint(8)), "u8");
        assert_eq!(format!("{}", CType::sint(32)), "s32");
        assert_eq!(format!("{}", CType::Bool), "bool");
        assert_eq!(
            format!(
                "{}",
                CType::Enum {
                    domain: EnumDomainId(3)
                }
            ),
            "enum#3"
        );
        assert_eq!(
            format!(
                "{}",
                CType::Set {
                    elem: Box::new(CType::uint(4))
                }
            ),
            "set<u4>"
        );
        assert_eq!(
            format!(
                "{}",
                CType::List {
                    elem: Box::new(CType::uint(8)),
                    max_len: Some(4)
                }
            ),
            "list<u8, max=4>"
        );
        assert_eq!(format!("{}", CType::Bottom), "⊥");
    }

    #[test]
    fn field_path_round_trip() {
        let p = FieldPath::of("hdr", "addr");
        assert_eq!(p.dotted(), "hdr.addr");
        assert_eq!(format!("{p}"), "hdr.addr");
        let single = FieldPath::single("len");
        assert_eq!(single.dotted(), "len");
    }

    #[test]
    fn expr_display_uses_typed_suffix() {
        let e = bvlit(42, 8, Sign::Unsigned);
        // Display form is `<expr>:<type>`.
        assert_eq!(format!("{e}"), "42:u8");
        let f = field("p.addr", CType::uint(8));
        assert_eq!(format!("{f}"), "p.addr:u8");
    }

    #[test]
    fn binary_expr_renders_inorder() {
        let lhs = field("p.addr", CType::uint(8));
        let rhs = bvlit(24, 8, Sign::Unsigned);
        let eq = CTypedExpr::new(
            CExprKind::Binary {
                op: CBinaryOp::Eq,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            CType::Bool,
            sp(),
        );
        assert_eq!(format!("{eq}"), "(p.addr:u8 == 24:u8):bool");
    }

    #[test]
    fn children_walks_subtree_in_order() {
        let lhs = field("a", CType::uint(8));
        let rhs = bvlit(7, 8, Sign::Unsigned);
        let parent = CTypedExpr::new(
            CExprKind::Binary {
                op: CBinaryOp::Lt,
                lhs: Box::new(lhs.clone()),
                rhs: Box::new(rhs.clone()),
            },
            CType::Bool,
            sp(),
        );
        let kids = parent.children();
        assert_eq!(kids.len(), 2);
        assert_eq!(format!("{}", kids[0]), format!("{lhs}"));
        assert_eq!(format!("{}", kids[1]), format!("{rhs}"));
    }

    #[test]
    fn problem_display_includes_fields_clauses_and_order() {
        let mut env = FieldEnv::new();
        env.fields.insert(
            FieldPath::of("p", "addr"),
            FieldInfo {
                ty: CType::uint(8),
                non_random: false,
                has_default: false,
                attrs: vec![],
            },
        );
        env.fields.insert(
            FieldPath::of("p", "value"),
            FieldInfo {
                ty: CType::uint(32),
                non_random: false,
                has_default: false,
                attrs: vec![],
            },
        );
        let lhs = field("p.addr", CType::uint(8));
        let rhs = bvlit(24, 8, Sign::Unsigned);
        let clause = CTypedClause {
            origin: ConstraintOrigin::RandomizeWith { span: sp() },
            expr: CTypedExpr::new(
                CExprKind::Binary {
                    op: CBinaryOp::Eq,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                CType::Bool,
                sp(),
            ),
            assertion_name: "c_p_addr_eq_24".into(),
        };
        let problem = CTypedProblem {
            problem_id: ConstraintProblemId(0),
            origin: ProblemOrigin::RandomizeSite {
                record: "RegPair".into(),
                span: sp(),
            },
            env,
            constraints: vec![clause],
            soft_constraints: vec![],
            solve_order: Some(vec![
                FieldPath::of("p", "addr"),
                FieldPath::of("p", "value"),
            ]),
        };

        let out = format!("{problem}");
        assert!(out.contains("problem #0"));
        assert!(out.contains("randomize(RegPair) with"));
        assert!(out.contains("field p.addr: u8"));
        assert!(out.contains("field p.value: u32"));
        assert!(out.contains("c_p_addr_eq_24"));
        assert!(out.contains("(p.addr:u8 == 24:u8):bool"));
        assert!(out.contains("solve_order = [p.addr, p.value]"));
    }

    #[test]
    fn id_ordering_is_deterministic() {
        let mut ids: Vec<EnumDomainId> = (0..5).rev().map(EnumDomainId).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                EnumDomainId(0),
                EnumDomainId(1),
                EnumDomainId(2),
                EnumDomainId(3),
                EnumDomainId(4)
            ]
        );
    }
}
