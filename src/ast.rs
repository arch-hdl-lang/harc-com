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
    /// Raw text of the leading `//!` block (prefixes stripped, lines
    /// joined by `\n`). Captures both free-form inner-doc prose AND a
    /// `---`-fenced YAML frontmatter block verbatim. None when the
    /// file has no leading `//!` block.
    pub inner_doc: Option<String>,
    /// Raw text of the `---` … `---` YAML-frontmatter sub-block at
    /// the very top of `inner_doc`, with the fence lines stripped.
    /// Stored separately for downstream tooling (RAG indexer, doc
    /// generator) that wants the structured part without re-scanning.
    /// Always a substring of `inner_doc` when both are present. None
    /// when the file has no frontmatter block. Mirrors arch-com's
    /// `plan_arch_doc_comments.md` v1 design.
    ///
    /// Compiler does NOT interpret the YAML in v0 — it stores the raw
    /// text and passes it through. Conventional fields downstream
    /// tooling will look for: `spec_md` (path to authoritative
    /// markdown spec, with optional `#anchor`), `tags` (list of
    /// retrieval tags), `refs` (list of citations / ticket IDs / URLs).
    pub frontmatter: Option<String>,
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
    /// `extern fn name(params) -> ret` — forward declaration of a
    /// C / C++ reference function (spec §9). See `ExternFnDecl`.
    ExternFn(ExternFnDecl),
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
    /// `impl <target> for <TestName> { setup ... run ... check ...
    /// teardown ... phase <name> ... }` — per-target test implementation
    /// (spec §7.2). Replaces the older inline `scope sim` block: the
    /// test struct holds field declarations only, while each target
    /// (`sim`, `emu`, ...) gets its own top-level `impl` item. Mirrors
    /// Rust's `impl Trait for Type` shape — sim and emu impls of the
    /// same test compose orthogonally and can live in separate files.
    /// In v0 only `run` actually lowers; `setup`/`check`/`teardown` and
    /// custom `phase <name>` blocks parse but are placeholder no-ops
    /// (the latter are inlined when the user calls them by name from
    /// `run`).
    Impl(ImplDecl),
    /// `regblock R via Helper { register NAME @ ADDR width N reset V
    /// access POLICY }` — Register Abstraction Layer block, lowered
    /// to a POD mirror struct + `constexpr` address table + frontdoor
    /// reads/writes that route through the helper transactor's
    /// `write(addr, data)` / `read(addr) -> data` methods. See
    /// docs/ral-support.md. Phase 1a: registers only (no field-level
    /// decomposition yet).
    Regblock(RegblockDecl),
}

// ── Register Abstraction Layer ────────────────────────────────────────────────

/// `regblock <Name> via <Helper> [width <N>] { <register>* }`
#[derive(Debug, Clone)]
pub struct RegblockDecl {
    pub name: Ident,
    /// `via <HelperType>` — names the transactor whose `write(addr, data)`
    /// and `read(addr) -> data` methods receive bus traffic. Required in
    /// Phase 1a; later phases may infer it from a `bound to BusT` clause
    /// or auto-synthesize a helper for known protocol types.
    pub via_helper: Ident,
    /// Default register width in bits (used when a `register` block
    /// omits an explicit width). Defaults to 32 if absent.
    pub default_width: Option<u32>,
    pub registers: Vec<RegisterDecl>,
    pub span: Span,
    pub doc: Option<String>,
    pub inner_doc: Option<String>,
}

/// `register <Name> @ <addr> [width <N>] [reset <V>] [access <Policy>]`
#[derive(Debug, Clone)]
pub struct RegisterDecl {
    pub name: Ident,
    /// Byte-offset within the regblock. Parsed as an `Expr` so hex
    /// literals (`0x18`), constant references, and simple arithmetic
    /// all work; constant-folded at codegen time.
    pub offset: Expr,
    /// Width in bits. `None` means inherit the regblock's `default_width`.
    pub width: Option<u32>,
    /// Reset value. `None` means zero.
    pub reset: Option<Expr>,
    /// Access policy. Phase 1a supports `rw` only; the enum is shaped
    /// for the RFC-§3.1 expansion to `ro`/`wo`/`w1c`/`w1s`/`wclr`/
    /// `wset`/`rc`/`rs` in later phases.
    pub access: RegAccess,
    pub span: Span,
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegAccess {
    Rw,
}

impl RegAccess {
    pub fn keyword(self) -> &'static str {
        match self {
            RegAccess::Rw => "rw",
        }
    }
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
    pub inner_doc: Option<String>,
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
    /// `//!` lines immediately after the opening `struct <Name>` and
    /// before the first field. Documents the struct from the inside —
    /// useful for type-level invariants or spec-link annotations.
    pub inner_doc: Option<String>,
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
    pub inner_doc: Option<String>,
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
    pub inner_doc: Option<String>,
}

// ── Component declarations (agent, env, scoreboard, sequencer)
//
// `transactor` (spec §8.1) is a sibling top-level Item but lowers
// through ComponentDecl-shaped structures internally; the synthetic
// ComponentDecl carries `ComponentKind::Transactor` so existing
// component codegen paths can still discriminate when needed (e.g.
// for tag prefixes in registration sites).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    Agent,
    Env,
    Scoreboard,
    Sequencer,
    /// Synthesizable BFM unit. No top-level `Item::Transactor`-style
    /// parsing path uses this directly; instead, the codegen layer
    /// tags a synth ComponentDecl built from a `TransactorDecl` with
    /// this kind so the rest of the pipeline can identify it.
    Transactor,
    /// `testbench T ... end testbench T` — the DUT-owning, helper-
    /// method-bearing variant per docs/test-ergonomics.md §3.
    /// Lowered through the same component machinery as `env`; the
    /// distinction is conventional + source-keyword level.
    Testbench,
}

impl ComponentKind {
    pub fn keyword(self) -> &'static str {
        match self {
            ComponentKind::Agent => "agent",
            ComponentKind::Env => "env",
            ComponentKind::Scoreboard => "scoreboard",
            ComponentKind::Sequencer => "sequencer",
            ComponentKind::Transactor => "transactor",
            ComponentKind::Testbench => "testbench",
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
    pub inner_doc: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ComponentItem {
    Field(ComponentField),
    Connect(ConnectBlock),
    OnHandler(OnHandler),
    Hookable(HookableMethod),
    /// Inline `apply Name` inside a component body (rare but legal in scopes).
    Apply(ApplyDecl),
    /// Built-in watchdog (spec §8.6). At elaboration time this desugars
    /// to a synthetic `hookable watchdog()` whose body asserts the
    /// `idle(max_idle)` predicate is false, plus a periodic `_checkers`
    /// closure that calls the method every `period` cycles. External
    /// aspects can attach via `on <ComponentType>.watchdog pre/post`,
    /// reusing the existing hookable-hook mechanism.
    Watchdog(WatchdogDecl),
}

/// `watchdog … end watchdog` declaration (spec §8.6). All fields
/// optional; missing `period` / `max_idle` default to 1000 / 10000
/// cycles respectively. `disabled = true` (from `watchdog disabled`)
/// suppresses all codegen — useful for tests where the user wants
/// to skip the watchdog entirely (e.g. soak tests, randomized stress).
#[derive(Debug, Clone)]
pub struct WatchdogDecl {
    /// `watchdog disabled` — opt-out. `period`/`max_idle`/`body` are
    /// ignored when `disabled = true`.
    pub disabled: bool,
    /// `period <expr> cycles` clause. `None` → default 1000 cycles.
    /// May reference component fields so per-test override works:
    ///   `agent A
    ///       wdog_period : uint<32> default 1000
    ///       watchdog
    ///           period wdog_period cycles
    ///       end watchdog
    ///   end agent A`
    pub period: Option<Expr>,
    /// `max_idle <expr> cycles` clause. `None` → default 10000 cycles.
    pub max_idle: Option<Expr>,
    /// Optional user statements that run BEFORE the idle check on each
    /// firing. The conventional use is debug logging
    /// (`log(info, "[wdog] cycle=${cycle_count}")`).
    pub body: Block,
    pub span: Span,
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
    pub inner_doc: Option<String>,
}

// ── Test and scope (§7.2) ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TestDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub items: Vec<TestItem>,
    pub span: Span,
    pub doc: Option<String>,
    pub inner_doc: Option<String>,
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
    /// `phase <name> ... end phase <name>` — user-defined named phase
    /// block at test scope (docs/test-ergonomics.md inline form).
    /// Equivalent to the same body item inside an `impl sim for T` —
    /// codegen lifts both into the same `custom_phases` table.
    Phase(Ident, Block),
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

// ── Impl block (§7.2) ─────────────────────────────────────────────────────────

/// `impl <target> for <TestName> { ... }` — per-target test implementation.
///
/// `target` is a tag identifier — `sim` for native simulation, `emu` for
/// emulation; v0 only emits codegen for `sim`. The body holds the four
/// reserved phase blocks (`setup` / `run` / `check` / `teardown`) plus any
/// number of user-defined `phase <name> ... end phase <name>` helper
/// blocks. v0 codegen only lowers `run` (and inlines user phases the
/// user calls from `run`); the other reserved phases parse but emit
/// placeholder comments so the syntax surface is locked.
#[derive(Debug, Clone)]
pub struct ImplDecl {
    /// Target tag — `sim`, `emu`, etc. Free-form identifier for now;
    /// v0 only recognizes `sim` at codegen time.
    pub target: Ident,
    /// Test the impl applies to. Resolution at codegen time picks the
    /// `TestDecl` with matching name; missing-test error is reported
    /// from the codegen layer.
    pub test_name: Ident,
    /// Reserved-phase + custom-phase body items, in source order.
    pub items: Vec<ImplItem>,
    pub span: Span,
    pub doc: Option<String>,
    pub inner_doc: Option<String>,
}

/// A single body item inside an `impl <target> for <TestName>` block.
#[derive(Debug, Clone)]
pub enum ImplItem {
    /// `setup ... end setup` — runs once before clocks start.
    /// v0 placeholder (parses, no-op codegen).
    Setup(Block),
    /// `run ... end run` — main coroutine body. The runtime entry point
    /// for the test.
    Run(Block),
    /// `check ... end check` — runs once after `run` completes.
    /// v0 placeholder.
    Check(Block),
    /// `teardown ... end teardown` — runs at process exit.
    /// v0 placeholder.
    Teardown(Block),
    /// `phase <name> ... end phase <name>` — user-defined named phase.
    /// Pure code-organization helper; not auto-fired by the runtime.
    /// User invokes it by name (`<name>()`) from `run` or another
    /// phase. v0 lowers as an inlined block at the call site.
    Phase(Ident, Block),
}

// ── Extend aspect (§3.6) ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExtendDecl {
    pub target: Path,
    pub body: ExtendBody,
    pub span: Span,
    pub doc: Option<String>,
    pub inner_doc: Option<String>,
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
    pub inner_doc: Option<String>,
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
    pub inner_doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PseqDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub body: Expr,
    pub span: Span,
    pub doc: Option<String>,
    pub inner_doc: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CoverSequenceDecl {
    pub name: Ident,
    pub pattern: Expr,
    pub span: Span,
    pub doc: Option<String>,
    pub inner_doc: Option<String>,
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
    pub inner_doc: Option<String>,
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
    pub inner_doc: Option<String>,
}

/// `extern function name(params) -> ret` — forward-declares a function
/// whose implementation lives in a separate C / C++ source file linked
/// into the verilator-built TB (spec §9). The typical use is calling a
/// reference model (CRC, AES, ISA simulator, etc.) from a scoreboard
/// to compare against the DUT. No body — the parser ends the
/// declaration at the return-type (or at the close-paren when the
/// function returns void).
///
/// Codegen lowers each `extern function` to a single `extern "C"
/// <ret> <name>(<params>);` forward declaration at file scope. The
/// user provides the implementation in a `.c`/`.cpp` file passed via
/// `harc sim --ref-src <file>` (repeatable). The C side sees plain
/// scalar types (`uint64_t`, `int64_t`, `bool`); 65–128b parameters
/// use `_harc_u128` (the runtime header's typedef for
/// `unsigned __int128`) — the user includes `harc_thread_rt.h` or
/// uses the underlying compiler extension type directly.
#[derive(Debug, Clone)]
pub struct ExternFnDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub return_ty: Option<TypeExpr>,
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
    /// `wait until <expr> [timeout N cycles fail("...")]` — suspend
    /// the current coroutine until the predicate becomes true at a
    /// posedge of the primary clock. With `all of` / `any of` the
    /// predicate is a conjunction / disjunction over multiple
    /// sub-expressions; on `timeout` the codegen reports which
    /// sub-predicate was false (or, for `any of`, that none became
    /// true) in the diagnostic. Spec §7.9.
    ///
    /// Forms supported (after the `wait until` prefix):
    /// - single condition: `wait until <expr> [timeout ...]`
    /// - all-of:           `wait until all of <e1>, <e2>, ... [timeout ...]`
    /// - any-of:           `wait until any of <e1>, <e2>, ... [timeout ...]`
    ///
    /// `mode == WaitUntilMode::Single` always has exactly one entry
    /// in `conditions`; `AllOf` / `AnyOf` have one or more.
    WaitUntil {
        mode: WaitUntilMode,
        conditions: Vec<Expr>,
        timeout: Option<WaitTimeout>,
        span: Span,
    },
    /// `fail("...")` as a standalone statement (also accepted inside
    /// `assert ... else fail(...)` via the existing `Verify.else_fail`
    /// channel). Lowering: unconditional `sim_log_line("FAIL", msg);
    /// errors++;` — same shape as the failure arm of an inline assert,
    /// just without the surrounding `if (!cond)` guard. Useful when a
    /// failure is triggered by control flow rather than a boolean
    /// predicate (e.g. inside an `if`/`for` body where the failure
    /// condition is structural rather than expressible as one
    /// expression).
    Fail { msg: Expr, span: Span },
}

/// Quantifier over the predicate list of a `wait until` statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitUntilMode {
    /// `wait until <single-expr>` — no quantifier prefix.
    Single,
    /// `wait until all of <e1>, <e2>, …` — conjunction.
    AllOf,
    /// `wait until any of <e1>, <e2>, …` — disjunction.
    AnyOf,
}

/// `timeout N cycles fail("…")` tail clause on a `wait until`.
/// The `message` is an optional string literal — when present the
/// codegen logs it on timeout *before* the per-predicate breakdown.
/// `cycles` is any integer expression (typically a const, but
/// component-parameter expressions are accepted so each test can
/// pick its own budget without editing the wait site).
#[derive(Debug, Clone)]
pub struct WaitTimeout {
    pub cycles: Expr,
    pub message: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LetStmt {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub value: Option<Expr>,
    pub bind: bool, // true if `= bind ...` — DUT/env binding form
    /// Hierarchical signal probes attached to this `let`. Used on the
    /// test-level `let dut : T` to declare observation points inside
    /// the DUT (see docs/probe-signals.md). Empty for ordinary lets.
    pub probes: Vec<Probe>,
    pub span: Span,
}

/// A single `probe <name> : <ty> at <path>` declaration inside a
/// `let dut : T` block. Lowers to a Verilator `bind` stub with
/// per-signal `/* verilator public_flat_rd */` annotations; signal
/// access (`dut.<name>`) resolves to the mangled root accessor.
#[derive(Debug, Clone)]
pub struct Probe {
    pub name: Ident,
    pub ty: TypeExpr,
    /// Dotted hierarchical path inside the DUT, e.g. `alu0.result`.
    /// Stored as joined string for verbatim emission into the SV stub;
    /// HARC does not validate the path — Verilator does, with
    /// diagnostics cross-referenced back to the probe decl.
    pub path: String,
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
    /// Ignored for event-subscription `on event_name(arg)` form and
    /// for the periodic form (`on N cycles`).
    pub edge: EdgeMode,
    pub body: Block,
    pub span: Span,
    /// `on <N> cycles ... end on` — when `true`, `event` is the period
    /// (in primary-clock cycles) at which to fire the body, not a
    /// boolean trigger expression. The codegen lowers this to a
    /// `_checkers` closure that compares `cycle_count` against a
    /// per-handler "last-fired" tracker so the body fires once every
    /// `event` cycles regardless of `event` being constant or
    /// variable (the period is re-read each cycle, so users can
    /// override it from the test scope).
    pub periodic: bool,
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

// ── Construct trait ───────────────────────────────────────────────────────────

/// Centralizing trait for every top-level `Item::*` variant. Ports the
/// shape from arch-com's `ast::Construct`: a uniform accessor surface
/// so the learning-store feature-harvester (and future passes that
/// want a single dispatch point) can walk items without N-arm matches.
///
/// v0 covers the five always-applicable accessors —
/// `kind_label` / `name` / `span` / `doc` / `inner_doc`. HARC's
/// per-construct inner doc isn't captured yet (only `SourceFile.inner_doc`
/// is populated by the parser), so `inner_doc()` returns `None` for
/// every concrete impl today; the slot is in place so adding it later
/// is a parser-level change with no Construct churn.
pub trait Construct {
    /// The lowercase keyword introducing this construct in source
    /// (`"transactor"`, `"test"`, `"impl"`, `"struct"`, …). Used as
    /// the `error_code` field on `kind: "feature"` events so BM25
    /// can rank by construct kind.
    fn kind_label(&self) -> &'static str;

    /// The construct's name as declared. For `impl <target> for
    /// <TestName>`, returns `test_name` (the item that the impl
    /// hangs off of); for `extend X { ... }`, returns the last
    /// segment of `target`. `use foo.bar.Baz` returns `Baz`.
    fn name(&self) -> &Ident;

    /// Source span covering the construct.
    fn span(&self) -> Span;

    /// Outer doc-comment text (`///` lines immediately preceding the
    /// construct). `None` if no run was attached.
    fn doc(&self) -> Option<&str>;

    /// Inner doc-comment text (`//!` lines immediately after the
    /// opening keyword and name). Reserved for the per-construct inner
    /// doc surface described in spec §2.5.2 — none of HARC's
    /// concrete `*Decl` types carry the field yet, so this returns
    /// `None` uniformly today.
    fn inner_doc(&self) -> Option<&str> {
        None
    }
}

// ── Construct impls ──────────────────────────────────────────────────────────
//
// One `impl Construct for $ty` per `Item::*` variant. Most pull
// `name` / `span` / `doc` directly off the `*Decl` struct;
// `UseDecl` / `ExtendDecl` / `ApplyDecl` / `ImplDecl` carry a `path`
// or different field names and need slim shim accessors.

/// Implement `Construct` for a `*Decl` with the canonical
/// `name: Ident`, `span: Span`, `doc: Option<String>` fields.
/// Pass `+inner` when the `*Decl` also carries
/// `pub inner_doc: Option<String>`; omit otherwise and the slot
/// returns `None` via the trait default.
macro_rules! impl_construct_direct {
    ($ty:ty, $label:expr) => {
        impl Construct for $ty {
            fn kind_label(&self) -> &'static str { $label }
            fn name(&self) -> &Ident { &self.name }
            fn span(&self) -> Span { self.span }
            fn doc(&self) -> Option<&str> { self.doc.as_deref() }
        }
    };
    ($ty:ty, $label:expr, +inner) => {
        impl Construct for $ty {
            fn kind_label(&self) -> &'static str { $label }
            fn name(&self) -> &Ident { &self.name }
            fn span(&self) -> Span { self.span }
            fn doc(&self) -> Option<&str> { self.doc.as_deref() }
            fn inner_doc(&self) -> Option<&str> { self.inner_doc.as_deref() }
        }
    };
}

impl_construct_direct!(PackageDecl, "package", +inner);
impl_construct_direct!(ConstDecl, "const");
impl_construct_direct!(DomainDecl, "domain");
impl_construct_direct!(StructDecl, "struct", +inner);
impl_construct_direct!(EnumDecl, "enum");
impl_construct_direct!(TransactionDecl, "transaction", +inner);
impl_construct_direct!(RelationDecl, "relation");
impl_construct_direct!(TseqDecl, "tseq", +inner);
impl_construct_direct!(TransactorDecl, "transactor", +inner);
impl_construct_direct!(TestDecl, "test", +inner);
impl_construct_direct!(CovergroupDecl, "covergroup", +inner);
impl_construct_direct!(PropertyDecl, "property", +inner);
impl_construct_direct!(PseqDecl, "pseq", +inner);
impl_construct_direct!(CoverSequenceDecl, "cover_sequence", +inner);
impl_construct_direct!(BusDecl, "bus", +inner);
impl_construct_direct!(ExternalModuleDecl, "module");
impl_construct_direct!(FunctionDecl, "function", +inner);
impl_construct_direct!(ExternFnDecl, "extern fn");

// `agent` / `env` / `scoreboard` / `sequencer` share `ComponentDecl`;
// the kind_label varies. Implemented manually so `kind` selects the
// right keyword string.
impl Construct for ComponentDecl {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            ComponentKind::Agent => "agent",
            ComponentKind::Env => "env",
            ComponentKind::Scoreboard => "scoreboard",
            ComponentKind::Sequencer => "sequencer",
            // Transactor lives in the AST as a separate `TransactorDecl`
            // (not a `ComponentDecl`), but the `ComponentKind` enum
            // includes the variant for historical reasons. If a
            // `ComponentDecl` with this kind ever shows up at runtime,
            // surface it under the same label as the dedicated
            // transactor.
            ComponentKind::Transactor => "transactor",
            ComponentKind::Testbench => "testbench",
        }
    }
    fn name(&self) -> &Ident { &self.name }
    fn span(&self) -> Span { self.span }
    fn doc(&self) -> Option<&str> { self.doc.as_deref() }
    fn inner_doc(&self) -> Option<&str> { self.inner_doc.as_deref() }
}

// `use foo.bar.Baz` — Construct uses the last path segment as the
// "name" so the feature event lands under `Baz`. A self-owned `Ident`
// would be cleaner but UseDecl doesn't store one; cache via a
// thread-local'd cell to keep the borrow lifetime aligned with
// `&self`. Simpler approach: synthesize the Ident on the fly via a
// `OnceCell` per UseDecl. For v0 we just return the last segment's
// Ident directly — `Path.segments` already owns the Idents.
impl Construct for UseDecl {
    fn kind_label(&self) -> &'static str { "use" }
    fn name(&self) -> &Ident {
        // Return the last segment's Ident; falls back to a static
        // synthetic name when the path is empty (shouldn't happen
        // post-parse but the borrow surface needs something to point
        // at).
        self.path
            .segments
            .last()
            .map(|s| s)
            .unwrap_or_else(|| {
                static EMPTY: std::sync::OnceLock<Ident> = std::sync::OnceLock::new();
                EMPTY.get_or_init(|| Ident {
                    name: "<empty-use>".into(),
                    span: Span::new(0, 0),
                })
            })
    }
    fn span(&self) -> Span { self.span }
    fn doc(&self) -> Option<&str> { self.doc.as_deref() }
}

// `extend X { ... }` — name is the last segment of `target`.
impl Construct for ExtendDecl {
    fn kind_label(&self) -> &'static str { "extend" }
    fn name(&self) -> &Ident {
        self.target
            .segments
            .last()
            .unwrap_or_else(|| {
                static EMPTY: std::sync::OnceLock<Ident> = std::sync::OnceLock::new();
                EMPTY.get_or_init(|| Ident {
                    name: "<empty-extend>".into(),
                    span: Span::new(0, 0),
                })
            })
    }
    fn span(&self) -> Span { self.span }
    fn doc(&self) -> Option<&str> { self.doc.as_deref() }
    fn inner_doc(&self) -> Option<&str> { self.inner_doc.as_deref() }
}

// `apply Foo.Bar` — no name, no doc. Synthesize an empty name and
// return None for doc. Apply isn't a real construct that ever
// produces a useful feature event, but Construct must be exhaustive
// so `as_construct` covers every Item variant.
impl Construct for ApplyDecl {
    fn kind_label(&self) -> &'static str { "apply" }
    fn name(&self) -> &Ident {
        self.path
            .segments
            .last()
            .unwrap_or_else(|| {
                static EMPTY: std::sync::OnceLock<Ident> = std::sync::OnceLock::new();
                EMPTY.get_or_init(|| Ident {
                    name: "<empty-apply>".into(),
                    span: Span::new(0, 0),
                })
            })
    }
    fn span(&self) -> Span { self.span }
    fn doc(&self) -> Option<&str> { None }
}

// `impl <target> for <TestName>` — "name" is the TestName (which
// is what the impl hangs off of); kind_label is "impl".
impl Construct for ImplDecl {
    fn kind_label(&self) -> &'static str { "impl" }
    fn name(&self) -> &Ident { &self.test_name }
    fn span(&self) -> Span { self.span }
    fn doc(&self) -> Option<&str> { self.doc.as_deref() }
    fn inner_doc(&self) -> Option<&str> { self.inner_doc.as_deref() }
}

impl Item {
    /// Single dispatch point covering every `Item::*` variant. Lets
    /// the learning-store feature harvester (and future passes)
    /// access the common `Construct` accessors without an N-arm
    /// match at every call site.
    pub fn as_construct(&self) -> &dyn Construct {
        match self {
            Item::Use(u) => u,
            Item::Package(p) => p,
            Item::Const(c) => c,
            Item::Domain(d) => d,
            Item::Struct(s) => s,
            Item::Enum(e) => e,
            Item::Transaction(t) => t,
            Item::Relation(r) => r,
            Item::Tseq(t) => t,
            Item::Agent(c) => c,
            Item::Env(c) => c,
            Item::Scoreboard(c) => c,
            Item::Sequencer(c) => c,
            Item::Test(t) => t,
            Item::Extend(e) => e,
            Item::Covergroup(g) => g,
            Item::Property(p) => p,
            Item::Pseq(p) => p,
            Item::CoverSequence(c) => c,
            Item::ExternalModule(m) => m,
            Item::Function(f) => f,
            Item::ExternFn(f) => f,
            Item::Apply(a) => a,
            Item::Bus(b) => b,
            Item::Transactor(t) => t,
            Item::Impl(i) => i,
            Item::Regblock(r) => r,
        }
    }
}

impl_construct_direct!(RegblockDecl, "regblock", +inner);
