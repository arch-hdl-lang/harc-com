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
    /// Index into `TbProgram::property_checks` — one registered
    /// concurrent `assert`/`assume` (spec §5). See `PropertyCheckSchema`.
    PropertyCheckId
);
ir_id!(
    /// Index into `TbProgram::cover_checks` — one registered concurrent
    /// `cover` witness counter (spec §5). See `CoverCheckSchema`.
    CoverCheckId
);
ir_id!(
    /// Index into `TbProgram::cycle_handlers` — one statement-position
    /// `on` handler inside a run/check body. See `CycleHandlerSchema`.
    CycleHandlerId
);
ir_id!(
    /// Index into `TbProgram::constraint_sites` — the handle a
    /// `Terminator::Randomize` carries into the constraint-IR layer.
    /// See `ConstraintSite` and the `Terminator::Randomize` doc.
    ConstraintRef
);

/// The element type of a `tseq`'s `-> TSeq<T>` return sequence. Carried
/// in the lowering `tseqs` map so the consumer site (`let xs = Some(...)`)
/// can type the receiving local: a record element becomes `RecordSeq`, a
/// scalar element becomes `Seq(scalar)`. v1 renders both as `std::vector<T>`
/// — the only difference is the element C++ type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TseqElem {
    /// `TSeq<Record>` — element is a declared `transaction`/`struct`.
    Record(RecordId),
    /// `TSeq<scalar>` — element is a `UInt`/`SInt`/`Bool` (`IrType`).
    Scalar(IrType),
}

impl TseqElem {
    /// The accumulator/result `IrType` a tseq of this element produces
    /// (`RecordSeq` for a record element, `Seq` for a scalar element).
    pub fn seq_type(&self) -> IrType {
        match self {
            TseqElem::Record(r) => IrType::RecordSeq(*r),
            TseqElem::Scalar(t) => IrType::Seq(Box::new(t.clone())),
        }
    }
}

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
    /// Concurrent (`per-primary-clock-edge`) property checks — the table
    /// `Stmt::PropertyCheck` indexes. One entry per lowered `assert`
    /// / `assume` whose body is a named property reference or carries a
    /// temporal operator (spec §5). See `PropertyCheckSchema`.
    pub property_checks: Vec<PropertyCheckSchema>,
    /// Concurrent `cover` witness counters — the table
    /// `Stmt::CoverCheck` indexes, and the source of the end-of-test
    /// cover summary. See `CoverCheckSchema`.
    pub cover_checks: Vec<CoverCheckSchema>,
    /// Statement-position `on` handlers — the table `Stmt::CycleHandler`
    /// indexes. One entry per `on <bool-expr>` / `on <N> cycles` handler
    /// written inside a run or check body. See `CycleHandlerSchema`.
    pub cycle_handlers: Vec<CycleHandlerSchema>,
}

/// One statement-position `on` handler (spec §7.10 / §7.x cycle
/// triggers) written inside a run or check body, as opposed to the
/// testbench-declaration-scoped forms in `TestbenchSchema::
/// {periodic_services, cycle_services}`.
///
/// The difference is WHEN it arms: a testbench-scoped handler registers
/// during test setup, while this one registers where the statement
/// appears, so a handler written after a `wait` never observes the
/// earlier cycles. v1 makes the same distinction — its `emit_cycle_trigger`
/// pushes into `_checkers` inline at the statement position.
///
/// The body is a zero-parameter `FunctionKind::TestHook` function
/// (emitted as a free `[&]`-capturing lambda at test scope, like every
/// other hook body) so the per-cycle registration closure can call it
/// without capturing anything that dies with the enclosing block.
#[derive(Debug, Clone)]
pub struct CycleHandlerSchema {
    pub kind: CycleHandlerKind,
    /// Lowered handler body (`kind: TestHook`, zero params).
    pub function: FunctionId,
    /// `phase` modifier (`on <expr> phase post_eval`). `Checker`
    /// (default) registers into `_checkers`; `PostEval` into
    /// `_post_eval_services`.
    pub phase: HandlerPhase,
}

/// What arms a `CycleHandlerSchema`.
#[derive(Debug, Clone)]
pub enum CycleHandlerKind {
    /// `on <bool-expr> ... end on` — re-evaluate the predicate every
    /// primary-clock cycle and fire the body per `edge`.
    Trigger { trigger: Expr, edge: CycleEdge },
    /// `on <N> cycles ... end on` — fire once every positive `period`
    /// primary-clock cycles. Statement-position registration captures the
    /// surrounding run/check frame, so the expression is re-read each cycle
    /// exactly where v1 registers it.
    Periodic { period: Expr },
}

/// Severity band of a concurrent property check — which log tag the
/// failure line carries and whether it bumps the test error counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertySeverity {
    /// `assert` — logs `FAIL` and bumps `ctx.errors`.
    Fail,
    /// `assume` — logs `ASSUME-FAIL` and does NOT bump the error
    /// counter (an assumption is an input constraint, not a DUT bug).
    AssumeFail,
}

impl PropertySeverity {
    /// The `sim_log_line` severity tag (v1's `emit_property_check`
    /// `severity` argument).
    pub fn tag(self) -> &'static str {
        match self {
            PropertySeverity::Fail => "FAIL",
            PropertySeverity::AssumeFail => "ASSUME-FAIL",
        }
    }
    /// Whether a failure bumps the test's error counter.
    pub fn counts_as_error(self) -> bool {
        matches!(self, PropertySeverity::Fail)
    }
}

/// The top-level temporal shape of a concurrent property body. v1's
/// `emit_property_check` switches on exactly these three cases; the
/// classification moves to lowering so the backend is a renderer.
#[derive(Debug, Clone)]
pub enum PropertyShape {
    /// `a |-> b` — same-cycle implication: fail when `a && !b`.
    Implies { ante: Expr, cons: Expr },
    /// `a |=> b` — one-cycle-delayed implication: fail when the
    /// PREVIOUS cycle's `a` held and this cycle's `b` does not. Carries
    /// one implicit `prev` latch, distinct from the `temporals` slots.
    ImpliesNext { ante: Expr, cons: Expr },
    /// A plain boolean invariant — fail when it is false.
    Invariant(Expr),
}

/// One `past(e)` / `rose(e)` / `fell(e)` / `stable(e)` latch inside a
/// concurrent check body. The backend gives each slot a `static`
/// previous-value cell plus a per-call current-value local; references
/// to the slot inside the body are `Expr::TemporalSlot`, so the tree is
/// self-contained (v1 threads the same information through a span-keyed
/// `prop_subs` side table during emission).
#[derive(Debug, Clone)]
pub struct TemporalSlot {
    /// The argument expression, latched into `<tag>_cur<i>` each cycle
    /// and copied into `<tag>_ps<i>` after the body runs.
    pub inner: Expr,
}

/// Which temporal reading a `Expr::TemporalSlot` takes of its slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalFn {
    /// `past(e)` — the previous cycle's value.
    Past,
    /// `rose(e)` — 0 → 1 this cycle.
    Rose,
    /// `fell(e)` — 1 → 0 this cycle.
    Fell,
    /// `stable(e)` — unchanged from the previous cycle.
    Stable,
}

/// One registered concurrent property check (`assert`/`assume` whose
/// body is a named-property reference or carries a temporal operator).
/// Registered at the source statement's position (v1 pushes the closure
/// into `_checkers` inline, so a check declared mid-`run` only observes
/// cycles from that point on) and evaluated once per primary-clock edge.
#[derive(Debug, Clone)]
pub struct PropertyCheckSchema {
    /// Unique C++ identifier stem for this check's `static` state,
    /// derived from the source span (v1's `_p_<start>_<end>`).
    pub tag: String,
    /// Human-readable name in the failure line — the property name for
    /// a named reference, else `<inline>`.
    pub label: String,
    pub severity: PropertySeverity,
    pub shape: PropertyShape,
    /// Temporal latch slots referenced by `Expr::TemporalSlot` inside
    /// `shape`, in slot-index order.
    pub temporals: Vec<TemporalSlot>,
    /// `else fail("...")` message, replacing the generic
    /// ``property `<label>` failed`` line. `None` for a check written
    /// without one. (v1 parses the clause and then discards it, so a
    /// concurrent assertion there always reports the generic line.)
    pub message: Option<FmtArgs>,
}

/// One registered concurrent `cover` witness counter (spec §5): every
/// primary-clock edge on which `cond` holds bumps a persistent hit
/// count, and the end-of-test summary reports hit/total plus a
/// per-point line. Flat (no bins) — a covergroup is the binned form.
#[derive(Debug, Clone)]
pub struct CoverCheckSchema {
    /// Unique C++ identifier stem for the hit counter (`_cov_<tag>_hits`),
    /// derived from the source span (v1's `c_<start>_<end>`).
    pub tag: String,
    /// Report label — the property/identifier name for a named cover,
    /// else `cov_<start>_<end>` (v1's fallback).
    pub label: String,
    /// The witness predicate, evaluated once per primary-clock edge.
    pub cond: Expr,
    /// Temporal latch slots referenced by `Expr::TemporalSlot` inside
    /// `cond`, in slot-index order.
    ///
    /// **Documented divergence from v1:** v1 does not translate temporal
    /// calls inside a `cover` body (`emit_expr` has no `SystemCall` arm
    /// outside a property check, so the operand vanishes and the emitted
    /// C++ does not compile). TB-IR gives a cover body the same latch
    /// machinery as a property body, so `cover rose(dut.valid)` works.
    pub temporals: Vec<TemporalSlot>,
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
/// represented by the regblock lowering paths in `src/ir/lower/regblock.rs`.
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
    /// Persistent state fields of a bound-to target transactor. They
    /// live on a generated per-instance C++ struct (mirroring v1's
    /// component-instance struct), readable/writable from the responder
    /// bodies and from the test (`target.read_count`). A field is either
    /// a scalar counter (`read_count : uint<32> default 0`) or a typed
    /// FIFO queue (`pending : queue<Record>` / `queue<uint<32>>`),
    /// reusing the scoreboard/component `QueueElem` machinery. Empty for
    /// the unbound BFM form (whose state fields are still rejected in
    /// that path).
    pub state_fields: Vec<StateFieldSchema>,
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
    /// Whether the source thread is always present or declared inside
    /// `when active`. Passive bindings must not schedule active-only
    /// responder actors.
    pub activation: Activation,
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
    /// Declared parameter NAMES, in order — duplicated from the
    /// function so call sites (which lower under a schema snapshot,
    /// without the functions table) can both check arity (`.len()`) and
    /// check a named argument against the declaration.
    ///
    /// This was `n_params: usize` and the names were dropped on the
    /// floor at construction, where `f.params` has them. That left the
    /// transactor-method call sites with nothing to validate a named
    /// argument against, so they refused every named argument —
    /// including the in-order form v1 emits byte-identically. Carrying
    /// the names rather than a second count also means arity and names
    /// cannot disagree.
    pub param_names: Vec<String>,
    /// Declared parameter TYPES, in order — duplicated from the function
    /// for the same reason as `param_names`: a call site lowers under a
    /// schema snapshot, without the functions table, so it had nothing
    /// to type-check an argument against. Without this, passing a record
    /// to a scalar parameter (or a scalar to a record one) lowered and
    /// emitted `AxilXactor_step(_tb.drv, <Beat>)` — "no match for call
    /// to" in both backends.
    pub param_tys: Vec<IrType>,
    /// Declared return type, retained at call sites so signed and wide
    /// results use the same local/C++ ABI as the method body.
    pub ret_ty: Option<IrType>,
    /// True for `-> T` methods (the function carries a `ret` slot).
    pub has_ret: bool,
    /// True only when the source declaration used `hookable` rather than
    /// plain `function`. Both forms share the same method-body IR, but only
    /// a hookable method may be targeted by `on ... pre/post` handlers or
    /// hook-triggered covergroups.
    pub hookable: bool,
    /// True when this method is declared inside the transactor's `when
    /// active` block (as opposed to the always-on body). A method that is
    /// active-only is NOT callable on a `passive` instance — the call site
    /// rejects it (mirroring v1's "`<m>` is declared inside `when
    /// active`" diagnostic). Always-on methods are callable on both.
    pub active_only: bool,
    /// Covergroup auto-samplers that subscribe to this method's pre/post
    /// hook boundary (`covergroup G @(drv.send(t) post)`). Populated by
    /// the `covergroup_hooks` pass after transactors are lowered. Each
    /// entry names the covergroup to sample and which side fires it.
    /// When non-empty, the tbir backend emits the `<Type>_<method>_pre`/
    /// `_post` hook-vector spine and fans it out at the method body (v1's
    /// `emit_hook_vectors` + fan-out), and the cov field registers its
    /// sample closure onto that vector instead of `_checkers`.
    pub cov_hook_subs: Vec<(CovgroupId, crate::ast::HookSide)>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordSchema {
    pub name: String,
    pub fields: Vec<RecordFieldSchema>,
    /// `keep <expr>` constraint clauses, pretty-printed. Inert (see
    /// the struct doc); randomize-free usage never evaluates them,
    /// which matches v1 (keeps emit zero C++ unless randomized).
    pub keeps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordFieldSchema {
    pub name: String,
    /// Field element type. For a scalar field this is the field's own
    /// scalar type; for a `Vec<T, N>` field (`vec_len = Some(N)`) it is
    /// the *element* type `T` — a scalar (its width drives both the C++
    /// storage type and the packed-bit layout) or a nested record
    /// (`IrType::Record`, v1's `std::array<Inner, N>` member; every leaf
    /// of the element record is itself schema-supported). Lowering
    /// rejects field element types outside that set. A dynamic scalar
    /// `list<T>` is represented distinctly as `IrType::Seq(T)` with
    /// `vec_len = None`; widthless and aggregate list elements remain out of
    /// subset.
    pub ty: IrType,
    /// `Some(N)` when the field is a fixed-size `Vec<T, N>` aggregate
    /// (v1's `std::array<T, N>` record member); `None` for a scalar
    /// field or a dynamic `list<T>`. The element type/width is carried in
    /// `ty`; lists are distinguished by `IrType::Seq(T)`.
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
/// host-state record of scalar counters, value-records, dynamic lists, and
/// typed FIFO queues, in declaration order. Both backends emit this as a C++ struct (v1's
/// `emit_scoreboard` shape is the behavior reference): scalar fields as
/// `uint64_t`/`int64_t`/`bool` members with their declared defaults,
/// value-record fields as default-constructed record members, `list<T>` fields
/// as `std::vector<T>` members, and
/// `queue<T>` fields as `harc_rt::HarcQueue<T>` members.
///
/// Subset (v0): a scoreboard is a *testbench field* holding data only.
/// Scalar fields, whole value-record fields, `list<T>` fields with scalar or
/// one-dimensional fixed-vector elements, and `queue<T>` fields where `T`
/// is `bool`, an explicitly sized `uint`/`sint` in the language range
/// 1..=1024, or a value-record lower; the test body manipulates them through
/// `Stmt::ScoreboardOp` (scalar/record read/write and queue push/pop), plus
/// read-only size/empty queries shared by list and queue storage.
/// Scoreboard
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

/// A scoreboard field is persistent host-side value storage.
#[derive(Debug, Clone)]
pub enum ScoreboardFieldKind {
    /// `writes : uint<32> default 0` — a scalar host counter. The
    /// `default` is the declared initializer literal (0 fallback, v1).
    Scalar {
        ty: IrType,
        default: ScoreboardScalarDefault,
    },
    /// `last : CheckerState` — a value-record held directly in the
    /// scoreboard struct. Whole-record reads/writes use the existing
    /// scoreboard query/op seam; the record is default-constructed.
    Record { record: RecordId },
    /// `history : list<uint<8>>` / `lanes : list<Vec<uint<8>, 2>>` —
    /// dynamically-sized storage emitted as `std::vector<T>`. Lists expose
    /// read-only size/empty queries but not queue push/pop operations; this
    /// schema preserves the declaration and its exact scalar or
    /// one-dimensional fixed-vector element layout.
    List {
        elem: IrType,
        vec_len: Option<usize>,
    },
    /// `expected : queue<uint<32>>` / `errors : queue<CheckerError>` — a
    /// FIFO whose element is an exact scalar type through the 1024-bit
    /// language ceiling, a scalar-leaf fixed vector, or a value-record.
    Queue { elem: QueueElem },
}

/// Width-preserving initializer for persistent scoreboard scalar state.
#[derive(Debug, Clone)]
pub enum ScoreboardScalarDefault {
    Narrow(u64),
    /// LSB-first 32-bit words, matching `Expr::WideLiteral`.
    Wide(Vec<u32>),
}

/// The element type of a `queue<T>` field: an exact scalar IR type, a
/// scalar/record-leaf fixed vector, scalar/record dynamic list, or a
/// value-record carried by struct. Shared by testbenches, scoreboards,
/// composite components, and transactor state so every queue owner uses one
/// storage descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueElem {
    /// `queue<uint<256>>` / `queue<sint<65>>` / `queue<bool>` — the exact
    /// scalar type (1..=1024 bits when sized) drives inferred pop locals and
    /// the width-aware C++ storage mapping (`uint64_t`, `_harc_u128`, or
    /// `HarcWide<N>`).
    Scalar { ty: IrType },
    /// `queue<Vec<uint<8>, 4>>` / `queue<Vec<Beat, 4>>` — a fixed-size
    /// aggregate carried by value. The element schema is recursive, so nested
    /// scalar or record vectors retain their exact `std::array` layout.
    FixedVec { elem: Box<IrType>, len: usize },
    /// `queue<list<uint<8>>>` / `queue<list<Beat>>` — a dynamic scalar or
    /// record list carried by value. The element remains exact so storage is
    /// `HarcQueue<std::vector<T>>` and inferred pop locals retain their list type.
    List { elem: Box<IrType> },
    /// `queue<CheckerError>` — a value-record element. `RecordId` indexes
    /// `TbProgram::records`; the C++ element type is the record struct.
    Record(RecordId),
}

impl QueueElem {
    /// The queue's full IR element type, shared by lowering and verification
    /// so inferred/discarded pop destinations cannot lose scalar width or
    /// signedness.
    pub fn ir_type(&self) -> IrType {
        match self {
            Self::Scalar { ty } => ty.clone(),
            Self::FixedVec { elem, len } => IrType::FixedVec {
                elem: elem.clone(),
                len: *len,
            },
            Self::List { elem } => match elem.as_ref() {
                IrType::Record(record) => IrType::RecordSeq(*record),
                _ => IrType::Seq(elem.clone()),
            },
            Self::Record(record) => IrType::Record(*record),
        }
    }
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
    /// Binding rule that remains meaningful even when the schema has no
    /// active-only member. In particular, an always-on analysis monitor may
    /// be modeless or passive, but an explicitly active monitor is invalid.
    pub instance_mode_policy: ComponentInstanceModePolicy,
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
    /// selected cycle-service closure with per-instance static prev-state. The body
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

/// The source-level ownership mode of a component-path transactor binding.
/// Structural components may carry this only as inherited context for a nested
/// transactor; they never gain an active surface themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentInstanceMode {
    Active,
    Passive,
}

/// Per-schema rule for explicit instance-mode annotations. Most component
/// schemas use `Standard`; only always-on analysis monitors reject an
/// explicitly active binding because it selects no source-level behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentInstanceModePolicy {
    Standard,
    AlwaysOnAnalysisMonitor,
}

impl ComponentInstanceMode {
    pub fn includes(self, activation: Activation) -> bool {
        matches!(activation, Activation::Always) || matches!(self, ComponentInstanceMode::Active)
    }
}

/// Whether an optional effective instance mode exposes a member with this
/// activation. A missing mode never exposes an active-only member.
pub fn component_mode_includes_activation(
    mode: Option<ComponentInstanceMode>,
    activation: Activation,
) -> bool {
    matches!(activation, Activation::Always) || mode.is_some_and(|mode| mode.includes(activation))
}

/// Whether a component member came from the ordinary body or `when active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Always,
    ActiveOnly,
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
    /// `when active` provenance for per-instance registration.
    pub activation: Activation,
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
    /// `phase` modifier (`on <expr> phase post_eval`). `Checker` (default)
    /// dispatches from the per-cycle `_checkers` vector; `PostEval`
    /// dispatches from `_post_eval_services`.
    pub phase: HandlerPhase,
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
    ///
    /// Sampling cadence differs from the plain `edge` modes: v1 lowers a
    /// bound monitor as a `wait_until(valid && ready)` + `wait_cycles(1)`
    /// coroutine loop, so a continuously-held handshake samples every OTHER
    /// cycle (one beat, then the `wait_cycles(1)` re-arm consumes the next).
    /// The tbir cycle-service emission reproduces this with a fire-then-cooldown
    /// latch (see `mod::emit_lifecycle_checkers`), NOT the `edge` field — for
    /// a monitor channel the stored `edge` (`Rising`) is vestigial.
    pub monitor_channel: Option<String>,
    /// `when active` provenance for per-instance registration.
    pub activation: Activation,
}

/// Edge mode for a cycle-trigger handler — mirrors `ast::EdgeMode` in the
/// IR so the codegen needn't reach back into the AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleEdge {
    Rising,
    Falling,
    Level,
}

impl CycleEdge {
    /// Lower from the AST `EdgeMode`.
    pub fn from_ast(e: crate::ast::EdgeMode) -> Self {
        match e {
            crate::ast::EdgeMode::Rising => CycleEdge::Rising,
            crate::ast::EdgeMode::Falling => CycleEdge::Falling,
            crate::ast::EdgeMode::Level => CycleEdge::Level,
        }
    }
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
    /// `when active` provenance for per-instance registration.
    pub activation: Activation,
}

impl ComponentSchema {
    pub fn field(&self, name: &str) -> Option<&ComponentFieldSchema> {
        self.fields.iter().find(|f| f.name == name)
    }
    pub fn method(&self, name: &str) -> Option<&ComponentMethodSchema> {
        self.methods.iter().find(|m| m.name == name)
    }

    /// Whether this schema contains behavior or storage selected by an
    /// instance's active/passive mode.
    pub fn has_active_surface(&self) -> bool {
        self.fields
            .iter()
            .any(|field| matches!(field.activation, Activation::ActiveOnly))
            || self
                .methods
                .iter()
                .any(|method| matches!(method.activation, Activation::ActiveOnly))
            || self
                .on_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || self
                .periodic_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || self
                .cycle_handlers
                .iter()
                .any(|handler| matches!(handler.activation, Activation::ActiveOnly))
            || self
                .watchdog
                .as_ref()
                .is_some_and(|handler| matches!(handler.activation, Activation::ActiveOnly))
    }

    /// Whether an instance binding must carry a resolved mode. Connect edges
    /// are included for transactors because an active-only endpoint changes
    /// per-instance registration, even when no owned field/function is active.
    pub fn requires_instance_mode(&self) -> bool {
        matches!(self.kind, ComponentKindTag::Transactor)
            && (self.has_active_surface()
                || self.connects.iter().any(|edge| {
                    matches!(edge.src_activation, Activation::ActiveOnly)
                        || matches!(edge.sink_activation, Activation::ActiveOnly)
                }))
    }
}

/// The component and effective transactor mode reached by a path through
/// by-value component fields. Structural components preserve their inherited
/// context; a transactor field may replace it with its declared mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedComponentPath {
    pub component: ComponentId,
    pub effective_mode: Option<ComponentInstanceMode>,
}

/// A malformed component path or illegal structural mode encountered while
/// resolving an instance path. Lowering maps these to source diagnostics;
/// verification and codegen use the same policy to protect mutated IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentPathResolutionError {
    MissingComponent(ComponentId),
    NotSubcomponent {
        component: String,
        segment: String,
    },
    StructuralMode {
        kind: ComponentKindTag,
        segment: String,
    },
    UnresolvedMode {
        path: String,
    },
    ForbiddenActiveMode {
        path: String,
    },
}

impl std::fmt::Display for ComponentPathResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingComponent(component) => write!(f, "missing component c{}", component.0),
            Self::NotSubcomponent { component, segment } => {
                write!(f, "`{segment}` is not a sub-component of `{component}`")
            }
            Self::StructuralMode { kind, segment } => {
                write!(
                    f,
                    "a transactor mode on {} field `{segment}`",
                    kind.keyword()
                )
            }
            Self::UnresolvedMode { path } => {
                write!(
                    f,
                    "transactor path `{path}` has no effective active/passive mode"
                )
            }
            Self::ForbiddenActiveMode { path } => write!(
                f,
                "an explicit `active` mode on always-on analysis monitor `{path}` is invalid"
            ),
        }
    }
}

/// Resolve one component instance path for every consumer of mode metadata.
/// This is the sole traversal policy for lowering access checks, verifier
/// integrity checks, and TB-IR registration emission.
pub fn resolve_component_path_mode(
    components: &[ComponentSchema],
    start: ComponentId,
    inherited: Option<ComponentInstanceMode>,
    path: &[String],
) -> Result<ResolvedComponentPath, ComponentPathResolutionError> {
    let mut component = start;
    let mut effective_mode = inherited;
    for segment in path {
        let schema = components
            .get(component.index())
            .ok_or(ComponentPathResolutionError::MissingComponent(component))?;
        let field =
            schema
                .field(segment)
                .ok_or_else(|| ComponentPathResolutionError::NotSubcomponent {
                    component: schema.name.clone(),
                    segment: segment.clone(),
                })?;
        let ComponentFieldKind::Sub {
            component: child,
            mode: declared_mode,
        } = &field.kind
        else {
            return Err(ComponentPathResolutionError::NotSubcomponent {
                component: schema.name.clone(),
                segment: segment.clone(),
            });
        };
        let child_schema = components
            .get(child.index())
            .ok_or(ComponentPathResolutionError::MissingComponent(*child))?;
        if matches!(child_schema.kind, ComponentKindTag::Transactor) {
            effective_mode = (*declared_mode).or(effective_mode);
        } else if declared_mode.is_some() {
            return Err(ComponentPathResolutionError::StructuralMode {
                kind: child_schema.kind,
                segment: segment.clone(),
            });
        }
        component = *child;
    }
    if components.get(component.index()).is_none() {
        return Err(ComponentPathResolutionError::MissingComponent(component));
    }
    Ok(ResolvedComponentPath {
        component,
        effective_mode,
    })
}

/// Validate that every reachable transactor with a mode-sensitive surface has
/// an effective mode. The traversal is type/mode memoized so malformed
/// recursive schemas cannot make verifier or lowering validation recurse
/// forever.
pub fn validate_component_binding_modes(
    components: &[ComponentSchema],
    bindings: &[ComponentFieldBinding],
) -> Result<(), ComponentPathResolutionError> {
    fn visit(
        components: &[ComponentSchema],
        component: ComponentId,
        inherited: Option<ComponentInstanceMode>,
        path: &mut Vec<String>,
        visited: &mut std::collections::HashSet<(ComponentId, Option<ComponentInstanceMode>)>,
    ) -> Result<(), ComponentPathResolutionError> {
        let schema = components
            .get(component.index())
            .ok_or(ComponentPathResolutionError::MissingComponent(component))?;
        if schema.requires_instance_mode() && inherited.is_none() {
            return Err(ComponentPathResolutionError::UnresolvedMode {
                path: path.join("."),
            });
        }
        if !visited.insert((component, inherited)) {
            return Ok(());
        }
        for field in &schema.fields {
            let ComponentFieldKind::Sub {
                component: child,
                mode: declared_mode,
            } = &field.kind
            else {
                continue;
            };
            path.push(field.name.clone());
            let child_schema = components
                .get(child.index())
                .ok_or(ComponentPathResolutionError::MissingComponent(*child))?;
            if matches!(
                child_schema.instance_mode_policy,
                ComponentInstanceModePolicy::AlwaysOnAnalysisMonitor
            ) && matches!(declared_mode, Some(ComponentInstanceMode::Active))
            {
                return Err(ComponentPathResolutionError::ForbiddenActiveMode {
                    path: path.join("."),
                });
            }
            let resolved = resolve_component_path_mode(
                components,
                component,
                inherited,
                std::slice::from_ref(&field.name),
            )?;
            visit(
                components,
                resolved.component,
                resolved.effective_mode,
                path,
                visited,
            )?;
            path.pop();
        }
        Ok(())
    }

    let mut visited = std::collections::HashSet::new();
    for binding in bindings {
        let schema = components.get(binding.component.index()).ok_or(
            ComponentPathResolutionError::MissingComponent(binding.component),
        )?;
        if matches!(
            schema.instance_mode_policy,
            ComponentInstanceModePolicy::AlwaysOnAnalysisMonitor
        ) && matches!(binding.mode, Some(ComponentInstanceMode::Active))
        {
            return Err(ComponentPathResolutionError::ForbiddenActiveMode {
                path: binding.field.clone(),
            });
        }
        let mut path = vec![binding.field.clone()];
        visit(
            components,
            binding.component,
            binding.mode,
            &mut path,
            &mut visited,
        )?;
    }
    Ok(())
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
    /// The source body that declared this field.
    pub activation: Activation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedVecSchema {
    pub elem: IrType,
    pub len: usize,
}

#[derive(Debug, Clone)]
pub enum ComponentFieldKind {
    /// `count : uint<32> default 0` — a scalar host counter.
    Scalar { ty: IrType, default: u64 },
    /// `words : Vec<uint<64>, 4>` — fixed, by-value component state.
    FixedVec(FixedVecSchema),
    /// `current : Sample` — a value-record held as persistent host-side
    /// component/transactor state.
    Record { record: RecordId },
    /// `expected : queue<uint<32>>` / `errors : queue<CheckerError>` — a
    /// FIFO whose element is an exact scalar through 1024 bits or a
    /// value-record (`elem` selects the C element type). Manipulated through
    /// the component-queue ops (`Stmt::ComponentQueuePush`/
    /// `ComponentQueuePop`, `Expr::ComponentQueueSize`).
    Queue { elem: QueueElem },
    /// `observed : out event<uint<8>>` — an analysis port. Lowers to a
    /// `std::vector<std::function<void(<payload>)>>` member; `payload`
    /// selects the C payload type (scalar `uint64_t`/`int64_t` or a
    /// value-record struct). Sinks subscribe via `connect`.
    Event { payload: EventPayload },
    /// `source : AnalysisSource passive` / `sb : AnalysisSb` — a nested
    /// by-value sub-component. `component` indexes
    /// `TbProgram::components`.
    Sub {
        component: ComponentId,
        /// Explicit nested transactor override, if present in source.
        mode: Option<ComponentInstanceMode>,
    },
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
/// `payload_type_for_arg`: integer scalars use their width-aware carrier,
/// `bool` remains `bool`; a
/// user-named `transaction`/`struct` payload is carried by value as the
/// record struct (`std::function<void(<RecordName>)>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPayload {
    /// `event<uint<8>>` / `event<sint<…>>` / `event<bool>` — a scalar
    /// payload: its signedness, declared width, and boolean identity.
    ///
    /// This was `Scalar { signed: bool }`, which could say `int64_t` or
    /// `uint64_t` and nothing else — so an `event<uint<1024>>` had no
    /// representation and was refused, while v1 emitted
    /// `std::function<void(harc_rt::HarcWide<32>)>` for it. Two sites
    /// already converted the payload back into an `IrType` with a
    /// `None` width to talk to the rest of lowering, one of them
    /// commenting that the param "IS widthless" — a claim that had
    /// spread into two more comments and a test by the time it was
    /// made false.
    ///
    /// Every sibling slot — `QueueElem::Scalar { ty }`,
    /// `StateFieldKind::Scalar { ty }`, `FixedVecSchema { elem }` — carries a
    /// whole `IrType`. This scalar variant keeps the compact fields because
    /// `IrType::Event(EventPayload)` makes an unboxed `IrType` here recursive;
    /// aggregate payloads below use a box to break that cycle.
    Scalar {
        signed: bool,
        width: Option<u32>,
        /// Distinguishes source `bool` from `uint<1>`. Both are one-bit
        /// values, but v1 carries a boolean event through a C++ `bool`
        /// callback and therefore coerces every emitted value to 0/1.
        boolean: bool,
    },
    /// `event<Vec<T, N>>` — a fixed-size vector value. The boxed element
    /// breaks the `IrType::Event(EventPayload)` recursion while retaining the
    /// complete aggregate element shape (including another fixed vector).
    FixedVec { elem: Box<IrType>, len: usize },
    /// `event<TinyTxn>` — a value-record payload. `RecordId` indexes
    /// `TbProgram::records`; the C++ payload type is the record struct.
    Record(RecordId),
}

impl EventPayload {
    /// The payload as an `IrType`, for the lowering sites that type a
    /// handler parameter or an `emit` argument against it. This is the
    /// conversion two call sites already open-coded with a `None`
    /// width; it keeps the declared width now.
    pub fn scalar_ir_type(&self) -> Option<IrType> {
        match self {
            EventPayload::Scalar {
                signed,
                width,
                boolean,
            } => Some(if *boolean {
                IrType::Bool
            } else if *signed {
                IrType::SInt(*width)
            } else {
                IrType::UInt(*width)
            }),
            EventPayload::Record(_) => None,
            EventPayload::FixedVec { .. } => None,
        }
    }

    pub fn value_ir_type(&self) -> IrType {
        match self {
            EventPayload::Scalar { .. } => self.scalar_ir_type().expect("scalar payload"),
            EventPayload::Record(record) => IrType::Record(*record),
            EventPayload::FixedVec { elem, len } => IrType::FixedVec {
                elem: elem.clone(),
                len: *len,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ComponentMethodSchema {
    pub name: String,
    /// Lowered method body (`kind: ComponentMethod`).
    pub function: FunctionId,
    /// Declared parameter types after lowering. Connection resolution uses
    /// this surface before method bodies exist to validate the single
    /// analysis payload accepted by a hookable sink.
    pub param_tys: Vec<IrType>,
    /// Declared parameter NAMES, in order. Same reason as
    /// `TransactorMethodSchema::param_names`: a call site that only has
    /// this schema needs them to tell an inert named argument (name in
    /// its own position, which v1 emits identically) from a reordered
    /// one (which v1 silently swaps). Replaces a separate `n_params`
    /// count so arity and names cannot disagree.
    pub param_names: Vec<String>,
    pub has_ret: bool,
    /// Declared return type after lowering, when present.
    pub ret_ty: Option<IrType>,
    /// True for `hookable` (emits pre/post hook vectors + fan-out);
    /// false for `function` (no hooks). Mirrors v1's
    /// `HookableMethod::is_hookable`.
    pub hookable: bool,
    /// Covergroup auto-samplers that subscribe to this component method's
    /// pre/post hook boundary (`covergroup G @(sb.observe(t) post)`).
    pub cov_hook_subs: Vec<(CovgroupId, crate::ast::HookSide)>,
    /// `when active` provenance for call validation and hook registration.
    pub activation: Activation,
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
    /// `when active` provenance for subscriber registration.
    pub activation: Activation,
}

/// One `connect <src>.<event> -> <sink>.<method|event>` edge inside an
/// env or reusable testbench, resolved to the component paths used by its
/// owning scope.
/// Two sink shapes (see `ConnectSink`):
///   * method sink — `<env>.<src_path>.<event>.push_back([&](auto _t){
///     <SinkComp>_<method>(<env>.<sink_path>, _t); })`
///   * event sink — `<env>.<src_path>.<event>.push_back([&](auto _t){
///     for (auto& _s : <env>.<sink_path>.<event>) _s(_t); })` (forwards
///     into the sink event field's own subscriber list, i.e. an
///     event→event bridge feeding a driver's `in event` handler).
#[derive(Debug, Clone)]
pub struct ConnectEdgeSchema {
    /// Dotted path from the owning scope to the source component. For an
    /// env it is relative to the bound env field; for a testbench it starts
    /// with a testbench-owned component field.
    ///
    /// EMPTY means the owner itself — `own_ev -> sb.write_obs`, where the
    /// event is declared on the env rather than on a sub-component.
    /// Renderers must not `join(".")` this blindly; use
    /// `lower::endpoint_label`.
    pub src_path: Vec<String>,
    /// `out event<T>` field on the source component — a sub-component's,
    /// or the owner's own when `src_path` is empty.
    pub src_event: String,
    /// Activation provenance of the source event endpoint.
    pub src_activation: Activation,
    /// Dotted path to the sink component (`["sb"]`). EMPTY means the
    /// owner itself — `source.observed -> own_sink`. Same rendering
    /// caveat as `src_path`.
    pub sink_path: Vec<String>,
    /// Sink component's schema (to name `<Comp>_<method>`) — a
    /// sub-component's, or the owner's own when `sink_path` is empty.
    pub sink_component: ComponentId,
    /// What the edge feeds on the sink sub-component.
    pub sink: ConnectSink,
    /// Activation provenance of the sink endpoint.
    pub sink_activation: Activation,
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
    /// Concurrent `cover` checks registered anywhere in this test, in
    /// registration order. The end-of-test summary reports exactly these
    /// (v1 clears its `covers` list per test and emits the summary in the
    /// same order). Empty for a test with no `cover` statement.
    pub cover_checks: Vec<CoverCheckId>,
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
    /// Typed FIFO fields owned directly by the reusable testbench. These
    /// share the scalar host object's lifetime but use explicit TB-IR queue
    /// operations so their mutation cannot be confused with a scoreboard or
    /// component queue.
    pub queue_fields: Vec<TbQueueFieldSchema>,
    /// Source-ordered typed host state. The scalar/queue projections above
    /// remain for existing lowering lookups; emitters and dump-ir use this
    /// collection to preserve declaration order across state kinds.
    pub state_fields: Vec<TbStateFieldSchema>,
    /// `connect` edges owned directly by this testbench. Paths are rooted
    /// at testbench component fields and are installed before run/check.
    pub connects: Vec<ConnectEdgeSchema>,
    /// Transaction/struct-typed testbench fields, in declaration order.
    /// These are run/helper/check-shared host records declared once at
    /// test scope and referenced as synthetic record locals in each
    /// owning function.
    pub record_fields: Vec<(String, RecordId)>,
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
    /// declaration order.
    pub transactor_fields: Vec<(String, TransactorId)>,
    /// The subset of `transactor_fields` declared `passive`. A passive
    /// instance exposes only its passive surface — persistent state
    /// fields (and any always-on `on` handlers) — never its `when active`
    /// methods, so the type-shared method bodies are NOT filled with a
    /// passive instance name. Codegen consults this so a transactor type
    /// with ONLY passive instances emits no (unfilled, uncallable) method
    /// lambdas (#494 P0a/P1b). Keyed by field name.
    pub passive_transactor_fields: std::collections::HashSet<String>,
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
    /// Unbound DUT-poking transactor instances that need persistent
    /// storage, in declaration order: either they declare state fields or
    /// an idle/quiesced predicate needs their heartbeat stamps. Each names
    /// an entry in `transactor_fields`; emission generates a per-instance
    /// state struct that method lambdas and run/check share by capture.
    /// Otherwise-stateless instances absent from heartbeat expressions do
    /// not appear here.
    pub unbound_state_actors: Vec<UnboundStateActorSchema>,
    /// True when no `testbench` declaration existed in source and this
    /// schema was synthesized for a classic-form test. Codegen skips
    /// the `_tb` struct + wire statement for synthetic testbenches.
    pub synthetic: bool,
    /// Testbench-scoped `on <N> cycles [phase post_eval] ... end on`
    /// periodic handlers (issue #485). v1 emits these through its
    /// testbench-component path (registering into `_checkers` /
    /// `_post_eval_services`); the TB-IR backend has no testbench
    /// component, so the handler bodies lower to flow-owned
    /// `FunctionKind::TestHook` functions and register here. Each fires
    /// its body once every `period` primary-clock cycles at the recorded
    /// phase. Empty for a testbench without periodic handlers.
    pub periodic_services: Vec<TbPeriodicServiceSchema>,
    /// Testbench-scoped `on <bool-expr> ... end on` cycle-trigger handlers
    /// (issue #494 P2b). v1 emits these through its testbench-component
    /// path (`emit_cycle_trigger` registering into `_checkers`); the TB-IR
    /// backend has no testbench component, so the handler bodies lower to
    /// flow-owned `FunctionKind::TestHook` functions and register here.
    /// Each re-evaluates its predicate every primary-clock cycle and fires
    /// the body when the predicate satisfies the requested edge mode. Empty
    /// for a testbench without cycle-trigger handlers.
    pub cycle_services: Vec<TbCycleServiceSchema>,
}

/// One testbench-scoped `on <N> cycles ... end on` periodic handler
/// (issue #485). Mirrors `PeriodicHandlerSchema` but at flow scope: the
/// body is a flow-owned `FunctionKind::TestHook` function (`_tb`/`dut`
/// captured by reference, no `self`), registered into the per-cycle
/// `_checkers` / `_post_eval_services` vector by the run coroutine's
/// setup. The period is a compile-time integer literal (`on 1 cycles`);
/// a field-backed period is rejected at lowering with a clear message.
#[derive(Debug, Clone)]
pub struct TbPeriodicServiceSchema {
    /// Firing period in primary-clock cycles (a positive literal).
    pub period: u64,
    /// Lowered handler body (`kind: TestHook`, zero params). Field reads
    /// resolve to `Expr::TbField`, DUT reads to the shared `dut` handle,
    /// testbench-method calls inline like `_tb.<m>()`.
    pub function: FunctionId,
    /// `phase` modifier (`on N cycles phase post_eval`). `Checker`
    /// (default) registers into `_checkers`; `PostEval` into
    /// `_post_eval_services`.
    pub phase: HandlerPhase,
}

/// One testbench-scoped `on <bool-expr> ... end on` cycle-trigger handler
/// (issue #494 P2b). Mirrors `CycleTriggerHandlerSchema` but at flow
/// scope: the body is a flow-owned `FunctionKind::TestHook` function
/// (`_tb`/`dut` captured by reference, no `self`), registered into the
/// per-cycle `_checkers` / `_post_eval_services` vector by the run
/// coroutine's setup. The trigger predicate is evaluated standalone in the
/// registration closure every primary-clock cycle; the body fires when the
/// predicate satisfies `edge`.
#[derive(Debug, Clone)]
pub struct TbCycleServiceSchema {
    /// The boolean trigger predicate, lowered in TEST scope (so `dut.<sig>`
    /// reads route to the shared `dut` handle and bare field reads resolve
    /// to `_tb.<field>`). Rendered standalone inside the registration
    /// closure.
    pub trigger: Expr,
    /// Edge mode: `Rising` (0→1, default), `Falling` (1→0), or `Level`
    /// (every cycle the predicate holds).
    pub edge: CycleEdge,
    /// Lowered handler body (`kind: TestHook`, zero params). Field reads
    /// resolve to `Expr::TbField`, DUT reads to the shared `dut` handle.
    pub function: FunctionId,
    /// `phase` modifier (`on <expr> phase post_eval`). `Checker` (default)
    /// registers into `_checkers`; `PostEval` into `_post_eval_services`.
    pub phase: HandlerPhase,
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
    /// Explicit mode on this root binding. On a transactor this is its
    /// effective mode; on a structural root it is inheritance context for
    /// nested transactors.
    pub mode: Option<ComponentInstanceMode>,
}

/// One bound-to target-side TLM responder instance (`let target :
/// MemTarget passive = bind mem`). The actor coroutines serve the bound
/// bus binding's blocking req/rsp wire protocol on the DUT.
#[derive(Debug, Clone)]
pub struct TargetTlmActorSchema {
    /// Instance name (the per-instance struct + actor prefix).
    pub instance: String,
    /// The test-scope bus-binding field this responder serves — also
    /// the flat DUT signal prefix (`mem` → `mem_read_req_valid`, ...).
    pub bus_field: String,
    /// The bound-to transactor type providing the responder bodies and
    /// state fields (`TbProgram::transactors[transactor]`).
    pub transactor: TransactorId,
    /// Component IR host for a mixed bound transactor that also declares
    /// `on` handlers. `None` means the actor owns its generated target-state
    /// object; `Some(c)` means the matching component field owns storage.
    pub host_component: Option<ComponentId>,
    /// Effective source binding mode. Standalone target responders are
    /// currently passive; a mixed component-hosted responder may be active.
    pub active: bool,
}

/// Per-instance storage for an unbound DUT-poking or initiator transactor.
/// `field` is the source-level receiver used for resolution; `storage` is
/// the C++ identifier emitted for the state object. They are deliberately
/// distinct so a source field cannot collide with a generated method slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnboundStateActorSchema {
    pub field: String,
    pub transactor: TransactorId,
    pub storage: String,
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
    /// one-param (`data : uint`) `FunctionKind::TestHook` function;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TbScalarFieldSchema {
    pub name: String,
    pub ty: IrType,
    /// Declared `default <lit>` value, or 0 (v1's fallback).
    pub default: u64,
}

/// One testbench-owned FIFO (`pending : queue<uint<32>>` or
/// `pending : queue<Record>`). Scalar elements retain their exact type through
/// the 1024-bit language ceiling. Queue elements reuse the shared queue shape
/// used by scoreboards, components, and transactor state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TbQueueFieldSchema {
    pub name: String,
    pub elem: QueueElem,
}

/// One source-ordered testbench host-state declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TbStateFieldSchema {
    Scalar(TbScalarFieldSchema),
    Queue(TbQueueFieldSchema),
}

/// One persistent state field of a transactor. Scalar, queue, record, and
/// fixed-vector state reuse the same value schemas carried by component
/// fields so the three transactor owner forms share one runtime layout.
#[derive(Debug, Clone)]
pub struct StateFieldSchema {
    pub name: String,
    pub kind: StateFieldKind,
}

/// The kind of a target-transactor persistent state field.
#[derive(Debug, Clone)]
pub enum StateFieldKind {
    /// `read_count : uint<32> default 0` — a scalar host counter/latch.
    /// `default` is the declared initializer literal (0 fallback).
    Scalar { ty: IrType, default: u64 },
    /// `pending : queue<uint<32>>` / `pending : queue<Record>` — a FIFO
    /// whose element is an exact scalar type through 1024 bits or a
    /// value-record. Manipulated through the state-queue ops
    /// (`Stmt::TransactorStateQueuePush`/`TransactorStateQueuePop`,
    /// `Expr::TransactorStateQueueQuery`).
    Queue { elem: QueueElem },
    /// `last : Beat` — a whole value-record held as persistent state.
    /// `RecordId` indexes `TbProgram::records`; the C++ member is the
    /// record struct by value (mirroring the scoreboard/component record
    /// machinery). Fields are read/written through the state-record ops
    /// (`Expr::TransactorStateRecordField`, `Stmt::TransactorStateRecord`-
    /// `FieldWrite`); the whole record is read/copied through the scalar
    /// `Expr::TransactorState` / `Stmt::TransactorStateWrite` forms.
    Record { record: RecordId },
    /// `lanes : Vec<T, N>` — a fixed-size aggregate held by value.
    /// The complete `IrType::FixedVec` preserves nested-vector and record
    /// element metadata for indexed access and whole-value copies.
    FixedVec { ty: IrType },
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
    SamplerAuto {
        covgroup: CovgroupId,
    },
    Helper,
    /// One transactor method body (design doc §"Function-kind
    /// handling": one `TbFunction` per method). `params` are the
    /// method's declared parameters; the owning transactor and the
    /// method name live in `TbProgram::transactors[transactor]`.
    TransactorBody {
        transactor: TransactorId,
    },
    /// One composite-component method body (env/agent cluster). `params`
    /// are the method's declared parameters; bodies address fields self-
    /// relatively via `ComponentBase::SelfField`. The owning component +
    /// method name live in `TbProgram::components[component]`. Emitted as
    /// a `<Comp>_<method>(<Comp>& self, args)` free lambda (v1's
    /// `emit_component_method` shape).
    ComponentMethod {
        component: ComponentId,
    },
    /// One `tseq` declaration body — a transaction-sequence generator.
    /// `elem` is the element type (`TSeq<Record>` → `TseqElem::Record`,
    /// `TSeq<scalar>` → `TseqElem::Scalar`); the function's `ret` slot
    /// holds the matching `RecordSeq`/`Seq` accumulator, each `yield`
    /// pushes onto it (`Stmt::SeqPush`), and `Terminator::Return` returns
    /// it. Emitted as a `[&]`-capturing lambda returning `std::vector<T>`
    /// (v1's `emit_tseq` shape). Called via `CallTarget::Tseq` from a
    /// test-scope `let txns = Name(args)`.
    Tseq {
        elem: TseqElem,
    },
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
    /// A reusable testbench lifecycle phase body — `setup`/`check`/
    /// `teardown` declared on a bound `testbench`. Owned by the
    /// testbench, NOT by any one bound test: the intent (issue #619
    /// M4a) is to lower each phase ONCE and invoke it from every owning
    /// test via an explicit call edge, in the order of issue #619
    /// §"Lifecycle Ordering". `testbench` indexes
    /// `TbProgram::testbenches`.
    ///
    /// Not yet produced by lowering: the tbir path still reuses v1's
    /// lifecycle-copying `desugar_impl_for_test_in_file`, which inlines
    /// these bodies into each test's `Run`/`Check` before the IR is
    /// built. The native lowering that constructs this variant lands in
    /// a later M4a slice — see docs/619-m4a-ir-ownership.md. Until then
    /// the variant is part of the IR vocabulary only; no emitter path
    /// consumes it.
    TestbenchLifecycle {
        testbench: TestbenchId,
        phase: crate::ast::LifecyclePhase,
    },
    /// A reusable testbench helper method body, owned by the testbench
    /// and shared across every bound test (issue #619 M4a). Same
    /// not-yet-produced status as [`FunctionKind::TestbenchLifecycle`]:
    /// today such a method is inlined per test by the desugaring.
    /// `testbench` indexes `TbProgram::testbenches`.
    TestbenchMethod {
        testbench: TestbenchId,
    },
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
    /// A scalar-element transaction-sequence local (`let xs = SomeTseq(...)`
    /// declared `-> TSeq<uint<N>>`). Emitted as `std::vector<T>` where `T`
    /// is the boxed scalar element's C++ scalar type — the scalar analogue
    /// of `RecordSeq`. Built by a `FunctionKind::Tseq` body (each `yield e`
    /// is a `Stmt::SeqPush` of a scalar value), consumed by `for x in <seq>`
    /// iteration (`Expr::SeqLen` + `Expr::SeqIndex`, both element-agnostic).
    /// The boxed element is always a scalar (`UInt`/`SInt`/`Bool`); a record
    /// element uses `RecordSeq` instead.
    Seq(Box<IrType>),
    /// A fixed-length vector value — the element type of a nested
    /// `Vec<Vec<T, N>, M>` component field, and the type an index into
    /// its outer dimension (`v[i]`) yields. Emitted as v1 emits it: a
    /// `std::array<elem, len>` (which nests when `elem` is itself a
    /// `FixedVec`). `FixedVecSchema.elem` holds one of these for a
    /// nested field; a single-level `Vec<T, N>` field keeps a scalar
    /// `elem` and never constructs this. Recursive like `Seq`, which is
    /// why `IrType` is not `Copy`.
    FixedVec {
        elem: Box<IrType>,
        len: usize,
    },
    /// A composite-component value local — a method parameter typed by a
    /// component name (`observe(addr: uint<8>, model: ProtocolModel)`).
    /// Taken by value as the component's C++ struct; method calls on it
    /// (`model.predict_read(addr)`) dispatch through `ComponentBase::Local`.
    /// Only ever a parameter local in this subset — never a `let` body
    /// local — so it has no value-construction or randomize support.
    Component(ComponentId),
    /// Internal carrier for a whole DUT port sampled before an impure lane
    /// selector. Codegen declares it with `auto` at the `DutRead` site so
    /// packed-wide and native-array port shapes retain their exact C++ type.
    PortSnapshot,
    /// A TEST-SCOPE event channel local (`let e : event<uint<8>>`,
    /// spec §3.4). Emitted as v1 emits it: a
    /// `std::vector<std::function<void(<payload>)>>` local in the
    /// enclosing coroutine. `on e(v) ... end on` pushes a subscriber
    /// (`Stmt::EventSubscribe`), `emit e(x)` fans out synchronously
    /// (`Stmt::EventEmit`). Distinct from a component's `in`/`out
    /// event<T>` FIELD, which lives on the component struct and is
    /// reached through `ComponentEmit` / the connect graph.
    Event(EventPayload),
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
    /// Return blocks synthesized from a source body's natural fallthrough.
    /// Explicit source `return` statements also lower to
    /// `Terminator::Return`, so this provenance lets hook-aware emitters
    /// preserve v1's distinction: post hooks run after natural completion
    /// but are bypassed by an explicit early return.
    pub implicit_returns: Vec<BlockId>,
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
    /// Re-default-construct an aggregate local whose complete type is carried
    /// by the local table. Currently used for fixed-vector return slots on
    /// CFG-inlined testbench methods.
    AggregateInit(LocalId),
    /// `t.field = value` on a record-typed local. The value is
    /// port-hoisted like `Assign` (no inline DUT reads). `index: Some(e)`
    /// writes a `Vec<T, N>` field element (`rec.data[e] = value`); `None`
    /// writes a scalar (or whole-`Vec` / whole-nested-record) field.
    ///
    /// `path` carries zero or more FURTHER nested field names beneath the
    /// first-level `field`, for a nested-struct write such as
    /// `s.a.b = v` (`field = "a"`, `path = ["b"]`). The written leaf is
    /// the last of `[field] ++ path`; `index` (when `Some`) indexes that
    /// leaf's `Vec`. An empty `path` is the single-level write. A
    /// whole-nested-record leaf assignment (`o.a = d`) carries a
    /// record-valued `value`.
    ///
    /// `mid_indices` carries selections embedded in the member path. On a
    /// non-leaf `Vec<Record, N>` segment it selects the record element to
    /// traverse (`tbl.entries[i].tag`). Repeated entries at the leaf position
    /// carry all but the final selection of a nested fixed vector
    /// (`grid[i][j]`); `index` carries the final `[j]`. Positions are
    /// nondecreasing and indexes at one position retain source order.
    RecordFieldWrite {
        local: LocalId,
        field: String,
        path: Vec<String>,
        mid_indices: Vec<(usize, Expr)>,
        index: Option<Expr>,
        value: Expr,
    },
    /// `let value = regs.record_read(addr)` with a runtime address. The
    /// regblock schema supplies the source-ordered address decoder; an
    /// unmatched address reads as zero, matching v1's generated helper.
    RecordRead {
        dest: LocalId,
        local: LocalId,
        regblock: RegblockId,
        addr: Expr,
    },
    /// `regs.record_write(addr, data)` with a runtime address. The owner
    /// testbench binding supplies callback registration metadata; the
    /// regblock schema supplies the source-ordered offset/width decoder.
    RecordWrite {
        local: LocalId,
        binding: String,
        regblock: RegblockId,
        addr: Expr,
        value: Expr,
    },
    /// `regs.record_write(addr, data)` where the binding has a per-register
    /// `on regs.REG` write callback registered — the firing form of the
    /// passive RAL record API. The address was const-decoded at lowering to
    /// the single target register, so the mirror update is a `RecordFieldWrite`
    /// in disguise (same `local`/`field`/masked `value`); the addition is the
    /// recursion-depth guard + callback dispatch. Emission mirrors v1's
    /// `try_emit_record_write`: bump `<binding>_cb_depth`, emit FATAL and bump
    /// the framework error counter past `HARC_RAL_CB_MAX_DEPTH`, else write
    /// the mirror cell and (when `callback` is `Some`) call the callback with
    /// the observed value, then un-bump. A `record_write` whose register has NO
    /// callback but whose BINDING has callbacks on OTHER registers still
    /// routes here (carrying `callback: None`) so its mirror write is
    /// depth-counted consistently (matches v1, which wraps the whole decode
    /// chain in one guard).
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
    TbFieldWrite {
        field: String,
        value: Expr,
    },
    /// `_tb.<field>[i] = value` — element write of a fixed-vector
    /// testbench field. See `Expr::TbFieldVecElement`.
    TbFieldVecElementWrite {
        field: String,
        index: Expr,
        inner_index: Option<Expr>,
        value: Expr,
    },
    /// `_tb.<field>.push(value)` on a testbench-owned typed FIFO.
    TbQueuePush {
        field: String,
        value: Expr,
    },
    /// `let value = _tb.<field>.pop()` on a testbench-owned typed FIFO.
    /// A bare `<field>.pop()` in statement position lowers here too,
    /// with `dest` a synthesized temp nothing reads — the CALL is the
    /// effect. A pass that eliminates dead stores must therefore treat
    /// every queue pop as live regardless of its destination.
    TbQueuePop {
        field: String,
        dest: LocalId,
    },
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
    /// `last.addr = addr` inside a target-responder body (or
    /// `responder.last.addr = ...` from the test): write a SUB-FIELD of a
    /// bound-to target transactor's persistent whole-record state field.
    /// `instance` names the bound testbench-field instance (placeholder in
    /// a method body, filled at test-binding), `field` the record state
    /// field, `path` the nested field chain to the written leaf. An empty
    /// path denotes a direct fixed-vector state element, with outer recursive
    /// selections retained as position-zero `mid_indices`. Record-state paths
    /// have length ≥ 1 (e.g. `["addr"]`, or `["a","b"]`). Emission
    /// produces `<instance>.<field>.<path…> = <value>`. The value is
    /// port-hoisted like `Assign`. Mirrors `Stmt::RecordFieldWrite` but
    /// against the per-instance state struct instead of a `LocalId`.
    TransactorStateRecordFieldWrite {
        instance: String,
        field: String,
        path: Vec<String>,
        mid_indices: Vec<(usize, Expr)>,
        index: Option<Expr>,
        value: Expr,
    },
    /// `pending.push(x)` inside a target-responder body (or
    /// `target.pending.push(x)` from the test): push onto a bound-to
    /// target transactor's persistent `queue<T>` state field. `instance`
    /// names the bound testbench-field instance (placeholder in a method
    /// body, filled at test-binding); emission produces
    /// `<instance>.<field>.push(<value>)`. A record value lowers to a
    /// struct push, mirroring `Stmt::ComponentQueuePush`.
    TransactorStateQueuePush {
        instance: String,
        field: String,
        value: Expr,
    },
    /// `let v = pending.pop()` — pop the state queue front into a local.
    /// Always has a destination; for a discarded `pending.pop()` that is
    /// an unread temp, so the store is dead but the pop is not (see
    /// `TbQueuePop`). Emitted as `<dest> = <instance>.<field>.pop();`.
    TransactorStateQueuePop {
        instance: String,
        field: String,
        dest: LocalId,
    },
    Log {
        level: LogLevel,
        args: FmtArgs,
    },
    AssertCheck {
        cond: Expr,
        on_fail: FmtArgs,
    },
    /// `assume <plain bool>` — the IMMEDIATE, point-in-time form of an
    /// assumption. Logs `ASSUME` when the predicate is false and, unlike
    /// `AssertCheck`, does NOT bump the error counter: an assumption
    /// bounds the inputs the test is meaningful over, it is not a DUT
    /// bug (v1's `emit_inline_assume`). The concurrent form
    /// (`assume` over a named property or a temporal expression) is a
    /// `PropertyCheck` with `PropertySeverity::AssumeFail` instead.
    AssumeCheck {
        cond: Expr,
        on_fail: FmtArgs,
    },
    /// Register the concurrent property check at `TbProgram::property_checks`
    /// index — a `_checkers` closure evaluated once per primary-clock edge
    /// from this statement's position onward. Mirrors v1, which pushes the
    /// closure inline where the `assert`/`assume` statement appears, so a
    /// check declared after a `wait` never observes the earlier cycles.
    PropertyCheck(PropertyCheckId),
    /// Register the concurrent `cover` witness counter at
    /// `TbProgram::cover_checks` index. Same per-cycle registration
    /// discipline as `PropertyCheck`; the counter itself is a file-scope
    /// `static` so the end-of-test summary can read it.
    CoverCheck(CoverCheckId),
    /// Arm the statement-position `on` handler at
    /// `TbProgram::cycle_handlers` index — a `_checkers` /
    /// `_post_eval_services` closure installed at this statement's
    /// position, exactly where v1's `emit_cycle_trigger` pushes it.
    CycleHandler(CycleHandlerId),
    /// `on e(v) ... end on` on a test-scope event local, or
    /// `on comp.event(v)` on a component event field — push a subscriber
    /// onto the channel. `handler` is a one-parameter
    /// `FunctionKind::TestHook` function (the payload is its parameter),
    /// declared at test scope so the pushed closure outlives the block
    /// that registered it.
    EventSubscribe {
        event: EventChannelRef,
        handler: FunctionId,
    },
    /// Register a pre/post subscriber on a hookable method at this
    /// statement's runtime position.
    MethodHookSubscribe {
        target: MethodHookTarget,
        side: crate::ast::HookSide,
        handler: FunctionId,
        /// Enclosing flow locals captured by reference at this registration
        /// point. The handler's trailing parameters have matching types and
        /// are emitted as C++ reference parameters.
        captures: Vec<LocalId>,
    },
    /// `emit e(x)` on a test-scope event local — call every subscriber
    /// synchronously, in subscription order. Mirrors v1's
    /// `for (auto& _s : e) _s(x);`. A channel with no subscribers is a
    /// no-op, as in v1.
    EventEmit {
        event: LocalId,
        args: Vec<Expr>,
    },
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
    /// Method-body-only sibling call on the current DUT-poking
    /// transactor. Carries the same optional result slot as
    /// `TransactorCall`, but resolves through the enclosing
    /// `FunctionKind::TransactorBody` instead of a testbench field.
    TransactorSelfCall {
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
    FailDiag {
        guard: Option<Expr>,
        args: FmtArgs,
    },
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
    /// Write a composite-component scalar/record field, or a whole fixed
    /// vector in the same-shape copy position. `base` selects the
    /// access form: `SelfField` (`count = count + 1` inside a method body
    /// → `self.count = ...`) or `Path` (`env.sb.errors = ...` from the
    /// test). The value is port-hoisted like `Assign`.
    ComponentFieldWrite {
        base: ComponentBase,
        field: String,
        value: Expr,
    },
    ComponentVecElementWrite {
        base: ComponentBase,
        field: String,
        index_pos: usize,
        index: Expr,
        /// Second index for a nested vector write (`v[i][j] = x`). See
        /// `Expr::ComponentVecElement::inner_index`.
        inner_index: Option<Expr>,
        value: Expr,
    },
    /// `emit observed(v)` — fan the args out to every callback registered
    /// on the named `out event<T>` field of the component named by `base`.
    /// `base = SelfField` for a self-relative `emit observed(v)` inside a
    /// method body (`self.<event>`); `base = Path([...])` for a test-scope
    /// or self-rooted child path; `base = Local` plus `subpath` for a
    /// component-typed parameter receiver. Emitted as
    /// `for (auto& _s : <base>.<event>) _s(args);` plus v1's
    /// `_last_out_cycle` heartbeat bump on the emitting component.
    ComponentEmit {
        base: ComponentBase,
        /// Sub-components below `base`. Normally empty because path bases
        /// already carry their receiver; used when `base` is a component-
        /// typed local and the event lives below that parameter.
        subpath: Vec<String>,
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
    /// Always has a destination; for a discarded `<queue>.pop()` that is
    /// an unread temp, so the store is dead but the pop is not (see
    /// `TbQueuePop`). Emitted as `<dest> = <recv>.<queue>.pop();`.
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
    SeqPush {
        seq: LocalId,
        value: Expr,
    },
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

/// Event channel targeted by a statement-position subscription.
#[derive(Debug, Clone)]
pub enum EventChannelRef {
    /// A test-scope `let e : event<T>` local.
    Local(LocalId),
    /// An event field on a test-scope component path (`env.src.obs`).
    Component {
        base: ComponentBase,
        /// Resolved schema of the component that owns `event`.
        component: ComponentId,
        event: String,
        payload: EventPayload,
    },
}

/// Hookable method targeted by a statement-position subscription.
#[derive(Debug, Clone)]
pub enum MethodHookTarget {
    /// A method on a direct transactor testbench field. Transactor hook
    /// vectors are type-scoped, matching v1's `<Type>_<method>_pre/post`.
    Transactor {
        field: String,
        transactor: TransactorId,
        method: String,
    },
    /// A hookable method on a component path (`env.source.publish`).
    Component {
        base: ComponentBase,
        component: ComponentId,
        method: String,
    },
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
    /// destination; for a discarded `sb.q.pop()` that is an unread temp,
    /// so the store is dead but the pop is not (see `Stmt::TbQueuePop`).
    QueuePop { queue: String, dest: LocalId },
    /// `sb.<value> = value` — write a scalar counter or whole-record field.
    /// The historical variant name remains to avoid duplicating the common
    /// scoreboard receiver/path machinery.
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
    File {
        path: String,
        level: FileLogLevel,
    },
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
    /// #619 M4a: re-inline a once-lowered reusable testbench lifecycle
    /// phase body at this point, then resume at `succ`. `function` indexes
    /// a [`FunctionKind::TestbenchLifecycle`] function that was lowered
    /// ONCE for the bound testbench (shared across every test that binds
    /// it). Unlike a normal call, the tbir emitter EXPANDS the callee's
    /// loop-switch inline here (a new emitter capability — see
    /// docs/619-m4a-ir-ownership.md) so the semantic TRACE is identical to
    /// the historical per-test lifecycle inlining. The generated C++ is
    /// trace-identical, not byte-identical — the re-inlined body is a nested
    /// loop-switch with its own block-scoped local names. It is a
    /// terminator (not a `Stmt`) because the callee body may suspend
    /// (lifecycle `wait`s) — sync points are terminators (invariant 7).
    /// Only produced under the `HARC_TBIR_NATIVE_LIFECYCLE` switch.
    TbLifecycleCall {
        function: FunctionId,
        succ: BlockId,
    },
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

/// A read-only query on a dynamic `list<T>` record field. Source `.len()`
/// and `.size()` share the same element-count operation; `.empty()` is
/// boolean-valued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DynamicListQuery {
    Size,
    Empty,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Literal {
        value: u64,
        ty: IrType,
    },
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
    /// emitted as `local.field[e]`); `None` reads a scalar field (or a
    /// whole nested-record field, `let d = s.inner`).
    ///
    /// `path` carries zero or more FURTHER nested field names beneath the
    /// first-level `field`, for a nested-struct read such as `s.a.b`
    /// (`field = "a"`, `path = ["b"]`). The read leaf is the last of
    /// `[field] ++ path`; `index` (when `Some`) indexes that leaf's
    /// `Vec`.  An empty `path` is the single-level read.
    ///
    /// `mid_indices` carries selections embedded in the member path. On a
    /// non-leaf `Vec<Record, N>` segment it selects the record element to
    /// traverse (`tbl.entries[i].tag`). Repeated entries at the leaf position
    /// carry all but the final selection of a nested fixed vector
    /// (`grid[i][j]`); `index` carries the final `[j]`. Positions are
    /// nondecreasing and indexes at one position retain source order.
    RecordField {
        local: LocalId,
        field: String,
        path: Vec<String>,
        mid_indices: Vec<(usize, Expr)>,
        index: Option<Box<Expr>>,
    },
    /// `_tb.<field>` read on a scalar testbench field. Host state —
    /// allowed in every expression position a `Local` is.
    TbField(String),
    /// `_tb.<field>[i]` — element read of a fixed-vector testbench field
    /// (`mem : Vec<T, N>`). `index` selects the outer dimension;
    /// `inner_index` a nested `Vec<Vec<..>>`'s inner dimension
    /// (`mem[i][j]`), `None` for a single-level read. Mirrors the
    /// component `ComponentVecElement` shape; a distinct node because the
    /// receiver is a `_tb` field name, not a `ComponentBase`.
    TbFieldVecElement {
        field: String,
        index: Box<Expr>,
        inner_index: Option<Box<Expr>>,
    },
    /// A temporal reading (`past`/`rose`/`fell`/`stable`) of latch slot
    /// `slot` in the enclosing concurrent check's `temporals` list.
    ///
    /// Only legal inside a `PropertyCheckSchema::shape` or a
    /// `CoverCheckSchema::cond` — those are the only bodies the backend
    /// wraps in a per-cycle closure that owns the latch cells. The
    /// verifier enforces that (a stray `TemporalSlot` in a run-function
    /// expression would render a reference to an undeclared C++ local).
    TemporalSlot {
        slot: u32,
        kind: TemporalFn,
    },
    /// `_tb.<field>.size()` / `_tb.<field>.empty()` on a testbench-owned
    /// typed FIFO. `pop()` mutates, so it lowers only as `Stmt::TbQueuePop`.
    TbQueueQuery {
        field: String,
        query: ScoreboardQuery,
    },
    /// A read of a bound-to target transactor's persistent state field,
    /// either from a responder body (`read_count`) or from the test
    /// (`target.read_count`). `instance` is the bound testbench-field
    /// instance name (e.g. `target`); emission produces
    /// `<instance>.<field>` against the generated per-instance struct.
    /// Host state — allowed in every position a `Local` is.
    TransactorState {
        instance: String,
        field: String,
    },
    /// A read of a SUB-FIELD of a bound-to target transactor's persistent
    /// whole-record state field: `last.data` in a responder body, or
    /// `responder.last.data` from the test. `instance` names the bound
    /// testbench-field instance (placeholder in a method body, filled at
    /// test-binding), `field` the record state field, `path` the nested
    /// field chain to the read leaf. An empty path denotes a direct
    /// fixed-vector state element, with outer recursive selections retained
    /// as position-zero `mid_indices`. Emission produces
    /// `<instance>.<field>.<path…>`. Host state — allowed wherever a
    /// `Local` is. Mirrors `Expr::RecordField` but against the per-
    /// instance state struct instead of a `LocalId`.
    TransactorStateRecordField {
        instance: String,
        field: String,
        path: Vec<String>,
        mid_indices: Vec<(usize, Expr)>,
        index: Option<Box<Expr>>,
    },
    /// A value-producing read on a bound-to target transactor's
    /// persistent `queue<T>` state field: `pending.size()` /
    /// `pending.empty()`. Host state — allowed wherever a `Local` is.
    /// `.pop()` is NOT here (it mutates) — it lowers to
    /// `Stmt::TransactorStateQueuePop`. Mirrors `ScoreboardQuery` /
    /// `ComponentQueueQuery` (`query` carries `QueueSize`/`QueueEmpty`).
    /// `instance` names the bound testbench-field instance (placeholder
    /// in a method body, filled at test-binding).
    TransactorStateQueueQuery {
        instance: String,
        field: String,
        query: ScoreboardQuery,
    },
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
        /// Verification replays the component path and requires its terminal
        /// leaf to reference the carried scoreboard id (the board lives
        /// inside the env, not on `_tb`).
        nested_path: Option<Vec<String>>,
    },
    /// Read a composite-component scalar/record field, or a whole fixed
    /// vector in a same-shape equality/copy position. `base` is `SelfField`
    /// (`count` inside a method → `self.count`) or `Path` (`env.sb.count`
    /// from the test → `env.sb.count`). Host state; whole vectors are limited
    /// to the explicitly shape-checked positions. Queue/event fields are
    /// never read this way (queues use
    /// scoreboard-style ops which are out of subset for components in v0;
    /// events are written via `connect`/`emit` only).
    ComponentField {
        base: ComponentBase,
        field: String,
    },
    ComponentVecElement {
        base: ComponentBase,
        field: String,
        index_pos: usize,
        index: Box<Expr>,
        /// Second index for a NESTED vector element (`v[i][j]`, where
        /// `v : Vec<Vec<T, N>, M>`): `index` selects the outer
        /// dimension, `inner_index` the inner. `None` for an ordinary
        /// single-level `v[i]`. `index_pos` spreads indices across
        /// dotted member SEGMENTS and cannot carry a second index on
        /// the same leaf, which is why this rides alongside it.
        inner_index: Option<Box<Expr>>,
    },
    /// A whole composite-component value, passed by value as a method
    /// argument: `sb.observe(addr, model)` reads `model` here, where the
    /// callee parameter is component-typed. `base` resolves the receiver
    /// (a `self` sub-component field, or a test-scope component path).
    /// Emitted as the C++ struct value at `base` (a by-value copy at the
    /// call), mirroring v1's `ResponseScoreboard_observe(..., model)`.
    ComponentValue {
        base: ComponentBase,
    },
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
    /// `.len()` / `.size()` / `.empty()` on a dynamic `list<T>` record
    /// field. `target` is one of the whole-list host-state field reads
    /// (`RecordField`, `ComponentField`, `ComponentVecElement`, or
    /// `TransactorStateRecordField`). The verifier proves that it still
    /// resolves to a dynamic list after IR-mutating passes.
    DynamicListQuery {
        target: Box<Expr>,
        query: DynamicListQuery,
    },
    /// A heartbeat-idle predicate on a component: `agent.idle_in(N)`,
    /// `agent.idle_out(N)`, or `agent.idle(N)` (= both). `base` resolves
    /// the component instance; `n` is the cycle-count threshold. Reads the
    /// auto-injected `_last_in_cycle`/`_last_out_cycle` stamps against the
    /// global `cycle_count` (v1's `emit_idle_predicate`). Boolean-valued —
    /// allowed wherever a `Local` is.
    ComponentIdle {
        base: ComponentBase,
        /// Sub-component member path relative to `base`. Empty for a
        /// predicate on the receiver itself; populated by `quiesced` when
        /// it expands a component-local receiver into its leaf components.
        subpath: Vec<String>,
        kind: IdleKind,
        n: Box<Expr>,
    },
    /// A heartbeat-idle predicate on a direct transactor instance:
    /// `drv.idle_in(N)`, `drv.idle_out(N)`, `drv.idle(N)`, or
    /// `drv.quiesced(N)` (the latter is equivalent to `idle(N)` for a
    /// transactor, which has no nested component leaves in this schema).
    /// `field` and `transactor` are carried together so verification can
    /// prove the owning testbench still binds that field to that schema
    /// after an IR-mutating pass. The threshold reads the same activity
    /// stamps already emitted on every transactor state struct.
    TransactorIdle {
        field: String,
        transactor: TransactorId,
        /// Verified C++ state-object symbol. This differs from `field` for
        /// demand-created storage on otherwise-stateless transactors.
        storage: String,
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
    /// `AssertCheck`/`Log(error|fatal)`), not a user local. Codegen
    /// emits the framework counter directly so user locals named
    /// `errors` cannot shadow failure accounting. Canonical use:
    /// `assert errors == 0` after a compile-time-unrolled walk like
    /// `bitbash(regs)`. int-valued.
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
    /// Constant bit slice `expr[hi:lo]` over a scalar expression. The
    /// lowered subset requires literal bounds with `hi >= lo` and emits
    /// a right-shift plus mask, mirroring v1's scalar slice behavior.
    BitSlice {
        target: Box<Expr>,
        hi: u32,
        lo: u32,
    },
    /// Runtime bit slice `expr[hi:lo]` where at least one bound is not a
    /// literal — `x[i:0]`, `x[hi:hi-3]`. The width is not known at
    /// lowering, so this cannot fold into a shift-and-mask; it emits the
    /// runtime `harc_rt::harc_bits(value, hi, lo)` helper (the same one
    /// v1 emits), which yields 0 for an out-of-range or reversed bound
    /// rather than shifting by an undefined amount. Value type is
    /// `uint64_t`, matching the helper's return.
    BitSliceDyn {
        target: Box<Expr>,
        hi: Box<Expr>,
        lo: Box<Expr>,
    },
    /// Lane read from a previously sampled whole DUT port. `port` retains
    /// the original signal identity solely for packed-lane width metadata;
    /// the runtime value is read from `snapshot`, never from the DUT again.
    PortSnapshotLane {
        snapshot: LocalId,
        port: PortRef,
        index: Box<Expr>,
    },
    /// Width-method intrinsic (`.trunc<N>()` / `.zext<N>()` /
    /// `.sext<N>()` / `.resize<N>()`), with destinations through the
    /// 1024-bit language limit. `src_width` is the best-effort receiver
    /// width inferred at lowering (typed `let`, `as uint<W>` cast,
    /// nested width method, literal) — it selects v1's emission shape
    /// for `sext`/`resize` and `None` selects v1's unknown-width fallback.
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
    /// `t.<field>` read on a hook-triggered covergroup's hook parameter.
    /// Cover-target-only: a `covergroup G @(drv.send(t) post)` may sample
    /// `cover t.burst`, where `t` is the hookable method's parameter (a
    /// record/transaction value), not a DUT port. Covergroup schemas lower
    /// before the transactor tables exist, so the parameter has no
    /// resolvable `LocalId` yet — the parameter NAME (matching the hook
    /// method's by-value closure arg) and the field are carried instead.
    /// `index: Some(e)` reads a `Vec<T, N>` field element (`t.data[e]`).
    /// Emitted by the tbir hook-sampler closure as `<param>.<field>`,
    /// mirroring v1's `emit_expr` over the closure's by-value param.
    CovHookParam {
        param: String,
        field: String,
        index: Option<Box<Expr>>,
    },
    /// Bare hook parameter sampled by a hook-triggered covergroup:
    /// `covergroup G @(sb.check(t, cycle_seen) post)` may use
    /// `cover cycle_seen` or pass `cycle_seen` into a pure helper call.
    /// Like `CovHookParam`, this is schema-lowered before method params
    /// have `LocalId`s, so it carries the closure parameter name.
    CovHookArg {
        param: String,
    },
    /// `<seq>.size()` — element count of a `RecordSeq` local, used as the
    /// upper bound of a `for t in <seq>` loop. Lowers to `uint64_t`
    /// (emitted as `<seq>.size()`).
    SeqLen(LocalId),
    /// `<seq>[<index>]` — the record value at `index` in a `RecordSeq`
    /// local. Record-valued (allowed wherever a record `Local` is —
    /// notably the `Stmt::Assign` that binds the `for t in <seq>` loop
    /// variable). Emitted as `<seq>[<index>]`.
    SeqIndex {
        seq: LocalId,
        index: Box<Expr>,
    },
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

/// Which heartbeat stamp(s) a component/transactor idle predicate reads.
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
    /// `sb.<value>` — read a scalar counter or whole-record field.
    Scalar { scalar: String },
    /// `sb.<queue>.size()` — element count (lowers to `uint64_t`).
    QueueSize { queue: String },
    /// `sb.<queue>.empty()` — true when no elements (lowers to `bool`).
    QueueEmpty { queue: String },
}

#[derive(Debug, Clone)]
pub enum CallTarget {
    /// Pure helper call plus the declared return type used by caller-side
    /// local inference and signed/width-sensitive expression emission.
    Helper {
        name: String,
        ret: IrType,
    },
    Builtin(String),
    /// Call to a `extern function name(...) -> ret` (spec §9) — a C
    /// reference model linked in via `--ref-src`. Emitted with the RAW
    /// symbol name (no `harc_helper_` mangling) so it resolves against
    /// the user-provided `extern "C"` definition; the forward
    /// declaration is emitted file-scope by `emit_extern_fn_decls`.
    ExternFn {
        name: String,
        ret: IrType,
    },
    TransactorMethod {
        bus_field: String,
        method: String,
    },
    /// Synchronous call from one method of a DUT-poking transactor to
    /// another method on the same transactor type. Unlike
    /// `TransactorMethod`, this is not a testbench-field call edge; it
    /// emits as a direct `<Transactor>_<method>(args...)` call inside the
    /// enclosing transactor method lambda.
    TransactorSelfMethod {
        transactor: String,
        method: String,
    },
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
    /// v1-compatible host-width `~` for a general expression whose newly
    /// admitted source tree contains a sized literal.
    BitNotHost,
}

/// Structured handle to a DUT signal. Codegen never re-parses port
/// names. Direction/width are `Option` because the MVP lowering does
/// not consult a DUT port table yet.
#[derive(Debug, Clone)]
pub struct PortRef {
    /// Test-scope field holding the DUT (`"dut"`).
    pub testbench_field: String,
    /// Dotted path below the DUT field (`["count_out"]`).
    pub port_path: Vec<String>,
    /// True when multi-segment paths are aggregate member accesses
    /// (`dut->exc_cause.irq_int`) rather than flattened bus-style
    /// signal names (`dut->axi_aw_valid`).
    pub aggregate_path: bool,
    pub direction: Option<PortDirection>,
    pub width: Option<u32>,
    pub access: PortAccess,
    /// Lane index for `dut.<port>[i]` accesses. Both constant and
    /// variable (runtime-evaluated) indices are in the lowered subset.
    /// Emission routes lanes of packed multi-lane ports (the `--sv`
    /// `vec_lane_widths` table) through
    /// `harc_rt::harc_vec_lane_{read,write}<W>`, and true unpacked-array
    /// ports through a raw C++ subscript — the same split as v1's
    /// `dut_packed_lane` (which also accepts an arbitrary index expr).
    pub lane: Option<LaneIndex>,
}

/// Lane index on a `dut.<port>[i]` access. v1 carries the raw `&Expr`
/// and re-renders it; the IR distinguishes a folded constant (kept as a
/// plain integer so the constant-lane fixtures are byte-identical to v1)
/// from a runtime expression that is lowered like any other value.
#[derive(Debug, Clone)]
pub enum LaneIndex {
    /// Constant-folded literal/`const`/enum index: `dut.<port>[2]`.
    Const(u64),
    /// Runtime-evaluated index expression: `dut.<port>[i]`.
    Var(Box<Expr>),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CovTrigger {
    PosedgeDutClk,
    /// Sample on a hookable transactor method's pre/post boundary instead
    /// of per primary-clock tick: `covergroup G @(drv.send(t) post)`.
    /// At lowering the target is UNRESOLVED — covergroup schemas lower
    /// before the testbench/transactor tables exist, so only the
    /// receiver field-access path and method name are stashed. The
    /// `covergroup_hooks` pass resolves `receiver_path` against the
    /// testbench's transactor fields after transactors are lowered and
    /// fills in `method` on the matching `TransactorMethodSchema`'s
    /// `cov_hook_subs`. Mirrors v1's late covergroup-hook registration.
    Hook {
        /// Field-access path of the call receiver, e.g. `["drv"]` for
        /// `drv.send(...)`. Resolved by the late pass.
        receiver_path: Vec<String>,
        /// The hookable method name (`send`).
        method: String,
        /// The hook trigger argument names (`["t"]` for `drv.send(t)`).
        /// The `covergroup_hooks` pass validates these against the
        /// resolved method's parameter names (v1's emit-time check) so a
        /// `cover <param>.<field>` target binds a real closure arg.
        param_names: Vec<String>,
        /// `pre` fires before the method body, `post` after.
        side: crate::ast::HookSide,
    },
}

#[derive(Debug, Clone)]
pub struct CoverPointSchema {
    pub name: String,
    /// Sampled expression. The shipped TBIR subset accepts pure DUT
    /// boundary expressions: direct ports, constant lanes/slices,
    /// literals, and scalar unary/binary/ternary expressions over those
    /// leaves. Test-scope locals and helper calls remain out of scope for
    /// auto-samplers.
    pub target: Expr,
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
///
/// BOTH forms carry a [`CovBinBound`]: either a folded compile-time
/// constant (`{3}`, `[1..3]`, `[LO..HI]` const names) or a genuine
/// runtime expression (`{dut.en}`, `[dut.en..7]`), matching v1, which
/// emits the raw expression per-sample rather than requiring a
/// constant. The exact-value form carried a bare `u64` until the runtime
/// case was implemented; ranges got runtime bounds first, and there was
/// never a reason for the two to differ — v1 renders both with the same
/// `emit_expr`.
#[derive(Debug, Clone)]
pub enum CovBinValue {
    Eq(CovBinBound),
    Range {
        lo: Option<CovBinBound>,
        hi: Option<CovBinBound>,
    },
}

/// A single covergroup bin range bound. Constant bounds fold at lowering
/// (`Const`); non-constant bounds carry a lowered [`Expr`] evaluated at
/// sample time (`Runtime`), mirroring v1's per-sample bound emission.
#[derive(Debug, Clone)]
pub enum CovBinBound {
    Const(u64),
    Runtime(Expr),
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
            Terminator::TbLifecycleCall { succ, .. } => vec![*succ],
            Terminator::Return | Terminator::Fatal(_) => vec![],
        }
    }
}
