//! HARC AST.
//!
//! Coverage targets the constructs used in §11's worked AXI example plus the
//! broader Phase 1a surface (§14): packages, transactions with `keep` /
//! `when`, `extend` aspects, relations, tseq, env/agent/driver/monitor/
//! scoreboard, test/scope, covergroup, properties, and common statements.

use crate::lexer::Span;

// ── Identifiers ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// Dotted path: `tests.aspects.short_bursts`, `env.agent.monitor.txn`.
#[derive(Debug, Clone)]
pub struct Path {
    pub segments: Vec<Ident>,
    pub span: Span,
}

// ── Source file / items ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub items: Vec<Item>,
    pub inner_doc: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Use(UseDecl),
    Package(PackageDecl),
    Const(ConstDecl),
    Domain(DomainDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Transaction(TransactionDecl),
    Relation(RelationDecl),
    Tseq(TseqDecl),
    Agent(ComponentDecl),
    Driver(ComponentDecl),
    Monitor(ComponentDecl),
    Env(ComponentDecl),
    Scoreboard(ComponentDecl),
    Sequencer(ComponentDecl),
    Test(TestDecl),
    Extend(ExtendDecl),
    Covergroup(CovergroupDecl),
    Property(PropertyDecl),
    Pseq(PseqDecl),
    CoverSequence(CoverSequenceDecl),
    ExternalModule(ExternalModuleDecl),
    Function(FunctionDecl),
    Apply(ApplyDecl),
    /// `bus Name { signals + handshake_channels }` — protocol-typed
    /// bundle of DUT signals. v0 carries flat signals + named
    /// handshake_channel groupings; `credit_channel` and `tlm_method`
    /// are parser-recognized but currently no-ops at the codegen
    /// layer. Mirrors arch-com's §19 bus construct so HARC tests can
    /// `use BusAxiLite;` against arch-built DUTs.
    Bus(BusDecl),
    /// `transactor T bound to BusType { ... when active { ... } }` —
    /// synthesizable BFM unit (spec §8.1). Combines driver and
    /// monitor under one roof; the always-present body holds the
    /// observation half plus shared protocol state, the optional
    /// `when active` block holds the stimulus half. Mode subtyping
    /// at instantiation (`let xact : T active = bind axil`) selects
    /// which body is synthesized — `passive` instances literally do
    /// not include the active block in the lowered ARCH module
    /// (`generate_if ACTIVE`).
    ///
    /// Codegen scheduling: AST + parser + pretty-print land in T-1;
    /// SW-side codegen targeting the SCE-MI pipe surface (with
    /// in-process `std::deque` transport) is T-2; ARCH-side
    /// `generate_if ACTIVE` lowering is T-3; emulator transport is
    /// out-of-v0.
    Transactor(TransactorDecl),
}

// ── Use / Package ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UseDecl {
    pub path: Path,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PackageDecl {
    pub name: Ident,
    pub items: Vec<Item>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConstDecl {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub value: Expr,
    pub span: Span,
    pub doc: Option<String>,
}

// ── Domain (shared with ARCH) ─────────────────────────────────────────────────

/// `domain SysDomain freq_mhz: 100 end domain SysDomain` — ARCH-shape clock-
/// domain declaration. HARC parses the same syntax so the period of a TB-
/// driven `clock <name> = <DomainName>` can be derived from `freq_mhz`. The
/// declaration is syntactically permissive (any `key: value` pairs); only
/// `freq_mhz` is interpreted today.
#[derive(Debug, Clone)]
pub struct DomainDecl {
    pub name: Ident,
    pub fields: Vec<DomainField>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DomainField {
    pub name: Ident,
    pub value: Expr,
}

// ── Struct / Enum ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StructDecl {
    pub name: Ident,
    pub fields: Vec<Field>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub name: Ident,
    pub variants: Vec<Ident>,
    pub span: Span,
    pub doc: Option<String>,
}

// ── Transaction (§3.1, §3.3) ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TransactionDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub body: Vec<TxnBodyItem>,
    pub span: Span,
    pub doc: Option<String>,
}

/// Items that may appear inside a transaction body or an `extend` block.
#[derive(Debug, Clone)]
pub enum TxnBodyItem {
    Field(Field),
    Keep(Keep),
    When(WhenSubtype),
}

/// `[!] name : type [default <expr>] [with [attr]+]`
#[derive(Debug, Clone)]
pub struct Field {
    pub name: Ident,
    pub non_random: bool, // `!` prefix
    pub ty: TypeExpr,
    pub default: Option<Expr>,
    pub attrs: Vec<Attr>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Keep {
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct WhenSubtype {
    pub discriminant: Expr,
    pub items: Vec<TxnBodyItem>,
    pub span: Span,
}

/// `[name]` or `[name(args...)]` — see §3.1.
#[derive(Debug, Clone)]
pub struct Attr {
    pub name: Ident,
    pub args: Vec<AttrArg>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum AttrArg {
    Expr(Expr),
    /// `unique within tseq` — keyword-introduced clause inside an attribute.
    WithinScope(Ident),
    /// `dist {[0..0xFF] :/ 80, [0x100..] :/ 20}` — distribution literal.
    Dist(Vec<DistEntry>),
}

#[derive(Debug, Clone)]
pub struct DistEntry {
    pub value: Expr, // expression — set/range literal or a single value
    pub weight: Expr,
}

// ── Relation (§4) ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RelationDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    /// Either a body of constraint-expressions, or a single `= expr` form.
    pub body: RelationBody,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub enum RelationBody {
    Block(Vec<Expr>),
    Alias(Expr),
}

// ── Tseq (§8.4) ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TseqDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub return_ty: Option<TypeExpr>,
    pub body: Block,
    pub span: Span,
    pub doc: Option<String>,
}

// ── Component declarations (agent, driver, monitor, env, scoreboard, sequencer)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    Agent,
    Driver,
    Monitor,
    Env,
    Scoreboard,
    Sequencer,
}

impl ComponentKind {
    pub fn keyword(self) -> &'static str {
        match self {
            ComponentKind::Agent => "agent",
            ComponentKind::Driver => "driver",
            ComponentKind::Monitor => "monitor",
            ComponentKind::Env => "env",
            ComponentKind::Scoreboard => "scoreboard",
            ComponentKind::Sequencer => "sequencer",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComponentDecl {
    pub kind: ComponentKind,
    pub name: Ident,
    pub params: Vec<Param>,
    pub bound_to: Option<TypeExpr>,
    pub items: Vec<ComponentItem>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ComponentItem {
    Field(ComponentField),
    Connect(ConnectBlock),
    OnHandler(OnHandler),
    Hookable(HookableMethod),
    /// Inline `apply Name` inside a component body (rare but legal in scopes).
    Apply(ApplyDecl),
}

/// A field of a component — `name : Type` or `name : direction event<T>` etc.
#[derive(Debug, Clone)]
pub struct ComponentField {
    pub name: Ident,
    pub direction: Option<Direction>, // `in` / `out` for ports on driver/monitor
    pub ty: TypeExpr,
    /// `agent : AxiAgent bound to BusAxi4#(...)` — per-instance binding (§11).
    pub bound_to: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
    InOut,
}

#[derive(Debug, Clone)]
pub struct ConnectBlock {
    pub edges: Vec<ConnectEdge>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ConnectEdge {
    pub from: Expr,
    pub to: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct HookableMethod {
    pub name: Ident,
    pub params: Vec<Param>,
    /// Optional `-> Type` clause. `None` for void methods.
    pub return_ty: Option<TypeExpr>,
    pub body: Block,
    pub span: Span,
}

// ── Transactor (§8.1) ──────────────────────────────────────────────────────

/// Active/passive role of a transactor instance. Determined at the
/// instantiation site (`let xact : T active = bind axil`) — same
/// transactor source emits two distinct elaboration-time types,
/// differing only in whether the `when active` block is synthesized.
/// In ARCH lowering this drives `param ACTIVE: const = 0|1` plus
/// `generate_if ACTIVE` around the active body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactorMode {
    Active,
    Passive,
}

/// `transactor T#(generics) bound to BusType { ... when active { ... } end when }`
/// — synthesizable BFM. Parses + round-trips through `harc fmt` from
/// T-1; codegen lands in T-2 onward (see spec §8.1 for the staging).
#[derive(Debug, Clone)]
pub struct TransactorDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    /// `bound to BusType` clause — mandatory for transactors (a
    /// transactor without a bus binding would have nothing to drive
    /// or observe). Parser allows omission for forward-compat /
    /// better diagnostics; the type-checker rejects unbound
    /// transactors.
    pub bound_to: Option<TypeExpr>,
    /// Always-present body items: fields, observation handlers
    /// (`on bus.<ch>.handshake(...)`), shared hookables, and shared
    /// protocol state. Compiled in both active and passive modes.
    pub items: Vec<ComponentItem>,
    /// Optional `when active { ... } end when` body — the active-
    /// only stimulus half. Items inside are synthesized only when
    /// the instance is `active`. v1+ may add `when passive` for
    /// observe-only-augmenting bodies; for now `passive` = "active
    /// block omitted at this elaboration."
    pub when_active: Option<Vec<ComponentItem>>,
    pub span: Span,
    pub doc: Option<String>,
}

// ── Test and scope (§7.2) ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TestDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub items: Vec<TestItem>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub enum TestItem {
    Apply(ApplyDecl),
    Let(LetStmt),
    Scope(ScopeDecl),
    Use(UseDecl),
    /// `clock <name> = <period>` — declare a TB-driven clock generator.
    /// `<name>` matches a DUT input port (e.g. `clock wr_clk = 5ns` drives
    /// `dut.wr_clk` at 200 MHz). Period is a time literal. Multiple clocks
    /// in the same test trigger the multi-clock scheduler in codegen
    /// (spec §10.1). The first-declared clock is the primary (default
    /// for `wait N cycles`).
    Clock(ClockDecl),
    /// Bare statement in a test body — implicit `run` block (spec §7.2).
    /// Useful when a test doesn't need separate setup / check / teardown
    /// phases. The statements form an implicit `run` that lowers the same
    /// way an explicit `run { ... }` would.
    Stmt(Stmt),
}

#[derive(Debug, Clone)]
pub struct ClockDecl {
    pub name: Ident,
    pub period: Expr,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApplyDecl {
    pub path: Path,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ScopeDecl {
    pub name: Ident, // typically `sim`
    pub setup: Option<Block>,
    pub run: Option<Block>,
    pub check: Option<Block>,
    pub teardown: Option<Block>,
    pub span: Span,
}

// ── Extend aspect (§3.6) ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExtendDecl {
    pub target: Path,
    pub body: ExtendBody,
    pub span: Span,
    pub doc: Option<String>,
}

/// Body of an `extend` block. The shape depends on what's being extended.
/// Type-extensions add fields/keeps/whens; component-extensions add fields/
/// connects/handlers; test-extensions add scopes/lets/applies/uses. Per spec
/// §3.6 the depth-1 rule still holds — extends always target a base
/// declaration, never another extend.
#[derive(Debug, Clone)]
pub enum ExtendBody {
    /// `extend AxiTxn { keep ...; when ...; field ... }` — txn / struct.
    TxnLike(Vec<TxnBodyItem>),
    /// `extend AxiAgent { connect ...; on ...; field ... }` — agent / driver
    /// / monitor / env / scoreboard / sequencer.
    Component(Vec<ComponentItem>),
    /// `extend Smoke { scope sim ... }` — test (spec §7.2 + §3.6, allows
    /// per-backend scope splitting across files).
    Test(Vec<TestItem>),
}

// ── Coverage (§6) ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CovergroupDecl {
    pub name: Ident,
    pub clocking: Option<Expr>, // `@(posedge ...)` argument
    pub items: Vec<CoverItem>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CoverItem {
    Point(CoverPoint),
    Cross(CoverCross),
}

#[derive(Debug, Clone)]
pub struct CoverPoint {
    pub name: Ident,
    pub target: Expr,
    pub bins: Vec<CoverBin>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct CoverBin {
    pub name: Ident,
    pub spec: Expr, // set / range / scalar literal
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct CoverCross {
    pub points: Vec<Ident>,
    pub span: Span,
}

// ── Properties (§5) ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PropertyDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub body: Expr,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PseqDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub body: Expr,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CoverSequenceDecl {
    pub name: Ident,
    pub pattern: Expr,
    pub span: Span,
    pub doc: Option<String>,
}

// ── Bus (§19 — protocol-typed signal bundle) ──────────────────────────────────

/// `bus Name { ... }` — protocol-typed bundle of DUT signals. Mirrors
/// arch-com's §19. v0 carries:
/// - plain signals (`name: in|out Type`) — flat fields
/// - `handshake_channel` groupings — directionally-flipped valid/ready
///   plus payload signals; flatten to `<chan>_valid`, `<chan>_ready`,
///   `<chan>_<sig>` per spec §19.2.2
/// - parameter list (`param NAME: const = default`)
///
/// `credit_channel` and `tlm_method` blocks parse but don't yet take
/// part in HARC's signal-access lowering (they're scaffold-only —
/// covered by future PRs).
#[derive(Debug, Clone)]
pub struct BusDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub signals: Vec<BusSignal>,
    pub handshakes: Vec<HandshakeChannel>,
    pub span: Span,
    pub doc: Option<String>,
}

/// One plain signal inside a `bus` body. Direction is from the
/// initiator's perspective — the use-site `target Bus` keyword flips it.
#[derive(Debug, Clone)]
pub struct BusSignal {
    pub name: Ident,
    pub direction: Direction,
    pub ty: TypeExpr,
    pub span: Span,
}

/// `handshake_channel <name>: send|receive kind: valid_ready` — a
/// grouping of payload signals plus the implicit valid/ready pair.
/// Flattens to `<name>_valid`, `<name>_ready`, and `<name>_<sig>` per
/// payload signal at lowering time.
#[derive(Debug, Clone)]
pub struct HandshakeChannel {
    pub name: Ident,
    /// `send` (initiator drives valid + payload) or `receive`
    /// (initiator drives ready, target drives valid + payload).
    pub role: HandshakeRole,
    /// `valid_ready` for now; `req_ack_4phase` and others are parsed
    /// but not lowered.
    pub variant: Ident,
    pub payload: Vec<BusSignal>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeRole {
    Send,
    Receive,
}

// ── External (Verilator-bound) module (§10.5) ─────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExternalModuleDecl {
    pub name: Ident,
    pub kind: Ident, // typically `verilator`
    pub fields: Vec<ExternalField>,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExternalField {
    pub name: Ident,
    pub value: Expr,
    pub span: Span,
}

// ── Functions ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FunctionDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub return_ty: Option<TypeExpr>,
    pub body: Block,
    pub span: Span,
    pub doc: Option<String>,
}

// ── Generic parameters / function parameters ──────────────────────────────────

/// `name : type` (function-style) or `Name = expr` (named-arg style at call).
#[derive(Debug, Clone)]
pub struct Param {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub default: Option<Expr>,
    pub span: Span,
}

// ── Type expressions ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum TypeExpr {
    /// Named type, optionally generic-applied: `AxiBus#(P=8, Q=4)` or `Foo`.
    ///
    /// `mode` annotates the type with a transactor active/passive
    /// role at the binding site (`let xact : T active = bind axil`).
    /// `None` for non-transactor types and for transactor decl
    /// references that aren't at an instantiation site (e.g.
    /// `bound to T` clauses, struct field types other than at
    /// `let .. = bind ..`). The type-checker rejects mode
    /// annotations on non-transactor types and missing-mode on
    /// transactor instantiations.
    Named {
        name: Path,
        generics: Vec<TypeArg>,
        mode: Option<TransactorMode>,
        span: Span,
    },
    /// `uint<N>`, `sint<N>`, `bits<N>`, `Vec<T, N>`, `event<T>`, `event comb<T>`,
    /// `buffer<T, depth=N>`, `stream<T>`, `state<T>`, `queue<T>`, `TSeq<T>`.
    Builtin {
        name: BuiltinTy,
        args: Vec<TypeArg>,
        span: Span,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinTy {
    UInt,         // uint<N>
    SInt,         // sint<N>
    Bits,         // bits<N>
    UIntCap,      // UInt<N> (legacy ARCH form)
    SIntCap,      // SInt<N>
    Bool,
    BoolLower,
    Bit,
    Int,
    Time,
    Prop,
    Pseq,
    Severity,
    Logger,
    String,
    Vec,
    Event,
    EventComb,
    Buffer,
    Stream,
    State,
    Queue,
    TSeq,
    Clock,
    Reset,
}

#[derive(Debug, Clone)]
pub enum TypeArg {
    /// Positional type or value: `Vec<bits<64>, 256>`.
    Expr(Expr),
    Type(TypeExpr),
    /// Named: `AxiBus#(ADDR_W=32)`, or `buffer<T, depth=16>`.
    Named { name: Ident, value: Expr },
}

// ── Expressions ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: Box<ExprKind>,
    pub span: Span,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Self { kind: Box::new(kind), span }
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    /// Numeric literal (decimal, hex, bin, sized) — keep raw text.
    Int(String),
    Float(String),
    Time(String),
    String(String),
    Bool(bool),
    /// Identifier reference (unqualified).
    Ident(Ident),
    /// `a.b` — also used for `.field` shorthand inside list-comprehension
    /// closures; in that case `target` is `ImplicitSelf`.
    Field { target: Expr, name: Ident },
    ImplicitSelf, // The implicit `.` in `.field` shorthand
    /// `a[i]` or `a[m..n]` (single-bracket subscript / slice).
    Index { target: Expr, index: Expr },
    /// `a[m:n]` — bit slice; kept distinct from range-slice to preserve syntax.
    BitSlice { target: Expr, hi: Expr, lo: Expr },
    /// Function or method call: `f(a, b)` or `t.foo(args)`.
    Call { callee: Expr, args: Vec<CallArg> },
    /// `a as Type` — cast.
    Cast { expr: Expr, ty: TypeExpr },
    /// `dut.s_axi_awready <- 1` — channel send / state write target.
    Send { target: Expr, value: Expr },
    /// Assignment `lhs = rhs` (statement-level only — see Stmt::Assign).
    Unary { op: UnaryOp, expr: Expr },
    Binary { op: BinaryOp, lhs: Expr, rhs: Expr },
    /// `cond ? then_branch : else_branch` — conditional (ternary) expression.
    /// Right-associative; lower precedence than every other operator
    /// except assignment (which isn't an expression in HARC).
    Ternary { cond: Expr, then_branch: Expr, else_branch: Expr },
    /// SVA delay `##N expr` and `##[m:n] expr` — unary in expression position.
    HashHash { count: HashCount, expr: Expr },
    /// `e [*N]` / `e [*m:n]` — sequence repetition.
    SeqRepeat { expr: Expr, count: HashCount },
    /// `[a..b]` range expression — used in `[range(a,b)]`, `bins`, etc.
    RangeLit { lo: Option<Expr>, hi: Option<Expr> },
    /// `{a, b, c}` set literal.
    SetLit(Vec<Expr>),
    /// `dist {[0..0xFF] :/ 80, c :/ 20}` distribution literal.
    DistLit(Vec<DistEntry>),
    /// `$past(e, N)`, `$rose(e)`, `$fell(e)`, `$stable(e)`, `$clog2(e)`.
    SystemCall { name: SystemFn, args: Vec<Expr> },
    /// `randomize(t)` or `randomize(t) with <body>` or `blocking randomize(t) with <body>`.
    Randomize {
        blocking: bool,
        target: Expr,
        with_body: Vec<Expr>,
    },
    /// `dist <expr> { ... }` directive form inside `randomize ... with`.
    DistDirective { target: Expr, entries: Vec<DistEntry> },
    /// Parenthesized.
    Paren(Expr),
    /// `name = value` — used in struct literals and named call args.
    NamedArg { name: Ident, value: Expr },
    /// `Type { name: value, ... }` struct literal.
    StructLit { ty: TypeExpr, fields: Vec<NamedExpr> },
    /// `a -> b` / `a ->[N] b` / `a ->[m:n] b` cover-sequence pattern operator
    /// (only valid inside `cover sequence` patterns; we accept it as a binary).
    CoverArrow { lhs: Expr, rhs: Expr, count: Option<HashCount> },
    /// `solve_before(a, b)` / `solve_after(a, b)` directive — kept as a call.
    Solve { kind: SolveKind, args: Vec<Expr> },
    /// `e in <set-or-range>` membership test.
    Membership { expr: Expr, set: Expr },
}

#[derive(Debug, Clone)]
pub enum CallArg {
    Expr(Expr),
    Named { name: Ident, value: Expr },
}

#[derive(Debug, Clone)]
pub struct NamedExpr {
    pub name: Ident,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum HashCount {
    Const(Expr),
    Range { lo: Expr, hi: Expr },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,    // -
    Not,    // !
    NotKw,  // not
    BitNot, // ~
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    // arithmetic
    Add, Sub, Mul, Div, Mod,
    // comparison
    Eq, Ne, Lt, Le, Gt, Ge,
    // logical
    AndAnd, OrOr, AndKw, OrKw,
    // bitwise
    BitAnd, BitOr, BitXor, Shl, Shr,
    // temporal SVA
    PipeImplies,     // |->
    PipeImpliesNext, // |=>
    Throughout,
    Within,
    Intersect,
    // membership
    In, Inside,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemFn {
    Rose,
    Fell,
    Stable,
    Past,
    Clog2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolveKind {
    Before,
    After,
}

// ── Statements / Blocks ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Let(LetStmt),
    Assign { target: Expr, value: Expr },     // lhs = rhs
    Send { target: Expr, value: Expr },       // lhs <- rhs
    For(ForStmt),
    Repeat(RepeatStmt),
    Loop(Block),
    /// `while <cond> ... end while` — pre-tested loop.
    While { cond: Expr, body: Block, span: Span },
    /// `break` — exit innermost enclosing loop (`while` / `loop` / `for` /
    /// `repeat`). Type-checked at codegen: a `break` outside a loop is a
    /// hard error.
    Break { span: Span },
    /// `continue` — skip to next iteration of innermost enclosing loop.
    Continue { span: Span },
    If(IfStmt),
    Fork(ForkStmt),
    Parallel(Vec<Block>),
    Schedule(Vec<Block>),
    Select(Vec<SelectArm>),
    On(OnHandler),
    Emit { name: Path, args: Vec<CallArg>, span: Span },
    Yield(Expr),
    Return(Option<Expr>),
    Apply(ApplyDecl),
    /// `assert name` / `assert <expr>` / `assert <expr> else fail("...")`.
    Assert(Verify),
    Assume(Verify),
    /// `cover <expr>` / `cover <name>` / `cover property <expr>`.
    Cover(Verify),
    /// `randomize(t) with ...` as a statement.
    Randomize { blocking: bool, target: Expr, with_body: Vec<Expr> },
    /// `log(severity, "...", id="...", verbosity=HIGH)`.
    Log { args: Vec<CallArg>, span: Span },
    /// `logf("path.log", severity, "...")` — like `log` but writes to a
    /// named file (in addition to stdout). Useful for per-component or
    /// per-protocol log streams. The first positional arg is the file
    /// path (string literal); the rest follow `log` semantics.
    LogF { args: Vec<CallArg>, span: Span },
    /// Bare expression statement (call, etc.).
    Expr(Expr),
    /// `after N cycles ... end after` — suspend primitive (§7.4) with a body.
    After { duration: Expr, body: Block, span: Span },
    /// `wait <expr> cycles [on <clock>]` — single-statement suspend,
    /// ARCH-shape sugar for `after N cycles ... end after` with an empty
    /// body. The trailing `cycles` / `cycle` keyword is decorative and
    /// optional. When `clock` is set, advances simulated time so the
    /// named clock sees `<expr>` more rising edges (other clocks
    /// continue ticking at their natural rate).
    Wait { duration: Expr, clock: Option<Ident>, span: Span },
}

#[derive(Debug, Clone)]
pub struct LetStmt {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub value: Option<Expr>,
    pub bind: bool, // true if `= bind ...` — DUT/env binding form
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ForStmt {
    pub var: Ident,
    pub iter: Expr,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RepeatStmt {
    pub count: Expr,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub cond: Expr,
    pub then_block: Block,
    pub elsifs: Vec<(Expr, Block)>,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ForkStmt {
    pub branches: Vec<Block>,
    pub join: ForkJoin,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkJoin {
    All,
    Any,
    None,
}

#[derive(Debug, Clone)]
pub struct SelectArm {
    pub event: Expr,    // before `=>`
    pub action: Block,  // after `=>` (one statement, lifted to block)
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct OnHandler {
    pub event: Expr,
    pub hook: Option<HookSide>, // `pre` / `post` (§7.3)
    /// Edge mode for cycle-trigger `on <bool-expr>` form. Defaults to
    /// `Rising` — fires on 0→1 transitions of the trigger expression.
    /// Ignored for event-subscription `on event_name(arg)` form.
    pub edge: EdgeMode,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookSide {
    Pre,
    Post,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeMode {
    /// 0→1 transition (default for `on <bool-expr>`).
    Rising,
    /// 1→0 transition.
    Falling,
    /// Every cycle the expression is true (no edge detection).
    Level,
}

#[derive(Debug, Clone)]
pub struct Verify {
    /// `assert name_only` form — looks up an existing property by name.
    pub named: Option<Ident>,
    /// Inline expression form.
    pub expr: Option<Expr>,
    /// `else fail("...")` clause on assert.
    pub else_fail: Option<Expr>,
    /// `cover property` / `cover sequence` mode markers — `cover property` is
    /// the default for cover; `cover sequence` is parsed at item-level.
    pub property_kw: bool,
    pub span: Span,
}
