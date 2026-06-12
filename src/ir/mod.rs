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
    /// Test-scope bus bindings (`let <field> : <Bus> = bind dut`), in
    /// declaration order. Carried on the schema (like `cov_fields`)
    /// because `CallTarget::TransactorMethod { bus_field, .. }` call
    /// edges are deliberately NOT inlined at the IR level — emission
    /// resolves the edge against this table to learn the method's wire
    /// names. The design doc's skeleton hangs bus metadata off a
    /// `BusId` table; v0 inlines the (small) per-binding method list
    /// here instead since bindings are per-test, not global.
    pub bus_bindings: Vec<BusBindingSchema>,
    /// True when no `testbench` declaration existed in source and this
    /// schema was synthesized for a classic-form test. Codegen skips
    /// the `_tb` struct + wire statement for synthetic testbenches.
    pub synthetic: bool,
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
    Log { level: LogLevel, args: FmtArgs },
    AssertCheck { cond: Expr, on_fail: FmtArgs },
    CovReport(CovgroupInstance),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogLevel {
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
    Local(LocalId),
    Port(PortRef),
    /// `t.field` read on a record-typed local. Host state, not DUT
    /// state — allowed in every expression position a `Local` is.
    RecordField { local: LocalId, field: String },
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Unary(UnOp, Box<Expr>),
    CovBin {
        inst: CovgroupInstance,
        point: String,
        bin: String,
    },
    Call(CallTarget, Vec<Expr>),
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
