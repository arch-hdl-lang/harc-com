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
ir_id!(
    /// Index into `TbProgram::regblocks`.
    RegblockId
);
ir_id!(
    /// Index into `TbProgram::components`.
    ComponentId
);
ir_id!(
    /// Index into `TbProgram::constraint_sites` — the handle a
    /// `Terminator::Randomize` carries into the constraint-IR layer.
    /// See `ConstraintSite` and the `Terminator::Randomize` doc.
    ConstraintRef
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
    /// Register-block schemas (`regblock` declarations), in file order.
    /// See `RegblockSchema`.
    pub regblocks: Vec<RegblockSchema>,
    /// Composite-component schemas (the env/agent cluster's flat-struct
    /// subset: method-bearing scoreboards, analysis-source transactors,
    /// and the `env`s that compose them), in file order. See
    /// `ComponentSchema`.
    pub components: Vec<ComponentSchema>,
    /// Constraint-IR sites — the table a `Terminator::Randomize`'s
    /// `ConstraintRef` indexes. One entry per lowered `randomize(t)` /
    /// `randomize(t) with {...}` site. See `ConstraintSite`.
    pub constraint_sites: Vec<ConstraintSite>,
}

/// One `randomize` site, resolved into the constraint-IR layer.
///
/// This is the data a `Terminator::Randomize`'s `ConstraintRef` resolves
/// to. The design pins `ConstraintRef` as "a handle into the constraint
/// IR, not a copy"; `problem_id` IS that handle (an index into the
/// `build_typed_solver_problem_table` problem table). The remaining
/// fields are the *emission inputs* v1's constraint-solver codegen
/// already consumes (`emit_constraint_solver_block`): the source
/// `target` expression, the combined constraint set (transaction-level
/// `keep`s merged ahead of the call-site `with {...}` body, exactly as
/// v1 merges them), the record type name, and the blocking flag.
///
/// **Divergence from the AST-free ideal (documented):** the IR core
/// otherwise holds no `ast::Expr`. The randomize seam carries the AST
/// constraint expressions because v1's Z3 emission is AST-driven and the
/// slice reuses it verbatim ("the constraint runtime is shared; only the
/// call site moves to the IR backend") rather than reimplementing the
/// solver. See `docs/tbir-mvp.md` for the divergence record.
#[derive(Debug, Clone)]
pub struct ConstraintSite {
    /// Record type name (`transaction`/`struct`) of the target local.
    pub record: String,
    /// Source `randomize(t)` target expression — the join key v1 uses
    /// for the per-site solver cache tag and the problem-table lookup.
    pub target: crate::ast::Expr,
    /// Combined constraint set: transaction `keep`s first, then the
    /// call-site `with {...}` body (v1's merge order). Empty for a bare
    /// `randomize(t)` of a keep-free transaction (the unconstrained
    /// PRNG-shell path).
    pub constraints: Vec<crate::ast::Expr>,
    /// `randomize(t)` (false) vs blocking `randomize(t) with ...` (the
    /// AST `blocking` flag) — selects v1's queued vs blocking solve shell.
    pub blocking: bool,
    /// Problem-table handle (`ConstraintProblemId.0`) when the typed
    /// constraint IR built a Z3-ready problem for this site; `None` when
    /// the site has no solver problem (lower/backend error — v1 then
    /// emits the nullptr-descriptor fallback, mirrored here).
    pub problem_id: Option<u32>,
}

/// One `regblock` declaration, lowered to its register-level subset
/// (docs/tbir-mvp.md §regblock). The mirror is modeled as a synthetic
/// value-record (`record`, one scalar field per register), so the
/// existing `IrType::Record` / `RecordInit` / `RecordFieldWrite` /
/// `Expr::RecordField` machinery carries the host-side mirror state with
/// no new IR variants — exactly the shape v1's `<Name>_Mirror` POD
/// struct holds. The `registers` table carries the offset / width /
/// access metadata register-level access lowering needs.
///
/// Subset (the register-level frontdoor): `register NAME @ ADDR [reset
/// V] access rw|ro|wo`, accessed register-level (`regs.NAME = v`,
/// `let x = regs.NAME`) through the `via <Helper>` transactor's
/// `write(addr, data)` / `read(addr) -> data` methods. Field-level
/// decomposition, `bitbash`, the passive `record_write`/`record_read`
/// API, per-register `on` callbacks, and `addrmap` composition are
/// explicit `Unsupported` rejections (see `src/ir/lower/regblock.rs`).
/// The `via` helper must be an unbound DUT-poking transactor; the
/// bus-bound helper form is the documented residual blocker for the
/// corpus `regblock_*` fixtures.
#[derive(Debug, Clone)]
pub struct RegblockSchema {
    pub name: String,
    /// Mirror record: the synthetic `RecordSchema` (in `TbProgram::
    /// records`) whose fields are the registers in declaration order,
    /// each defaulting to its reset value.
    pub record: RecordId,
    /// Registers in declaration order.
    pub registers: Vec<RegRegisterSchema>,
}

#[derive(Debug, Clone)]
pub struct RegRegisterSchema {
    pub name: String,
    /// Byte offset within the regblock (folded from the `@ <addr>`
    /// literal at lowering).
    pub offset: u64,
    /// Register width in bits (explicit `width` or the regblock default).
    pub width: u32,
    pub access: RegAccess,
    /// Named bit-fields declared inside the register (`field N : T @
    /// <pos>`), in declaration order. Empty for the single-line register
    /// form. Field-level access (`regs.REG.FIELD`) lowers to a masked
    /// read-modify-write on the whole-register mirror cell plus
    /// full-register bus traffic, mirroring v1's bit-slice
    /// extract/insert. See `src/ir/lower/regblock.rs`.
    pub fields: Vec<RegFieldSchema>,
}

/// One named bit-field inside a register (`field NAME : T @ <pos>
/// [access P]`). The mirror stays whole-register; this carries the
/// mask/shift metadata the field-level access lowering needs.
#[derive(Debug, Clone)]
pub struct RegFieldSchema {
    pub name: String,
    /// LSB bit position inside the parent register.
    pub bit_pos: u32,
    /// Field width in bits (derived from the field type).
    pub bit_width: u32,
    /// Field access policy (the field's own, defaulting to the
    /// register's policy when the field decl omits an `access` clause).
    pub access: RegAccess,
}

/// Register access policy, mirroring `ast::RegAccess`. Duplicated into
/// the IR so the IR stays self-contained (backends never reach back into
/// the AST). v1's `writes_to_bus` / `reads_from_bus` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegAccess {
    Rw,
    Ro,
    Wo,
}

impl RegAccess {
    /// Whether a write to this register reaches the bus (RW/WO).
    pub fn writes_to_bus(self) -> bool {
        matches!(self, RegAccess::Rw | RegAccess::Wo)
    }
    /// Whether a read of this register reaches the bus (RW/RO).
    pub fn reads_from_bus(self) -> bool {
        matches!(self, RegAccess::Rw | RegAccess::Ro)
    }
    pub fn keyword(self) -> &'static str {
        match self {
            RegAccess::Rw => "rw",
            RegAccess::Ro => "ro",
            RegAccess::Wo => "wo",
        }
    }
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
    /// `Some(<BusType>)` for a target-side TLM transactor declared
    /// `transactor X bound to <BusType>`; `None` for the unbound
    /// DUT-poking BFM form. When set, `target_methods` carries the
    /// `thread bus.<method>(...)` responder bodies and `dut_field`/
    /// `dut_type` are unused (target transactors drive the bound bus's
    /// req/rsp wires on the DUT instance, not a private DUT handle).
    pub bound_bus: Option<String>,
    /// Persistent scalar state fields (`read_count : uint<32> default
    /// 0`) of a bound-to target transactor. They live on a generated
    /// per-instance C++ struct (mirroring v1's component-instance
    /// struct), readable/writable from the responder bodies and from
    /// the test (`target.read_count`). Empty for the unbound BFM form
    /// (whose state fields are still rejected in that path).
    pub state_fields: Vec<TbScalarFieldSchema>,
    /// Target-side TLM responder threads (`thread bus.<method>(...)`),
    /// in declaration order. Each has one lowered `TbFunction` body
    /// whose state-field accesses reference `state_fields` by bare name;
    /// the actor-emission site resolves them against the bound instance.
    pub target_methods: Vec<TargetTlmMethodSchema>,
}

/// One target-side TLM responder thread (`thread bus.read(addr) ...
/// return data`). The body runs as a background coroutine actor that
/// serves the bound bus's blocking req/rsp wire protocol on the DUT.
#[derive(Debug, Clone)]
pub struct TargetTlmMethodSchema {
    /// `tlm_method` name on the bound bus (`read`, `write`, ...).
    pub name: String,
    /// The lowered responder body (`kind: TransactorBody`). Its
    /// `params` mirror the thread's declared parameters and its `ret`
    /// is the return-value slot for value-returning methods.
    pub function: FunctionId,
    /// Declared argument names (one per thread parameter), in order —
    /// the request-payload wire bases (`<bus>_<method>_<arg>`).
    pub args: Vec<String>,
    /// True for value-returning methods (`-> T`): the responder drives
    /// a `<bus>_<method>_rsp_data` wire after the body returns.
    pub has_ret: bool,
    /// `None` for a `blocking` `tlm_method` (single in-order responder
    /// coroutine, issue-order `req_tag`/`rsp_tag` unused). `Some(N)` for
    /// an `out_of_order tags N` method: emission generates the multi-lane
    /// RESPONDER topology mirroring v1's `emit_bound_tagged_tlm_target_actors`
    /// — a per-tag dispatcher (combinational `req_ready` accept + lane
    /// hand-off), N concurrent lane coroutines (each runs the responder
    /// body), and an arbiter routing each lane's response back on the
    /// hidden `req_tag`/`rsp_tag` wires. The lowered body `function` is
    /// identical to the blocking form; only the surrounding actor
    /// topology differs. The folded count is range-checked at lowering
    /// (1..=64), matching v1.
    pub ooo_tags: Option<u64>,
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
    /// Test-scope `on <obj>.<method> pre` hook bodies, registration
    /// order. Each is a `FunctionKind::TransactorBody` function sharing
    /// the method's parameter signature (the hook sees the same args).
    /// The hook bodies mutate promoted host state (`_tb` scalar fields)
    /// by reference and read the firing instance's transactor state
    /// (`drv.last_read`); host-state promotion (a captured run-scope
    /// `let` becomes a `_tb` field) is what lets the function-per-CFG IR
    /// express v1's `[&]`-capturing hook closure. Fired BEFORE the body
    /// at the `emit_method` call site (mirrors v1's `<Type>_<method>_pre`
    /// fan-out loop). Empty for a method with no registered hooks.
    pub pre_hooks: Vec<FunctionId>,
    /// Test-scope `on <obj>.<method> post` hook bodies — fired AFTER the
    /// body. See `pre_hooks`.
    pub post_hooks: Vec<FunctionId>,
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
    /// Field element type. For a scalar field this is the field's own
    /// scalar type; for a `Vec<T, N>` field (`vec_len = Some(N)`) it is
    /// the *element* scalar type `T` (its width drives both the C++
    /// storage type and the packed-bit layout). Lowering rejects field
    /// element types that are not scalar uint/sint/bits/bool/bit ≤ 64
    /// bits (nested records, enums, lists).
    pub ty: IrType,
    /// `Some(N)` when the field is a fixed-size `Vec<T, N>` aggregate
    /// (v1's `std::array<T, N>` record member); `None` for a scalar
    /// field. The element type/width is carried in `ty`.
    pub vec_len: Option<usize>,
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
    /// `expected : queue<uint<32>>` / `errors : queue<CheckerError>` — a
    /// FIFO whose element is a scalar ≤ 64 bits or a value-record.
    Queue { elem: QueueElem },
}

/// The element type of a `queue<T>` field, mirroring `EventPayload`:
/// a scalar ≤ 64 bits, or a value-record carried by struct. Shared by
/// scoreboard and composite-component queue fields so both lower a
/// `queue<Record>` element through one shape (`harc_rt::HarcQueue<Rec>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueElem {
    /// `queue<uint<32>>` / `queue<sint<…>>` / `queue<bool>` — a scalar
    /// ≤ 64 bits. `signed` selects the C element type (`int64_t` vs
    /// `uint64_t`) and printf width.
    Scalar { signed: bool },
    /// `queue<CheckerError>` — a value-record element. `RecordId` indexes
    /// `TbProgram::records`; the C++ element type is the record struct.
    Record(RecordId),
}

impl ScoreboardSchema {
    pub fn field(&self, name: &str) -> Option<&ScoreboardFieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// One composite-component declaration in the env/agent cluster, lowered
/// to v1's flat-struct + free-lambda-method shape
/// (`emit_component_struct` / `emit_component_method`). Three source
/// shapes lower into this one schema in the v0 env-composition subset
/// (docs/tbir-mvp.md §env/agent):
///
/// - a `scoreboard` that carries `hookable`/`function` **methods** (the
///   data-only scoreboard subset lowers method-less boards through
///   `ScoreboardSchema` instead — a method-bearing board needs per-
///   instance state materialized, which is exactly what a component
///   struct provides);
/// - a `transactor` used purely as an **analysis source** — one `out
///   event<T>` port plus `hookable`/`function` methods that `emit` on it
///   (the DUT-poking transactor subset stays on `TransactorSchema`);
/// - an `env` that **composes** the two as by-value sub-component fields
///   and `connect`s a source's event port to a sink's method.
///
/// Each becomes a C++ struct of its fields (scalar / `queue<T>` /
/// `event<T>` callback vector / nested sub-component) plus the v1
/// `_last_in_cycle`/`_last_out_cycle` heartbeat stamps, and one
/// free `<Comp>_<method>(<Comp>& self, args)` lambda per method whose
/// body resolves bare field names to `self.<field>`.
///
/// **Out of subset** (rejected, never mis-lowered): `agent` declarations
/// and `on <ev>` event handlers, `sequencer`/`tseq`, watchdog/phase
/// orchestration, `idle(N)`/`quiesced(N)` predicates, bus-bound source
/// transactors, generics, and event ports carrying non-scalar payloads
/// (`event<Struct>`). Those gate on the agent/sequencer/event slices.
#[derive(Debug, Clone)]
pub struct ComponentSchema {
    pub name: String,
    /// Source keyword, for diagnostics + dump-ir (`env`/`scoreboard`/
    /// `transactor`).
    pub kind: ComponentKindTag,
    pub fields: Vec<ComponentFieldSchema>,
    /// Methods in declaration order. Each has one lowered `TbFunction`
    /// (`kind: ComponentMethod`) whose body addresses fields self-
    /// relatively (`Expr::ComponentField`/`Stmt::ComponentFieldWrite`
    /// with `ComponentBase::SelfField`).
    pub methods: Vec<ComponentMethodSchema>,
    /// `connect <src>.<event> -> <sink>.<method>` edges (env only; empty
    /// for scoreboard/transactor components). Resolved against this env's
    /// sub-component fields; emission wires them at env construction.
    pub connects: Vec<ConnectEdgeSchema>,
    /// `on <ev>(arg) ... end on` event handlers (agent subset; empty for
    /// env/scoreboard/transactor). Each subscribes to a self-event field;
    /// registered at component construction as a `push_back` closure that
    /// bumps `_last_in_cycle` and runs the handler body. The handler body
    /// is lowered as a one-param `ComponentMethod` function.
    pub on_handlers: Vec<OnHandlerSchema>,
    /// `on <N> cycles ... end on` periodic handlers (agent subset). Each
    /// fires its body once every `period` primary-clock cycles, dispatched
    /// from a `_checkers` closure that gates on a per-instance last-fire
    /// stamp. The body is lowered as a zero-arg `ComponentMethod`
    /// function (`self` only); bare field reads resolve self-relatively.
    pub periodic_handlers: Vec<PeriodicHandlerSchema>,
    /// `on <bool-expr> ... end on` cycle-trigger handlers (monitor /
    /// observer half of an agent-mode transactor). Each fires its body
    /// once per primary-clock cycle that the trigger predicate satisfies
    /// the requested edge mode (rising/falling/level), dispatched from a
    /// `_checkers` closure with per-instance static prev-state. The body
    /// is lowered as a zero-arg `ComponentMethod` function (`self` only);
    /// bare field reads resolve self-relatively, `dut.<sig>` reads resolve
    /// to the DUT handle, and `sb.<f>` writes resolve to the sub-scoreboard.
    /// Always-on: present on BOTH active and passive instances (it is the
    /// observation half, unaffected by `when active` mode elision).
    pub cycle_handlers: Vec<CycleTriggerHandlerSchema>,
    /// `watchdog ... end watchdog` lifecycle directive (spec §8.6), at
    /// most one per component. `None` when the component declares none (or
    /// declares `watchdog disabled` — opt-out suppresses all codegen).
    /// Dispatched from a `_checkers` closure that gates on a per-instance
    /// last-fire stamp, runs the user body, then asserts the component has
    /// NOT been idle (`_last_in_cycle`/`_last_out_cycle`) for `max_idle`
    /// cycles. `None` means no watchdog.
    pub watchdog: Option<WatchdogSchema>,
    /// `transactor X bound to <Bus>` — the bus this event-driven
    /// transactor's `on <ev>` handler bodies drive (via `bus.<ch>.send/
    /// recv`/`<ch>.<sig>` handshake accesses, CFG-inlined exactly like the
    /// bound-initiator BFM). `None` for env/scoreboard/agent/sequencer and
    /// for an UNBOUND event-driven transactor (which pokes a private DUT
    /// handle instead). When `Some`, the handler bodies carry the
    /// placeholder bus prefix (`transactors::INITIATOR_BUS_PLACEHOLDER`),
    /// filled with the real binding name at test-binding time.
    pub bound_bus: Option<String>,
}

/// One `on <N> cycles ... end on` periodic handler (spec §7.10). Fires
/// its body once every `period` primary-clock cycles. The codegen
/// installs a `_checkers` closure that compares `cycle_count` against a
/// per-instance last-fire stamp; the period is re-read each cycle so a
/// field-backed period can be overridden from the test scope.
#[derive(Debug, Clone)]
pub struct PeriodicHandlerSchema {
    /// Firing period in primary-clock cycles. Lowered in the component's
    /// `SelfField` context (a field-backed period reads `self.<field>`).
    pub period: Expr,
    /// Lowered handler body (`kind: ComponentMethod`, zero params — `self`
    /// only). Bare field names resolve self-relatively.
    pub function: FunctionId,
    /// `phase` modifier (`on N cycles phase post_eval`). `Checker` (default)
    /// dispatches from the per-cycle `_checkers` vector; `PostEval`
    /// dispatches from `_post_eval_services` (after the DUT posedge eval, so
    /// the body observes freshly-clocked DUT outputs in the same cycle).
    pub phase: HandlerPhase,
}

/// One `on <bool-expr> ... end on` cycle-trigger handler (spec §7.x
/// monitor form). The trigger predicate is evaluated every primary-clock
/// cycle in a `_checkers` closure; the body fires when the predicate
/// satisfies the handler's edge mode. Mirrors v1's `emit_cycle_trigger`.
#[derive(Debug, Clone)]
pub struct CycleTriggerHandlerSchema {
    /// The boolean trigger predicate, lowered in the component's
    /// `SelfField` context (so `dut.<sig>` reads route to the DUT handle
    /// and bare field reads resolve self-relatively). Rendered standalone
    /// inside the per-instance `_checkers` closure.
    pub trigger: Expr,
    /// Edge mode: `Rising` (0→1, default), `Falling` (1→0), or `Level`
    /// (every cycle the predicate holds).
    pub edge: CycleEdge,
    /// Lowered handler body (`kind: ComponentMethod`, zero params — `self`
    /// only).
    pub function: FunctionId,
    /// `Some(channel)` when this cycle-trigger handler is the desugared
    /// form of an `on bus.<ch>.handshake(arg)` passive bus-monitor handler
    /// on a `bound to <Bus>` transactor (v1's `emit_bound_monitor_actors`).
    /// The synthesized `trigger` is the channel's `valid && ready` (rising
    /// edge); the body's lowered preamble captures the channel payload into
    /// the handler's `arg` local (+ per-field aliases) so `arg`/`arg.<f>`
    /// reads resolve, then runs the user body (`sb.<q>.push(arg.<f>)`).
    /// `None` for an agent-mode `on <bool-expr>` cycle-trigger (which reads
    /// `dut.<sig>` directly). The placeholder bus prefix in the trigger +
    /// body is filled with the real binding name at test-binding time, like
    /// the bound-bus driver. Always-on: present on BOTH active and passive
    /// instances (the observation half, unaffected by `when active` mode).
    pub monitor_channel: Option<String>,
}

/// Edge mode for a cycle-trigger handler — mirrors `ast::EdgeMode` in the
/// IR so the codegen needn't reach back into the AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleEdge {
    Rising,
    Falling,
    Level,
}

/// Scheduling phase for an `on` handler — mirrors `ast::OnPhase` in the IR
/// so the codegen needn't reach back into the AST. `Checker` registers the
/// handler in the per-cycle `_checkers` vector (run after the falling edge
/// has re-settled comb logic); `PostEval` registers it in the
/// `_post_eval_services` vector, run after the DUT posedge `eval` and
/// before the run coroutine resumes — the seam that lets a checker observe
/// freshly-clocked DUT outputs in the same cycle the test set its inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerPhase {
    Checker,
    PostEval,
}

impl HandlerPhase {
    /// Lower from the AST `OnPhase`.
    pub fn from_ast(p: crate::ast::OnPhase) -> Self {
        match p {
            crate::ast::OnPhase::Checker => HandlerPhase::Checker,
            crate::ast::OnPhase::PostEval => HandlerPhase::PostEval,
        }
    }

    /// The C++ dispatch vector this phase registers into.
    pub fn service_vec(self) -> &'static str {
        match self {
            HandlerPhase::Checker => "_checkers",
            HandlerPhase::PostEval => "_post_eval_services",
        }
    }
}

/// A `watchdog ... end watchdog` directive (spec §8.6). At most one per
/// component. `period`/`max_idle` lower in the component's `SelfField`
/// context (field-backed clauses read `self.<field>`); both default to
/// `None` (the codegen substitutes the spec defaults: 1000 / 10000
/// cycles). The body runs BEFORE the idle check on each firing
/// (conventionally a debug `log`).
#[derive(Debug, Clone)]
pub struct WatchdogSchema {
    /// `period <expr> cycles` — firing cadence. `None` → default 1000.
    pub period: Option<Expr>,
    /// `max_idle <expr> cycles` — idle threshold that trips the FAIL
    /// diagnostic. `None` → default 10000.
    pub max_idle: Option<Expr>,
    /// Lowered watchdog body (`kind: ComponentMethod`, zero params —
    /// `self` only), run before the idle check on each firing.
    pub function: FunctionId,
}

impl ComponentSchema {
    pub fn field(&self, name: &str) -> Option<&ComponentFieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }
    pub fn method(&self, name: &str) -> Option<&ComponentMethodSchema> {
        self.methods.iter().find(|m| m.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKindTag {
    Env,
    Scoreboard,
    Transactor,
    /// `agent` — composes event handlers (`on <ev>`) into a self-
    /// subscribing component. Same flat-struct shape as env/scoreboard;
    /// the distinguishing capability is on-handler registration.
    Agent,
    /// `sequencer` — generates a stimulus stream. Same flat-struct shape
    /// as the analysis-source transactor (`out event<T>` ports + hookable
    /// methods that `emit` on them); a `connect` edge feeds the emitted
    /// stream into a driver's sink method (the UVM sequencer/driver
    /// pattern). It carries no DUT field of its own.
    Sequencer,
}

impl ComponentKindTag {
    pub fn keyword(self) -> &'static str {
        match self {
            ComponentKindTag::Env => "env",
            ComponentKindTag::Scoreboard => "scoreboard",
            ComponentKindTag::Transactor => "transactor",
            ComponentKindTag::Agent => "agent",
            ComponentKindTag::Sequencer => "sequencer",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComponentFieldSchema {
    pub name: String,
    pub kind: ComponentFieldKind,
}

#[derive(Debug, Clone)]
pub enum ComponentFieldKind {
    /// `count : uint<32> default 0` — a scalar host counter.
    Scalar { ty: IrType, default: u64 },
    /// `expected : queue<uint<32>>` / `errors : queue<CheckerError>` — a
    /// FIFO whose element is a scalar ≤ 64 bits or a value-record (`elem`
    /// selects the C element type). Manipulated through the component-queue
    /// ops (`Stmt::ComponentQueuePush`/`ComponentQueuePop`,
    /// `Expr::ComponentQueueSize`).
    Queue { elem: QueueElem },
    /// `observed : out event<uint<8>>` — an analysis port. Lowers to a
    /// `std::vector<std::function<void(<payload>)>>` member; `payload`
    /// selects the C payload type (scalar `uint64_t`/`int64_t` or a
    /// value-record struct). Sinks subscribe via `connect`.
    Event { payload: EventPayload },
    /// `source : AnalysisSource passive` / `sb : AnalysisSb` — a nested
    /// by-value sub-component. `component` indexes
    /// `TbProgram::components`.
    Sub { component: ComponentId },
    /// `dut : AxiLiteRegs` — the module-typed DUT handle field on an
    /// event-driven transactor (consumer side). Lowers to a
    /// `V<dut_type>* <name> = nullptr;` pointer member; the test binds it
    /// (`drv.dut = dut`) and the `on <ev>` handler body pokes DUT signals
    /// through it. `dut_type` is the SV module name (e.g. `AxiLiteRegs`).
    Dut { dut_type: String },
    /// `sb : DrainSb` where `DrainSb` is a DATA-ONLY `scoreboard` (no
    /// methods) — a nested by-value scoreboard sub-component. `scoreboard`
    /// indexes `TbProgram::scoreboards`. Distinct from `Sub` because a
    /// data-only board lowers to a `ScoreboardSchema`, not a
    /// `ComponentSchema`; it is always a quiesce LEAF (it has no further
    /// sub-components). Mirrors v1, where a `scoreboard` is held by value
    /// inside the env struct and carries the `_last_in/out_cycle` stamps.
    ScoreboardSub { scoreboard: ScoreboardId },
}

/// The payload carried by an `event<T>` analysis port (and the matching
/// subscriber-closure / `on`-handler argument). Mirrors v1's
/// `payload_type_for_arg`: a scalar widens to `uint64_t`/`int64_t`; a
/// user-named `transaction`/`struct` payload is carried by value as the
/// record struct (`std::function<void(<RecordName>)>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPayload {
    /// `event<uint<8>>` / `event<sint<…>>` / `event<bool>` — a scalar
    /// ≤ 64 bits. `signed` selects `int64_t` vs `uint64_t`.
    Scalar { signed: bool },
    /// `event<TinyTxn>` — a value-record payload. `RecordId` indexes
    /// `TbProgram::records`; the C++ payload type is the record struct.
    Record(RecordId),
}

#[derive(Debug, Clone)]
pub struct ComponentMethodSchema {
    pub name: String,
    /// Lowered method body (`kind: ComponentMethod`).
    pub function: FunctionId,
    pub n_params: usize,
    pub has_ret: bool,
    /// True for `hookable` (emits pre/post hook vectors + fan-out);
    /// false for `function` (no hooks). Mirrors v1's
    /// `HookableMethod::is_hookable`.
    pub hookable: bool,
}

/// One `on <event>(arg) ... end on` handler on an agent (or other
/// component). The handler subscribes to the component's own `event`
/// field; on every `emit`/`connect` fan-out into that field, the
/// framework bumps `_last_in_cycle` (activity tracking) then invokes
/// `<Comp>_on<idx>(self, arg)`. `arg_payload` selects the C payload
/// type for the subscriber closure parameter, mirroring the event
/// field's `ComponentFieldKind::Event { payload }`.
#[derive(Debug, Clone)]
pub struct OnHandlerSchema {
    /// The self-event field this handler subscribes to.
    pub event: String,
    /// Payload shape of the subscribed event (matches the field).
    pub arg_payload: EventPayload,
    /// Lowered handler body (`kind: ComponentMethod`, exactly one param
    /// = the event argument).
    pub function: FunctionId,
}

/// One `connect <src>.<event> -> <sink>.<method|event>` edge inside an
/// env, resolved to the paths the test uses to reach the sub-components.
/// Two sink shapes (see `ConnectSink`):
///   * method sink — `<env>.<src_path>.<event>.push_back([&](auto _t){
///     <SinkComp>_<method>(<env>.<sink_path>, _t); })`
///   * event sink — `<env>.<src_path>.<event>.push_back([&](auto _t){
///     for (auto& _s : <env>.<sink_path>.<event>) _s(_t); })` (forwards
///     into the sink event field's own subscriber list, i.e. an
///     event→event bridge feeding a driver's `in event` handler).
#[derive(Debug, Clone)]
pub struct ConnectEdgeSchema {
    /// Dotted path from the env-bound test field to the source sub-
    /// component (e.g. `["source"]` for `env.source`).
    pub src_path: Vec<String>,
    /// `out event<T>` field on the source sub-component.
    pub src_event: String,
    /// Dotted path to the sink sub-component (`["sb"]`).
    pub sink_path: Vec<String>,
    /// Sink sub-component's component schema (to name `<Comp>_<method>`).
    pub sink_component: ComponentId,
    /// What the edge feeds on the sink sub-component.
    pub sink: ConnectSink,
}

/// The sink end of a `connect` edge.
#[derive(Debug, Clone)]
pub enum ConnectSink {
    /// `-> sb.write_obs` — a hookable sink method. Emission calls
    /// `<SinkComp>_<method>(<sink>, _t)`.
    Method { method: String },
    /// `-> drv.req` — an `in event<T>` field on the sink (an
    /// event-driven transactor's input pipe). Emission fans out over the
    /// sink event's own subscriber list, driving its registered
    /// `on <ev>` handler(s).
    Event { event: String },
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
    /// Register-block bindings (`let regs : R = bind <helper>`), in
    /// declaration order. Carried for dump-ir visibility; register
    /// access is fully resolved at lowering into mirror
    /// `RecordFieldWrite` / `Expr::RecordField` plus helper
    /// `Stmt::TransactorCall` edges, so emission needs nothing here.
    pub regblock_bindings: Vec<RegblockBinding>,
    /// Bound-to target-side TLM responder actors (`let target : X
    /// passive = bind <busbinding>`), in declaration order. Each names a
    /// passive instance of a `transactor X bound to <Bus>` transactor
    /// and the test-scope bus binding it serves; emission generates a
    /// per-instance state struct plus one background-coroutine actor per
    /// target method.
    pub target_tlm_actors: Vec<TargetTlmActorSchema>,
    /// Composite-component-typed test fields (`let env : AnalysisEnv` /
    /// `env : AnalysisEnv` testbench field), in declaration order. Each
    /// names a test-scope local that is a default-constructed instance of
    /// `TbProgram::components[component]`. Unlike scoreboards/transactors
    /// these are NOT held on `_tb` — they are emitted as plain run-scope
    /// locals (v1's `AnalysisEnv env;`), so `connect` push_backs and
    /// method calls work against the run function's `env`.
    pub component_fields: Vec<ComponentFieldBinding>,
    /// Unbound DUT-poking transactor instances that carry persistent
    /// scalar state fields (`drv : SeqXactor active` where `SeqXactor`
    /// has a `last_read : uint<32>` field), in declaration order. Each
    /// names an entry in `transactor_fields`; emission generates a
    /// per-instance state struct (mirroring the bound-to target form's
    /// `target_state_struct_inst`) that the method lambdas and the
    /// run/check coroutine share by `[&]` capture. Stateless unbound
    /// transactors (no state fields) are absent here — their methods are
    /// pure DUT-poking lambdas with no per-instance struct.
    pub unbound_state_actors: Vec<(String, TransactorId)>,
    /// True when no `testbench` declaration existed in source and this
    /// schema was synthesized for a classic-form test. Codegen skips
    /// the `_tb` struct + wire statement for synthetic testbenches.
    pub synthetic: bool,
}

/// One composite-component test field binding (`let env : AnalysisEnv`).
#[derive(Debug, Clone)]
pub struct ComponentFieldBinding {
    /// Test-scope local name (`env`).
    pub field: String,
    /// The env/component type instantiated.
    pub component: ComponentId,
    /// `connect` edges from the env declaration, resolved to paths. Empty
    /// for non-env components (only `env` carries a `connect` block).
    pub connects: Vec<ConnectEdgeSchema>,
}

/// One bound-to target-side TLM responder instance (`let target :
/// MemTarget passive = bind mem`). The actor coroutines serve the bound
/// bus binding's blocking req/rsp wire protocol on the DUT.
#[derive(Debug, Clone)]
pub struct TargetTlmActorSchema {
    /// Passive instance name (the per-instance struct + actor prefix).
    pub instance: String,
    /// The test-scope bus-binding field this responder serves — also
    /// the flat DUT signal prefix (`mem` → `mem_read_req_valid`, ...).
    pub bus_field: String,
    /// The bound-to transactor type providing the responder bodies and
    /// state fields (`TbProgram::transactors[transactor]`).
    pub transactor: TransactorId,
}

/// One `let regs : R = bind <helper>` register-block binding. The
/// binding name `field` is the mirror local's source name; `regblock`
/// indexes `TbProgram::regblocks`; `helper_field` is the transactor
/// instance the frontdoor `write`/`read` calls route through.
#[derive(Debug, Clone)]
pub struct RegblockBinding {
    pub field: String,
    pub regblock: RegblockId,
    pub helper_field: String,
    /// Per-register `on regs.REG` write callbacks: `(register-name,
    /// callback-function)`, registration order. Each callback is a
    /// one-param (`data : uint`) `FunctionKind::TransactorBody` function;
    /// `record_write` fires the matching register's callback after the
    /// mirror update, with a per-binding recursion-depth guard (v1's
    /// `<regs>_cb_depth` / `HARC_RAL_CB_MAX_DEPTH`). Empty when the test
    /// registers no callbacks on this binding (then `record_write` emits a
    /// plain mirror `RecordFieldWrite` with no guard, the prior behavior).
    pub callbacks: Vec<(String, FunctionId)>,
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
    /// Per-signal flat-name overrides from `bind ... with { ch.sig:
    /// "port", ... }` (v1's `bus_remap`). Each entry is `((channel,
    /// signal), flat_port_name)`; for a `tlm_method` the channel is the
    /// method name and the signal is a protocol wire (`req_valid`,
    /// `addr`, `rsp_data`, ...). Unmapped signals fall back to the
    /// `<field>_<channel>_<signal>` convention. Sorted by key for
    /// deterministic dump-ir / snapshot output.
    pub remap: Vec<((String, String), String)>,
}

impl BusBindingSchema {
    /// Resolve the flat DUT port name for `<channel>.<signal>` on this
    /// binding: the `bind ... with` override if present, else the
    /// `<field>_<channel>_<signal>` convention (mirrors v1's
    /// `bus_signal_name`).
    pub fn wire_name(&self, channel: &str, signal: &str) -> String {
        for ((ch, sig), port) in &self.remap {
            if ch == channel && sig == signal {
                return port.clone();
            }
        }
        format!("{}_{channel}_{signal}", self.field)
    }
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
    /// One composite-component method body (env/agent cluster). `params`
    /// are the method's declared parameters; bodies address fields self-
    /// relatively via `ComponentBase::SelfField`. The owning component +
    /// method name live in `TbProgram::components[component]`. Emitted as
    /// a `<Comp>_<method>(<Comp>& self, args)` free lambda (v1's
    /// `emit_component_method` shape).
    ComponentMethod { component: ComponentId },
    /// One `tseq` declaration body — a transaction-sequence generator.
    /// `record` is the element record type (`TSeq<Record>`); the
    /// function's `ret` slot holds the `IrType::RecordSeq(record)`
    /// accumulator, each `yield` pushes onto it (`Stmt::SeqPush`), and
    /// `Terminator::Return` returns it. Emitted as a `[&]`-capturing
    /// lambda returning `std::vector<Record>` (v1's `emit_tseq` shape).
    /// Called via `CallTarget::Tseq` from a test-scope `let txns =
    /// Name(args)`.
    Tseq { record: RecordId },
    /// A test-scope closure-hook body — either an `on <obj>.<method>
    /// pre/post` method hook or an `on regs.REG` per-register write
    /// callback. Lowered with the firing context's surface (the method's
    /// params / a single `data` param) but in the TEST scope's
    /// `LowerCtx`, so it resolves promoted `_tb` host fields, the firing
    /// transactor's state (`drv.last_read`), regblock bindings, and the
    /// passive `record_*` API. Emitted as a free `[&]`-capturing lambda
    /// named by `TbFunction::name`, called from the firing site
    /// (`emit_method` pre/post fan-out, or `Stmt::RecordWriteCb`
    /// dispatch). This is the host-state-promotion mechanism that lets
    /// the function-per-CFG IR express v1's reference-capturing hook
    /// closures.
    TestHook,
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
    /// A transaction-sequence local: an ordered list of record values
    /// (`let txns = SomeTseq(...)`, typed `TSeq<Record>`). Emitted as
    /// `std::vector<Record>` — v1's tseq accumulator shape. Built by a
    /// `FunctionKind::Tseq` body (each `yield` is a `Stmt::SeqPush`),
    /// consumed by `for t in <seq>` iteration (`Expr::SeqLen` +
    /// `Expr::SeqIndex`).
    RecordSeq(RecordId),
    /// A composite-component value local — a method parameter typed by a
    /// component name (`observe(addr: uint<8>, model: ProtocolModel)`).
    /// Taken by value as the component's C++ struct; method calls on it
    /// (`model.predict_read(addr)`) dispatch through `ComponentBase::Local`.
    /// Only ever a parameter local in this subset — never a `let` body
    /// local — so it has no value-construction or randomize support.
    Component(ComponentId),
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
    /// `release dut.<probe>` — clear the active SV procedural force on a
    /// `probe force` signal so the DUT signal returns to its natural
    /// value. The `PortRef` carries `access: PortAccess::Force` and the
    /// probe name in `port_path`; emission clears the `_en` enable wire
    /// (mirrors v1's `release` → `<mangled>_en = 0`). Only valid for a
    /// force-capable probe (lowering rejects `release` on a read-only
    /// probe or an ordinary port).
    ProbeRelease(PortRef),
    /// (Re-)default-construct a record-typed local at its `let` site.
    /// Emitted as an explicit assignment (not just the hoisted
    /// declaration's initializer) because v1 declares the struct at
    /// the source `let` position — a `let t : Txn` inside a loop body
    /// re-runs the field defaults every iteration.
    RecordInit(LocalId, RecordId),
    /// `t.field = value` on a record-typed local. The value is
    /// port-hoisted like `Assign` (no inline DUT reads). `index: Some(e)`
    /// writes a `Vec<T, N>` field element (`rec.data[e] = value`); `None`
    /// writes a scalar field.
    RecordFieldWrite {
        local: LocalId,
        field: String,
        index: Option<Expr>,
        value: Expr,
    },
    /// `regs.record_write(addr, data)` where the binding has a per-register
    /// `on regs.REG` write callback registered — the firing form of the
    /// passive RAL record API. The address was const-decoded at lowering to
    /// the single target register, so the mirror update is a `RecordFieldWrite`
    /// in disguise (same `local`/`field`/masked `value`); the addition is the
    /// recursion-depth guard + callback dispatch. Emission mirrors v1's
    /// `try_emit_record_write`: bump `<binding>_cb_depth`, FATAL+`errors++`
    /// past `HARC_RAL_CB_MAX_DEPTH`, else write the mirror cell and (when
    /// `callback` is `Some`) call the callback with the observed value, then
    /// un-bump. A `record_write` whose register has NO callback but whose
    /// BINDING has callbacks on OTHER registers still routes here (carrying
    /// `callback: None`) so its mirror write is depth-counted consistently
    /// (matches v1, which wraps the whole decode chain in one guard).
    RecordWriteCb {
        /// Mirror record local (the `<Block>_Mirror` instance).
        local: LocalId,
        /// Per-binding recursion-depth counter base name (`<binding>`).
        binding: String,
        /// Target register name (the const-decoded register).
        field: String,
        /// The const-decoded register byte offset — used verbatim in the
        /// recursion-guard FATAL message so it matches v1's `at addr 0x..`.
        offset: u64,
        /// Masked observed value to store in the mirror cell.
        value: Expr,
        /// Callback for this register, fired after the mirror update.
        /// `None` when only other registers in the binding carry callbacks.
        callback: Option<FunctionId>,
    },
    /// `_tb.<field> = value` on a scalar testbench field (run/check-
    /// shared host state — see `TestbenchSchema::scalar_fields`). The
    /// value is port-hoisted like `Assign`.
    TbFieldWrite { field: String, value: Expr },
    /// `read_count = read_count + 1` inside a target-responder body
    /// (or `target.read_count = ...` from the test): write a bound-to
    /// target transactor's persistent state field. `instance` names the
    /// bound testbench-field instance; emission produces
    /// `<instance>.<field> = <value>`. The value is port-hoisted like
    /// `Assign` (no inline DUT reads).
    TransactorStateWrite {
        instance: String,
        field: String,
        value: Expr,
    },
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
        /// `None` → a scoreboard-typed TESTBENCH field, accessed as
        /// `_tb.<field>`. `Some(path)` → a data-only scoreboard held as
        /// an ENV sub-component (`top.sb`), accessed by the full dotted
        /// `path` against the run-scope env local. See `ScoreboardQuery`.
        nested_path: Option<Vec<String>>,
    },
    /// Write a composite-component scalar field. `base` selects the
    /// access form: `SelfField` (`count = count + 1` inside a method body
    /// → `self.count = ...`) or `Path` (`env.sb.errors = ...` from the
    /// test). The value is port-hoisted like `Assign`.
    ComponentFieldWrite {
        base: ComponentBase,
        field: String,
        value: Expr,
    },
    /// `emit observed(v)` — fan the args out to every callback registered
    /// on the named `out event<T>` field of the component named by `base`.
    /// `base = SelfField` for a self-relative `emit observed(v)` inside a
    /// method body (`self.<event>`); `base = Path([...])` for a test-scope
    /// `emit env.agent.in_ev(v)` (`env.agent.<event>`). Emitted as
    /// `for (auto& _s : <base>.<event>) _s(args);` plus v1's
    /// `_last_out_cycle` heartbeat bump on the emitting component.
    ComponentEmit {
        base: ComponentBase,
        event: String,
        args: Vec<Expr>,
    },
    /// A composite-component method call (`env.source.publish(3)`). `base`
    /// resolves the receiver sub-component path; `component` is its schema
    /// (to name `<Comp>_<method>`); `dest` receives a `-> T` return.
    /// Emitted as `<Comp>_<method>(<receiver>, args)`.
    ComponentCall {
        base: ComponentBase,
        component: ComponentId,
        method: String,
        args: Vec<Expr>,
        dest: Option<LocalId>,
    },
    /// `<base>.<queue>.push(value)` on a composite-component `queue<T>`
    /// field. `base` selects the access form: `SelfField` (`errors.push(e)`
    /// inside a scoreboard/component method body → `self.errors.push(e)`)
    /// or `Path` (`checker.sb.errors.push(e)` from the test). The value is
    /// port-hoisted like `ScoreboardOp::QueuePush`; a record value lowers
    /// to a struct push. Emitted as `<recv>.<queue>.push(<value>);`.
    ComponentQueuePush {
        base: ComponentBase,
        queue: String,
        value: Expr,
    },
    /// `let v = <base>.<queue>.pop()` — pop the queue front into a local.
    /// Always has a destination (a discarded pop is rejected at lowering,
    /// matching the scoreboard form). Emitted as `<dest> = <recv>.<queue>.pop();`.
    ComponentQueuePop {
        base: ComponentBase,
        queue: String,
        dest: LocalId,
    },
    /// `<dst>.<field> = <src>` — a whole composite-component value copy of
    /// a test-scope sub-component (`checker.sb = sb` / `responder.model =
    /// model`). `dst` resolves the receiver holding the sub-component
    /// field; `src` resolves the source component value (another test-scope
    /// component). Emitted as `<dst>.<field> = <src>;` — a plain C++ struct
    /// copy (mirrors v1's `_tb.checker.sb = _tb.sb;`).
    ComponentSubAssign {
        dst: ComponentBase,
        field: String,
        src: ComponentBase,
    },
    /// `yield t` inside a `tseq` body — append a record value onto the
    /// sequence accumulator. `seq` is the function's `RecordSeq` `ret`
    /// local; `value` is `Expr::Local(record)` (the yielded record).
    /// Emitted as `<seq>.push_back(<value>);` (v1's `_result.push_back`).
    SeqPush { seq: LocalId, value: Expr },
    /// `let x = fork bus.<method>(args)` — issue the REQUEST side of a
    /// bus-bound `tlm_method` call now, deferring the response capture to
    /// the next `Stmt::TlmJoinAll` (v1's `try_emit_bus_tlm_fork`). The
    /// descriptor carries the request payload (`args`), the response
    /// destination (`dest`, declared/zero-init at the fork site), and the
    /// OOO lane `tag` (`None` for a `blocking` method = issue-order FIFO
    /// routing, `Some` for `out_of_order tags N` = hidden-tag routing).
    /// The wire protocol (req_valid/req_ready + optional req_tag) is
    /// backend-owned, mirroring v1.
    TlmFork(TlmForkDesc),
    /// `join_all` — drain every `Stmt::TlmFork` issued since the previous
    /// `join_all`, capturing each response into its `dest`. Carries the
    /// full pending-fork list (lowering accumulates it; the IR is self-
    /// contained so backends never replay lowering state). All-untagged
    /// forks route by issue order (`emit_ordered_tlm_join_all`); all-
    /// tagged forks route by `rsp_tag` match (`emit_tagged_tlm_join_all`).
    /// Mixing tagged and untagged forks before one join_all is rejected
    /// at lowering. An empty list is a no-op (v1's "no pending forks").
    TlmJoinAll(Vec<TlmForkDesc>),
}

/// One deferred bus-bound `tlm_method` fork: the request payload plus
/// the metadata both `Stmt::TlmFork` (request emission) and
/// `Stmt::TlmJoinAll` (response capture) need. Self-contained so the
/// join statement carries its own descriptors (no cross-statement
/// lowering replay in the backend), mirroring v1's `PendingTlmFork`.
#[derive(Debug, Clone)]
pub struct TlmForkDesc {
    /// Bus-binding field name (== flat DUT signal prefix).
    pub bus_field: String,
    /// `tlm_method` name on the bound bus.
    pub method: String,
    /// Request payload expressions, in declared-arg order. Port-hoisted
    /// like a blocking `tlm_method` call's args (no inline DUT reads).
    pub args: Vec<Expr>,
    /// Response destination local (declared + zero-init at the fork
    /// site); `None` when the fork result is discarded.
    pub dest: Option<LocalId>,
    /// True when the method declares a return type (a `rsp_data` wire to
    /// capture).
    pub has_ret: bool,
    /// OOO lane tag: `None` for a `blocking` method (issue-order FIFO),
    /// `Some(n)` for `out_of_order tags N` (the per-`(field, method)`
    /// monotonically allocated request tag, routed by `rsp_tag` match).
    pub tag: Option<u64>,
}

/// How a composite-component field/method access names its receiver.
#[derive(Debug, Clone)]
pub enum ComponentBase {
    /// Self-relative — inside a `ComponentMethod` body, `count` resolves
    /// to `self.count`. The body's owning component is the method's.
    SelfField,
    /// A dotted path from a test-scope component local
    /// (`env.source.publish` → `path = ["env", "source"]`). The first
    /// segment is the test field; the rest are nested sub-component
    /// fields. Emitted as `env.source` (dot-joined, all by-value).
    Path(Vec<String>),
    /// A component-typed method-parameter local (`model` in
    /// `observe(addr, model: ProtocolModel)`). The receiver is the local
    /// itself, passed by value. Emitted as the local's C++ name.
    Local(LocalId),
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
    /// `randomize(t)` / `randomize(t) with {...}` — solve the target
    /// record's constraint set and write the solved field values back
    /// into the `target` local, then resume at `succ`.
    ///
    /// A terminator (not a `Stmt`) per the design: it is a potential
    /// host-sync point on any placement-split backend, so every backend
    /// must make an explicit decision about it. `constraints` is the
    /// handle into the constraint-IR layer (`TbProgram::constraint_sites`).
    Randomize {
        target: LocalId,
        constraints: ConstraintRef,
        succ: BlockId,
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
    /// `index: Some(e)` reads a `Vec<T, N>` field element (`rec.data[e]`,
    /// emitted as `local.field[e]`); `None` reads a scalar field.
    RecordField { local: LocalId, field: String, index: Option<Box<Expr>> },
    /// `_tb.<field>` read on a scalar testbench field. Host state —
    /// allowed in every expression position a `Local` is.
    TbField(String),
    /// A read of a bound-to target transactor's persistent state field,
    /// either from a responder body (`read_count`) or from the test
    /// (`target.read_count`). `instance` is the bound testbench-field
    /// instance name (e.g. `target`); emission produces
    /// `<instance>.<field>` against the generated per-instance struct.
    /// Host state — allowed in every position a `Local` is.
    TransactorState { instance: String, field: String },
    /// A value-producing scoreboard read on a scoreboard-typed testbench
    /// field: `sb.writes` (scalar), `sb.expected.size()`,
    /// `sb.expected.empty()`. Host state — allowed wherever a `Local`
    /// is. `.pop()` is NOT here (it mutates) — it lowers to
    /// `Stmt::ScoreboardOp { op: QueuePop, .. }`.
    ScoreboardQuery {
        sb: ScoreboardId,
        field: String,
        query: ScoreboardQuery,
        /// `None` → a scoreboard-typed TESTBENCH field, accessed as
        /// `_tb.<field>`. `Some(path)` → a data-only scoreboard held as
        /// an ENV sub-component (`top.sb`), accessed by the full dotted
        /// `path` (e.g. `["top","sb"]`) against the run-scope env local.
        /// Validation skips the testbench-field binding check for the
        /// nested form (the board lives inside the env, not on `_tb`).
        nested_path: Option<Vec<String>>,
    },
    /// Read a composite-component scalar field. `base` is `SelfField`
    /// (`count` inside a method → `self.count`) or `Path` (`env.sb.count`
    /// from the test → `env.sb.count`). Host state — allowed wherever a
    /// `Local` is. Queue/event fields are never read this way (queues use
    /// scoreboard-style ops which are out of subset for components in v0;
    /// events are written via `connect`/`emit` only).
    ComponentField { base: ComponentBase, field: String },
    /// A whole composite-component value, passed by value as a method
    /// argument: `sb.observe(addr, model)` reads `model` here, where the
    /// callee parameter is component-typed. `base` resolves the receiver
    /// (a `self` sub-component field, or a test-scope component path).
    /// Emitted as the C++ struct value at `base` (a by-value copy at the
    /// call), mirroring v1's `ResponseScoreboard_observe(..., model)`.
    ComponentValue { base: ComponentBase },
    /// A value-producing read on a composite-component `queue<T>` field:
    /// `<base>.<queue>.size()` / `.empty()`. Host state — allowed wherever
    /// a `Local` is. `.pop()` is NOT here (it mutates) — it lowers to
    /// `Stmt::ComponentQueuePop`. Mirrors `ScoreboardQuery` for the
    /// method-bearing-scoreboard / component-field queue (`query` carries
    /// the queue-field name via `QueueSize`/`QueueEmpty`).
    ComponentQueueQuery {
        base: ComponentBase,
        query: ScoreboardQuery,
    },
    /// A heartbeat-idle predicate on a component: `agent.idle_in(N)`,
    /// `agent.idle_out(N)`, or `agent.idle(N)` (= both). `base` resolves
    /// the component instance; `n` is the cycle-count threshold. Reads the
    /// auto-injected `_last_in_cycle`/`_last_out_cycle` stamps against the
    /// global `cycle_count` (v1's `emit_idle_predicate`). Boolean-valued —
    /// allowed wherever a `Local` is.
    ComponentIdle {
        base: ComponentBase,
        kind: IdleKind,
        n: Box<Expr>,
    },
    /// The global simulation cycle counter (`cycle_count`). A bare
    /// `cycle_count` ident resolves here (it is a framework-provided
    /// value, not a user local). Both backends emit the in-scope
    /// `cycle_count` variable; allowed wherever a `Local` is — its
    /// canonical use is `${cycle_count}` inside a `log`/`watchdog`
    /// diagnostic. uint64_t-valued.
    CycleCount,
    /// The framework error counter (`errors`). A bare `errors` ident
    /// resolves here — it is a framework-provided counter (bumped by
    /// `AssertCheck`/`Log(error|fatal)`), not a user local. Both
    /// backends emit the in-scope `errors` variable; allowed wherever a
    /// `Local` is. Canonical use: `assert errors == 0` after a
    /// compile-time-unrolled walk like `bitbash(regs)`. int-valued.
    ErrorCount,
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
    /// `<seq>.size()` — element count of a `RecordSeq` local, used as the
    /// upper bound of a `for t in <seq>` loop. Lowers to `uint64_t`
    /// (emitted as `<seq>.size()`).
    SeqLen(LocalId),
    /// `<seq>[<index>]` — the record value at `index` in a `RecordSeq`
    /// local. Record-valued (allowed wherever a record `Local` is —
    /// notably the `Stmt::Assign` that binds the `for t in <seq>` loop
    /// variable). Emitted as `<seq>[<index>]`.
    SeqIndex { seq: LocalId, index: Box<Expr> },
    Call(CallTarget, Vec<Expr>),
    /// A register-level frontdoor READ on a regblock binding in a
    /// general expression position — `regs.NAME` outside `let`-RHS
    /// (an assert condition, a `log`/`fail` format arg). Emits v1's
    /// inline assignment-expression:
    ///
    /// ```text
    /// RW/RO: (mirror.field = <HelperTy>_read(off))   // bus read + predict
    /// WO:    mirror.field                            // mirror only
    /// ```
    ///
    /// This is NOT a `CallTarget::TransactorMethod` call edge: the
    /// regblock `via` helper's `read` lowers to an ordinary hookable
    /// lambda (a plain C++ function call), not the bus req/rsp wire
    /// protocol, so it is a legitimate sub-expression value — unlike
    /// the TLM seam, which the verifier pins to statement position. The
    /// inline form fires exactly one bus read per textual occurrence,
    /// matching v1's read-count semantics (eager in conditions, lazy
    /// in fail messages emitted inside the `if (!cond)` branch).
    ///
    /// `helper_ty` is the resolved transactor TYPE name (the emitted
    /// lambda is `<helper_ty>_read`); `mirror` is the synthetic mirror
    /// record local; `field` is the register name; `offset` is the
    /// folded byte offset; `reads_bus` is `RegAccess::reads_from_bus`.
    RegRead {
        mirror: LocalId,
        helper_ty: String,
        field: String,
        offset: u64,
        reads_bus: bool,
    },
}

/// Which heartbeat stamp(s) an `Expr::ComponentIdle` predicate reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleKind {
    /// `idle_in(N)` — N cycles since last input activity.
    In,
    /// `idle_out(N)` — N cycles since last output activity.
    Out,
    /// `idle(N)` — both `idle_in(N)` AND `idle_out(N)`.
    Both,
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
    /// Call to a `extern function name(...) -> ret` (spec §9) — a C
    /// reference model linked in via `--ref-src`. Emitted with the RAW
    /// symbol name (no `harc_helper_` mangling) so it resolves against
    /// the user-provided `extern "C"` definition; the forward
    /// declaration is emitted file-scope by `emit_extern_fn_decls`.
    ExternFn(String),
    TransactorMethod { bus_field: String, method: String },
    /// Call a `tseq` generator by name — `let txns = RandomTxns(5)`.
    /// Resolves to the `FunctionKind::Tseq` function of that name; the
    /// result is an `IrType::RecordSeq` assigned into a test-scope local.
    /// Emitted as a direct `<name>(args)` lambda call (v1's tseq lambda).
    Tseq(String),
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
    pub fn regblock(&self, id: RegblockId) -> &RegblockSchema {
        &self.regblocks[id.index()]
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
            Terminator::Randomize { succ, .. } => vec![*succ],
            Terminator::Return | Terminator::Fatal(_) => vec![],
        }
    }
}
