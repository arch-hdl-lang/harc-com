//! TB-IR — typed, CFG-shaped intermediate representation between HARC's
//! AST and codegen. See `docs/tb-ir-design.md` for the full contract.
//!
//! MVP spine: the core types, a textual `Display` form (`display.rs`),
//! a structural verifier (`verify.rs`), and AST → IR lowering for the
//! core statement subset (`lower/`). Constructs outside the subset are
//! rejected at lowering time with `LowerError::Unsupported` — the IR
//! never silently mis-lowers.

pub mod display;
pub mod lower;
pub mod passes;
pub mod verify;

macro_rules! ir_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);

        impl $name {
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

ir_id!(
    /// Index into `TbProgram::functions`.
    FunctionId
);
ir_id!(
    /// Index into `TbFunction::blocks`.
    BlockId
);
ir_id!(
    /// Index into `TbFunction::locals`.
    LocalId
);
ir_id!(
    /// Index into `TbProgram::covgroups`.
    CovgroupId
);
ir_id!(
    /// Index into `TbProgram::testbenches`.
    TestbenchId
);
ir_id!(
    /// Index into `TbProgram::records`.
    RecordId
);
ir_id!(
    /// Index into `TbProgram::transactors`.
    TransactorId
);
ir_id!(
    /// Index into `TbProgram::scoreboards`.
    ScoreboardId
);

/// Whole-program IR for one merged HARC source file (post
/// `merge_for_sim` + impl-for desugaring).
#[derive(Debug, Clone, Default)]
pub struct TbProgram {
    pub functions: Vec<TbFunction>,
    pub testbenches: Vec<TestbenchSchema>,
    pub tests: Vec<TestSchema>,
    pub covgroups: Vec<CovgroupSchema>,
    /// Value-record schemas (`transaction` declarations), in file
    /// order. The design doc's records table; see `RecordSchema`.
    pub records: Vec<RecordSchema>,
    /// Transactor schemas (`transactor` declarations), in file order.
    /// See `TransactorSchema`.
    pub transactors: Vec<TransactorSchema>,
    /// Scoreboard schemas (`scoreboard` declarations), in file order.
    /// See `ScoreboardSchema`.
    pub scoreboards: Vec<ScoreboardSchema>,
}

/// One `transactor` declaration, lowered to its structural shape.
///
/// Subset (the unbound DUT-poking BFM form): no generics, no
/// `bound to <BusType>` clause, exactly one module-typed field (the
/// DUT handle the methods drive), and `hookable`/`function` methods.
/// Each method body lowers to its own `TbFunction` with
/// `kind: TransactorBody` — the design doc's "one TbFunction per
/// method" rule, with v1's `field_subs` substitution replaced by
/// lowering-time resolution (the method body's DUT accesses resolve
/// against `dut_field` and lower to ordinary `PortRef`s).
///
/// The IR keeps the single-DUT model: the instance's `<inst>.dut = dut`
/// bind in the test body is validated at lowering (it must wire the
/// test's DUT) and then erased — the static bind is the schema itself.
#[derive(Debug, Clone)]
pub struct TransactorSchema {
    pub name: String,
    /// Name of the module-typed field the method bodies drive
    /// (`dut` by convention).
    pub dut_field: String,
    /// The field's declared module type. Cross-checked against the
    /// testbench's DUT type when an instance field is lowered.
    pub dut_type: String,
    /// Methods in declaration order (always-on body first, then the
    /// `when active` body — all methods require an `active` instance
    /// in this subset, so the distinction carries no behavior here).
    pub methods: Vec<TransactorMethodSchema>,
}

#[derive(Debug, Clone)]
pub struct TransactorMethodSchema {
    pub name: String,
    /// The lowered method body (`kind: TransactorBody`). Its `params`
    /// mirror the method's declared parameters and its `ret` is the
    /// return-value slot for `-> T` methods.
    pub function: FunctionId,
    /// Declared parameter count — duplicated from the function so call
    /// sites (which lower under a schema snapshot, without the
    /// functions table) can check arity.
    pub n_params: usize,
    /// True for `-> T` methods (the function carries a `ret` slot).
    pub has_ret: bool,
}

impl TransactorSchema {
    pub fn method(&self, name: &str) -> Option<&TransactorMethodSchema> {
        self.methods.iter().find(|m| m.name == name)
    }
}

/// One `transaction` declaration, lowered to its structural shape:
/// fields with names/types/defaults in declaration order. Both
/// backends emit this as a C++ value-record struct (v1's
/// `emit_record_struct` shape is the behavior reference).
///
/// Constraint metadata is **carried but inert**: `keeps` and the
/// per-field `attr_src` strings hold pretty-printed source text for
/// dump-ir/diagnostics only. When `randomize` lands, constraints will
/// NOT be lowered from these strings — the constraint-IR layer
/// (`src/constraints`, `elaborate_constraints` → `CTypedProblem`)
/// re-elaborates from the AST and the randomize terminator will carry
/// a `ConstraintRef` handle into that layer, per the design doc.
#[derive(Debug, Clone)]
pub struct RecordSchema {
    pub name: String,
    pub fields: Vec<RecordFieldSchema>,
    /// `keep <expr>` constraint clauses, pretty-printed. Inert (see
    /// the struct doc); randomize-free usage never evaluates them,
    /// which matches v1 (keeps emit zero C++ unless randomized).
    pub keeps: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RecordFieldSchema {
    pub name: String,
    /// Scalar field type. Lowering rejects non-scalar field types
    /// (nested records, enums, lists, vecs) and widths above 64 bits.
    pub ty: IrType,
    /// Declared `default <lit>` value (int/bool literals only), or
    /// `None` for the type-appropriate zero — same fallback as v1.
    pub default: Option<u64>,
    /// `!` prefix — pinned during randomization. Inert until the
    /// randomize slice lands; carried for the constraint seam.
    pub non_random: bool,
    /// `with [...]` field attributes, pretty-printed and inert (see
    /// `RecordSchema` doc — the constraint layer re-elaborates these
    /// from the AST when randomize lands).
    pub attr_src: Vec<String>,
}

impl RecordSchema {
    pub fn field(&self, name: &str) -> Option<&RecordFieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// One `scoreboard` declaration, lowered to its structural shape: a
/// host-state record of scalar counters and typed FIFO queues, in
/// declaration order. Both backends emit this as a C++ struct (v1's
/// `emit_scoreboard` shape is the behavior reference): scalar fields as
/// `uint64_t`/`int64_t`/`bool` members with their declared defaults,
/// `queue<T>` fields as `harc_rt::HarcQueue<T>` members.
///
/// Subset (v0): a scoreboard is a *testbench field* holding data only.
/// Scalar fields and `queue<T>` fields where `T` is a scalar ≤ 64 bits
/// lower; the test body manipulates them through `Stmt::ScoreboardOp`
/// (scalar read/write, queue push/pop/size/empty). Scoreboard
/// `hookable`/`function` methods — which mutate scoreboard instance
/// state and therefore need per-instance materialization — are NOT
/// lowered in this subset and are rejected at the call site with a
/// precise message. Event-driven `on`/`connect` wiring is likewise out
/// of scope (it gates on the agent/env/event slices).
#[derive(Debug, Clone)]
pub struct ScoreboardSchema {
    pub name: String,
    pub fields: Vec<ScoreboardFieldSchema>,
}

#[derive(Debug, Clone)]
pub struct ScoreboardFieldSchema {
    pub name: String,
    pub kind: ScoreboardFieldKind,
}

/// A scoreboard field is either a scalar counter or a typed FIFO queue.
#[derive(Debug, Clone)]
pub enum ScoreboardFieldKind {
    /// `writes : uint<32> default 0` — a scalar host counter. The
    /// `default` is the declared initializer literal (0 fallback, v1).
    Scalar { ty: IrType, default: u64 },
    /// `expected : queue<uint<32>>` — a FIFO of a scalar element type
    /// (≤ 64 bits; the lowered subset). `signed` selects the C element
    /// type for diagnostics/printf width (`int64_t` vs `uint64_t`).
    Queue { signed: bool },
}

impl ScoreboardSchema {
    pub fn field(&self, name: &str) -> Option<&ScoreboardFieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// One `test` (or `impl <Test> for <Tb>`) declaration.
#[derive(Debug, Clone)]
pub struct TestSchema {
    pub name: String,
    pub testbench: TestbenchId,
    pub run: FunctionId,
    pub check: Option<FunctionId>,
    /// Domain name of the primary clock (`clock clk = SysDomain` →
    /// `Some("SysDomain")`); `None` for time-literal periods or for
    /// clockless tests.
    pub clock_domain: Option<String>,
    /// All declared TB-driven clocks, in declaration order (the first
    /// is the primary). Empty for clockless tests. Carries the resolved
    /// period because codegen needs concrete picoseconds — the design
    /// doc's schema is extended here by compilation necessity.
    pub clocks: Vec<ClockSpec>,
}

/// One `clock <name> = <period-or-domain>` declaration, resolved.
#[derive(Debug, Clone)]
pub struct ClockSpec {
    /// DUT clock port name this generator drives (e.g. `clk`).
    pub name: String,
    /// Full period in picoseconds.
    pub period_ps: i64,
    /// Domain name when the period came from a `domain` declaration.
    pub domain: Option<String>,
}

/// The testbench a test runs against. For classic-form tests (no
/// `impl ... for Tb`) a synthetic schema is created so `TestSchema::
/// testbench` always resolves.
#[derive(Debug, Clone)]
pub struct TestbenchSchema {
    pub name: String,
    /// Test-scope field holding the DUT pointer — `"dut"` today.
    pub dut_field: String,
    /// Resolved DUT type name (string; HARC does not port-type, the
    /// DUT schema lives with the backend).
    pub dut_type: String,
    /// Covergroup-typed testbench fields (field name, covgroup).
    pub cov_fields: Vec<(String, CovgroupId)>,
    /// Scalar (integer/bool) testbench fields, in declaration order.
    /// These are run/check-shared host state living on the emitted
    /// `_tb` struct (v1's component-struct members), read via
    /// `Expr::TbField` and written via `Stmt::TbFieldWrite`.
    pub scalar_fields: Vec<TbScalarFieldSchema>,
    /// Test-scope bus bindings (`let <field> : <Bus> = bind dut`), in
    /// declaration order. Carried on the schema (like `cov_fields`)
    /// because `CallTarget::TransactorMethod { bus_field, .. }` call
    /// edges are deliberately NOT inlined at the IR level — emission
    /// resolves the edge against this table to learn the method's wire
    /// names. The design doc's skeleton hangs bus metadata off a
    /// `BusId` table; v0 inlines the (small) per-binding method list
    /// here instead since bindings are per-test, not global.
    pub bus_bindings: Vec<BusBindingSchema>,
    /// Transactor-typed testbench fields (field name, transactor), in
    /// declaration order. All instances are `active` in this subset.
    pub transactor_fields: Vec<(String, TransactorId)>,
    /// Scoreboard-typed testbench fields (field name, scoreboard), in
    /// declaration order. Each lowers to a default-constructed member of
    /// the `_tb` struct (v1's by-value scoreboard instance).
    pub scoreboard_fields: Vec<(String, ScoreboardId)>,
    /// True when no `testbench` declaration existed in source and this
    /// schema was synthesized for a classic-form test. Codegen skips
    /// the `_tb` struct + wire statement for synthetic testbenches.
    pub synthetic: bool,
}

/// One scalar testbench field (`expected : uint<32> default 0`).
/// Emitted as a member of the `_tb` struct with v1's C-type mapping
/// (bool → `bool`, signed → `int64_t`, unsigned → `uint64_t`).
#[derive(Debug, Clone)]
pub struct TbScalarFieldSchema {
    pub name: String,
    pub ty: IrType,
    /// Declared `default <lit>` value, or 0 (v1's fallback).
    pub default: u64,
}

/// One test-scope bus binding (`let axil : BusAxiLite = bind dut`).
/// The binding name doubles as the flat signal prefix on the DUT
/// (`axil` → `axil_aw_valid`, `axil_read_req_valid`, ...), mirroring
/// arch-com §19.6 / v1's `bus_bindings` convention.
#[derive(Debug, Clone)]
pub struct BusBindingSchema {
    /// Binding field name == flat DUT signal prefix.
    pub field: String,
    /// Bus type name (diagnostics + trace events).
    pub bus: String,
    /// `tlm_method` declarations on the bus, in declaration order.
    pub methods: Vec<TlmMethodSchema>,
}

/// One `tlm_method` on a bound bus — the call-edge metadata emission
/// needs to expand a `CallTarget::TransactorMethod` into the canonical
/// ARCH-compatible req/rsp wire protocol.
#[derive(Debug, Clone)]
pub struct TlmMethodSchema {
    pub name: String,
    /// Declared argument names, in order. Each maps to the request
    /// payload wire `<binding>_<method>_<arg>`.
    pub args: Vec<String>,
    /// True when the method declares a return type — the response
    /// carries a `<binding>_<method>_rsp_data` wire.
    pub has_ret: bool,
}

/// Function kinds. `Run`/`Check` are test phases; `SamplerAuto` is the
/// synthesized covergroup auto-sampler; `Helper` is a free function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionKind {
    Run,
    Check,
    SamplerAuto { covgroup: CovgroupId },
    Helper,
    /// One transactor method body (design doc §"Function-kind
    /// handling": one `TbFunction` per method). `params` are the
    /// method's declared parameters; the owning transactor and the
    /// method name live in `TbProgram::transactors[transactor]`.
    TransactorBody { transactor: TransactorId },
}

#[derive(Debug, Clone)]
pub struct TypedParam {
    pub name: String,
    pub ty: IrType,
}

#[derive(Debug, Clone)]
pub struct TypedLocal {
    /// Unique within the function (the lowering pass dedupes shadowed
    /// source names), so backends can use it as an identifier directly.
    pub name: String,
    pub ty: IrType,
}

/// IR-level value types. Widths are `Option` because HARC's v0 front
/// end does not type-check; `Unknown` is the common case today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrType {
    UInt(Option<u32>),
    SInt(Option<u32>),
    Bool,
    /// A transaction value-record local (`let t : TxnType`).
    Record(RecordId),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct TbFunction {
    pub id: FunctionId,
    pub name: String,
    pub kind: FunctionKind,
    /// Declared parameters. Convention: the first `params.len()` entries
    /// of `locals` mirror the params one-to-one (same order), so a
    /// `LocalId(i)` with `i < params.len()` *is* the i-th parameter.
    /// The verifier treats those locals as defined at entry.
    pub params: Vec<TypedParam>,
    pub locals: Vec<TypedLocal>,
    pub blocks: Vec<BasicBlock>,
    pub entry: BlockId,
    pub owner: Option<TestbenchId>,
    /// Return-value slot for `kind == Helper` functions with a declared
    /// return type: `return e` lowers to `Assign(ret, e); Return`, and
    /// the backend emits `return <ret>;` at `Terminator::Return`.
    /// `None` for run/check functions (their `Return` carries no value).
    pub ret: Option<LocalId>,
}

/// Straight-line statements + exactly one terminator. No statement may
/// suspend — sync points are `Terminator`s only (design invariant 7,
/// enforced by construction via this type split).
#[derive(Debug, Clone)]
pub struct BasicBlock {
    pub stmts: Vec<Stmt>,
    pub terminator: Terminator,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Assign(LocalId, Expr),
    DutWrite(PortRef, Expr),
    DutRead(LocalId, PortRef),
    /// (Re-)default-construct a record-typed local at its `let` site.
    /// Emitted as an explicit assignment (not just the hoisted
    /// declaration's initializer) because v1 declares the struct at
    /// the source `let` position — a `let t : Txn` inside a loop body
    /// re-runs the field defaults every iteration.
    RecordInit(LocalId, RecordId),
    /// `t.field = value` on a record-typed local. The value is
    /// port-hoisted like `Assign` (no inline DUT reads).
    RecordFieldWrite {
        local: LocalId,
        field: String,
        value: Expr,
    },
    /// `_tb.<field> = value` on a scalar testbench field (run/check-
    /// shared host state — see `TestbenchSchema::scalar_fields`). The
    /// value is port-hoisted like `Assign`.
    TbFieldWrite { field: String, value: Expr },
    Log { level: LogLevel, args: FmtArgs },
    AssertCheck { cond: Expr, on_fail: FmtArgs },
    CovReport(CovgroupInstance),
    /// A sequence→transactor method call — the Tier-1/Tier-0 placement
    /// seam. `call` is ALWAYS `Expr::Call(CallTarget::TransactorMethod
    /// { .. }, args)` (verifier-enforced) and is never inlined at the
    /// IR level. `dest` receives the method's return value
    /// (`let v = xact.m(...)`); `None` for void methods / discarded
    /// results.
    ///
    /// Suspension nuance: the callee follows v1's synchronous hookable
    /// model — its internal `wait`s advance the clock directly
    /// (`tick()`), so simulated time may pass inside the call, but the
    /// calling coroutine never yields to the scheduler. Invariant 7
    /// ("no statement may suspend") is about scheduler suspension, so
    /// a `Stmt` is the faithful shape.
    TransactorCall {
        dest: Option<LocalId>,
        call: Expr,
    },
    /// One `wait until … timeout` failure-diagnostic line: a
    /// `sim_log_line("FAIL", …)` that does NOT bump the error counter
    /// (v1 bumps `errors` exactly once per timed-out wait — that bump
    /// rides the `WaitUntilTimeout` terminator's timeout edge, not the
    /// per-line diagnostics). `guard: Some(pred)` prints only while
    /// the predicate is still false (`if (!(pred)) …` — the
    /// per-sub-predicate "not yet true:" breakdown); `guard: None`
    /// prints unconditionally (the header line). Lives only in
    /// `on_timeout` successor blocks.
    FailDiag { guard: Option<Expr>, args: FmtArgs },
    /// A statement-position scoreboard mutation on a scoreboard-typed
    /// testbench field (`sb.expected.push(x)`, `sb.writes = ...`). The
    /// design doc's `Stmt::ScoreboardOp(ScoreboardId, ScoreboardOp)`,
    /// extended with the resolved `field` (testbench-field name) so
    /// emission resolves the `_tb.<field>` member without a second
    /// lookup. Value-producing scoreboard reads (`.size()`, `.empty()`,
    /// `.pop()`, scalar reads) flow through `Expr::ScoreboardQuery` and
    /// are NOT statements (no side effect, except `.pop()` which is the
    /// `ScoreboardOp::QueuePop` form below — always assigned).
    ScoreboardOp {
        sb: ScoreboardId,
        field: String,
        op: ScoreboardOp,
    },
}

/// A scoreboard mutation. The design doc pins this as a tagged enum
/// "once semantics are pinned"; v0 covers exactly the ops the corpus
/// exercises on the data-only scoreboard subset.
#[derive(Debug, Clone)]
pub enum ScoreboardOp {
    /// `sb.<queue>.push(value)`. `queue` is the queue-field name.
    QueuePush { queue: String, value: Expr },
    /// `let v = sb.<queue>.pop()` — pop front into a local. Always has a
    /// destination (a bare `sb.q.pop()` discard is rejected at lowering,
    /// matching v1, which would warn on the unused value).
    QueuePop { queue: String, dest: LocalId },
    /// `sb.<scalar> = value` — write a scalar counter field.
    ScalarWrite { scalar: String, value: Expr },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
    /// `log(debug, ...)` — printed with a DEBUG tag; no test-result
    /// effect (v1 passes any severity ident through uppercased).
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    /// `logf("file", level, ...)` — write to a named file sink.
    File { path: String, level: FileLogLevel },
}

/// Severity inside a `logf` file sink (no nested `File`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileLogLevel {
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

/// A pre-parsed interpolated format string: `fmt` is the printf-style
/// format (produced by `cpp_tb::process_interp`), `args` are the lowered
/// `${...}` capture expressions in order.
#[derive(Debug, Clone)]
pub struct FmtArgs {
    pub fmt: String,
    pub args: Vec<FmtArg>,
}

#[derive(Debug, Clone)]
pub struct FmtArg {
    pub expr: Expr,
    /// `Some((hex_digit_width, uppercase))` when the capture used a
    /// `:WWx` / `:WWX` spec with WW > 16 — routed through the wide-hex
    /// runtime helper at emission. (The design skeleton's `bool` is
    /// widened here because emission needs the width and case.)
    pub wide_hex: Option<(usize, bool)>,
}

/// A reference to a covergroup instance held in a testbench field.
#[derive(Debug, Clone)]
pub struct CovgroupInstance {
    pub tb_field: String,
    pub covgroup: CovgroupId,
}

#[derive(Debug, Clone)]
pub enum Terminator {
    Jump(BlockId),
    Branch(Expr, BlockId, BlockId),
    /// Suspend for N clock cycles, then resume at the successor.
    /// `None` clock = the primary clock (plain `wait N cycles`);
    /// `Some(WaitClock)` = `wait N cycles on <clock>` — N rising edges
    /// of the named clock, with all other clocks ticking at their
    /// natural rate.
    WaitCycles(Expr, Option<WaitClock>, BlockId),
    /// A `wait N cycles` that originated inside an inlined helper /
    /// testbench-method body. v1 emits those bodies as plain lambdas
    /// whose waits run the synchronous `for (...) tick()` path — no
    /// coroutine yield — so a helpers-only test body completes inside
    /// `sched.bootstrap()`. The tbir backend mirrors that exactly
    /// (`tick()` advances the clock and runs the checkers inline);
    /// emitting `co_await` here instead leaves a observable trace
    /// delta in the final `sim_end` clock attribution.
    WaitCyclesSync(Expr, BlockId),
    /// Wall-clock wait (`wait 80ns`), duration resolved to picoseconds
    /// at lowering. Mirrors v1's inline `eval_clocks_until(now_ps + N)`
    /// emission (no coroutine yield, no checker pass) — only valid in
    /// tests with declared clocks (lowering enforces this; v1 emits
    /// uncompilable C++ for the clockless case).
    WaitTimePs(i64, BlockId),
    WaitUntil {
        preds: Vec<PredSrc>,
        mode: WaitMode,
        succ: BlockId,
    },
    WaitUntilTimeout {
        preds: Vec<PredSrc>,
        mode: WaitMode,
        cycles: Expr,
        on_fire: BlockId,
        on_timeout: BlockId,
    },
    Return,
    Fatal(FmtArgs),
}

/// Clock qualifier of a `wait N cycles on <clock>` suspension,
/// resolved at lowering against the test's declared clocks (an unknown
/// clock name is a lowering error, never a deferred codegen error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitClock {
    /// Declared clock name, as written after `on` (display text).
    pub name: String,
    /// Index into `TestSchema::clocks` — declaration order, which is
    /// also the runtime clock-scheduler (`clocks_`) index.
    pub index: usize,
}

/// One `wait until` sub-predicate plus its pretty-printed source text
/// (used in timeout diagnostics so logs name the user's expression).
#[derive(Debug, Clone)]
pub struct PredSrc {
    pub expr: Expr,
    pub src_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitMode {
    Single,
    AllOf,
    AnyOf,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Literal { value: u64, ty: IrType },
    /// An integer literal wider than 64 bits, as LSB-first 32-bit
    /// words (always > 2 words). General expression emission mirrors
    /// v1's `c_value_literal` (`_harc_u128` composite up to 128 bits,
    /// `harc_rt::HarcWide<N>` above); `DutWrite` values and `==`/`!=`
    /// against a port route through `harc_assign_words_checked` /
    /// `harc_eq_words` at emission, exactly like v1's special cases.
    WideLiteral(Vec<u32>),
    Local(LocalId),
    Port(PortRef),
    /// `t.field` read on a record-typed local. Host state, not DUT
    /// state — allowed in every expression position a `Local` is.
    RecordField { local: LocalId, field: String },
    /// `_tb.<field>` read on a scalar testbench field. Host state —
    /// allowed in every expression position a `Local` is.
    TbField(String),
    /// A value-producing scoreboard read on a scoreboard-typed testbench
    /// field: `sb.writes` (scalar), `sb.expected.size()`,
    /// `sb.expected.empty()`. Host state — allowed wherever a `Local`
    /// is. `.pop()` is NOT here (it mutates) — it lowers to
    /// `Stmt::ScoreboardOp { op: QueuePop, .. }`.
    ScoreboardQuery {
        sb: ScoreboardId,
        field: String,
        query: ScoreboardQuery,
    },
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    /// `cond ? a : b`. Both backends emit the C++ ternary; port reads
    /// inside follow the same position rules as any other subtree
    /// (hoisted everywhere except the port-allowed positions). Note
    /// the hoisted form evaluates both arms' port reads before
    /// selecting — port reads are side-effect-free, so this is
    /// observably identical to v1's lazy C++ ternary.
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// Width-method intrinsic (`.trunc<N>()` / `.zext<N>()` /
    /// `.sext<N>()` / `.resize<N>()`), width ≤ 64 in the lowered
    /// subset. `src_width` is the best-effort receiver width inferred
    /// at lowering (typed `let`, `as uint<W>` cast, nested width
    /// method, literal) — it selects v1's emission shape for
    /// `sext`/`resize` and `None` selects v1's unknown-width fallback.
    WidthCast {
        kind: WidthCastKind,
        width: u32,
        src_width: Option<u32>,
        inner: Box<Expr>,
    },
    CovBin {
        inst: CovgroupInstance,
        point: String,
        bin: String,
    },
    Call(CallTarget, Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthCastKind {
    Trunc,
    Zext,
    Sext,
    Resize,
}

/// A value-producing scoreboard read (see `Expr::ScoreboardQuery`).
#[derive(Debug, Clone)]
pub enum ScoreboardQuery {
    /// `sb.<scalar>` — read a scalar counter field.
    Scalar { scalar: String },
    /// `sb.<queue>.size()` — element count (lowers to `uint64_t`).
    QueueSize { queue: String },
    /// `sb.<queue>.empty()` — true when no elements (lowers to `bool`).
    QueueEmpty { queue: String },
}

#[derive(Debug, Clone)]
pub enum CallTarget {
    Helper(String),
    Builtin(String),
    TransactorMethod { bus_field: String, method: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
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
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

/// Structured handle to a DUT signal. Codegen never re-parses port
/// names. Direction/width are `Option` because the MVP lowering does
/// not consult a DUT port table yet.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PortRef {
    /// Test-scope field holding the DUT (`"dut"`).
    pub testbench_field: String,
    /// Dotted path below the DUT field (`["count_out"]`).
    pub port_path: Vec<String>,
    pub direction: Option<PortDirection>,
    pub width: Option<u32>,
    pub access: PortAccess,
    /// Constant lane index for `dut.<port>[i]` accesses (only literal
    /// indices are in the lowered subset). Emission routes lanes of
    /// packed multi-lane ports (the `--sv` `vec_lane_widths` table)
    /// through `harc_rt::harc_vec_lane_{read,write}<W>`, and true
    /// unpacked-array ports through a raw C++ subscript — the same
    /// split as v1's `dut_packed_lane`.
    pub lane: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortDirection {
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PortAccess {
    /// Architectural top-level port of the DUT.
    Port,
    /// DUT-internal signal surfaced via a declared probe; read-only.
    Probe,
    /// Wrapper-visible pin overridden via a declared force point; write-only.
    Force,
}

/// Covergroup schema: one `covergroup` declaration, lowered. Sampling
/// is schema-driven at emission (per-point bin counters plus the v1
/// auto-cross matrices); `FunctionKind::SamplerAuto` functions record
/// the per-testbench-field registration order.
#[derive(Debug, Clone)]
pub struct CovgroupSchema {
    pub name: String,
    pub trigger: CovTrigger,
    pub points: Vec<CoverPointSchema>,
    /// Declared `cross` items, in declaration order. Lowering resolved
    /// and validated the referenced coverpoints (2+ points, all with
    /// bins, bin product fits `usize`).
    pub crosses: Vec<CoverCrossSchema>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CovTrigger {
    PosedgeDutClk,
}

#[derive(Debug, Clone)]
pub struct CoverPointSchema {
    pub name: String,
    /// Sampled DUT signal (`cover dut.empty` → port `empty`). Lowering
    /// restricts targets to direct DUT port reads; the design doc's
    /// free-form `target: String` is structured here so emission never
    /// re-parses signal names.
    pub target: PortRef,
    pub bins: Vec<CoverBinSchema>,
}

#[derive(Debug, Clone)]
pub struct CoverBinSchema {
    pub name: String,
    pub values: Vec<CovBinValue>,
}

/// One member of a bin's value set: an exact value (`{3}`) or an
/// inclusive range (`[a..b]`, with either bound open). A bin spec like
/// `{[1..3], 7}` lowers to `[Range{1,3}, Eq(7)]`. Range bounds are
/// inclusive on both ends — v1's hit test is `_v >= lo && _v <= hi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CovBinValue {
    Eq(u64),
    Range { lo: Option<u64>, hi: Option<u64> },
}

/// One declared `cross` item, resolved against the owning schema's
/// `points`. Storage/label naming is derived at emission, mirroring
/// v1's `_cross_<item_index>_<p1>__<p2>` flat-array convention.
#[derive(Debug, Clone)]
pub struct CoverCrossSchema {
    /// Position of the `cross` among ALL of the covergroup's items
    /// (points and crosses) — v1's storage-name discriminator.
    pub item_index: usize,
    /// Indices into `CovgroupSchema::points`, in the order the cross
    /// names them. Always 2+ entries; every referenced point has bins.
    pub point_indices: Vec<usize>,
}

impl TbProgram {
    pub fn function(&self, id: FunctionId) -> &TbFunction {
        &self.functions[id.index()]
    }

    pub fn testbench(&self, id: TestbenchId) -> &TestbenchSchema {
        &self.testbenches[id.index()]
    }

    pub fn record(&self, id: RecordId) -> &RecordSchema {
        &self.records[id.index()]
    }

    pub fn transactor(&self, id: TransactorId) -> &TransactorSchema {
        &self.transactors[id.index()]
    }

    pub fn scoreboard(&self, id: ScoreboardId) -> &ScoreboardSchema {
        &self.scoreboards[id.index()]
    }
}

impl TbFunction {
    pub fn block(&self, id: BlockId) -> &BasicBlock {
        &self.blocks[id.index()]
    }

    pub fn local(&self, id: LocalId) -> &TypedLocal {
        &self.locals[id.index()]
    }
}

impl Terminator {
    /// Successor block ids, in a stable order.
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminator::Jump(b) => vec![*b],
            Terminator::Branch(_, t, f) => vec![*t, *f],
            Terminator::WaitCycles(_, _, b) => vec![*b],
            Terminator::WaitCyclesSync(_, b) => vec![*b],
            Terminator::WaitTimePs(_, b) => vec![*b],
            Terminator::WaitUntil { succ, .. } => vec![*succ],
            Terminator::WaitUntilTimeout {
                on_fire,
                on_timeout,
                ..
            } => vec![*on_fire, *on_timeout],
            Terminator::Return | Terminator::Fatal(_) => vec![],
        }
    }
}
