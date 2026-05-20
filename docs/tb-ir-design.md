# Design: TB-IR — typed control-flow IR for HARC testbenches

Status: **Proposed (RFC, not yet implemented).**
Date logged: 2026-05-20.
Companion: [tb-ir-plan.md](tb-ir-plan.md) (delivery plan, motivation),
[constraint-system-plan.md](constraint-system-plan.md) (typed
constraint IR).

This document specifies the concrete shape of the TB-IR: types,
invariants, AST → IR lowering rules, verifier checks, and pass
interfaces. The plan doc covers *why* and *staged delivery*; this doc
is what you read to actually implement it.

## Scope

The TB-IR is a **typed, CFG-shaped intermediate representation between
HARC's AST and codegen.** It is internal to the compiler — no
user-visible syntax or semantics change. The IR is consumed by:

- `codegen/cpp_tb.rs` (current C++ backend, after the refactor)
- `codegen/cuda_tb.rs` (future GPU backend)
- `codegen/sv_stub.rs` (existing SV-probe bind emitter, lifted onto the
  IR)
- Internal analysis passes (`classify_sync`, `randomize_analysis`,
  `extract_port_set`, …) that produce reports or side-tables

The TB-IR does **not** own:

- Record schemas, constraint expressions, or solver lowering — those
  belong to the constraint IR ([constraint-system-plan.md](constraint-system-plan.md)),
  referenced through `ConstraintRef` handles.
- Coverage schema or bin counters — those belong to the covergroup
  elaboration phase, referenced through `CovgroupId` handles.
- Source-text preservation — formatting stays AST-based via `pretty.rs`.

## Layered structure

```
            ┌────────────────────┐
  source ──►│  parser → AST      │
            └─────────┬──────────┘
                      │
            ┌─────────▼──────────┐    ┌─────────────────────────┐
            │  ast → constraint  │───►│  constraint IR + solver │
            │  ast → tb-ir       │    │  (constraint-system-    │
            └─────────┬──────────┘    │   plan.md)              │
                      │               └────────────┬────────────┘
                      ▼                            │
            ┌─────────────────────┐                │
            │   TB-IR (verify)    │◄───────────────┘
            └─────┬───────┬───────┘     ConstraintRef
                  │       │
       ┌──────────┘       └──────────┐
       │                             │
   ┌───▼───┐                     ┌───▼───┐
   │passes │                     │codegen│
   └───────┘                     └───────┘
   classify_sync                 cpp_tb
   lower_coroutine               cuda_tb (future)
   randomize_analysis            sv_stub
   hoist_stimulus
   extract_port_set
```

The TB-IR is the only thing codegen consumes. Passes either:

- Produce annotations attached to the IR (`classify_sync` adds a
  `BlockClass` to every block), or
- Mutate the IR (`lower_coroutine` rewrites `TbFunction` into a tagged
  FSM shape), or
- Produce side-tables (`extract_port_set` returns a `PortSet`;
  `randomize_analysis` returns a `RandomizeBudget` map).

Every mutation must preserve `verify` well-formedness.

## Core types

All types live in `src/ir/mod.rs` (with `Display` impls in
`src/ir/display.rs`). Source spans elided in this doc for readability;
every type carries a `SourceSpan` field in the real implementation.

### Top-level

```rust
pub struct TbProgram {
    pub functions: IndexVec<FunctionId, TbFunction>,
    pub testbenches: IndexVec<TestbenchId, TestbenchSchema>,
    pub tests: IndexVec<TestId, TestSchema>,
    pub records: ConstraintIrRef,           // owned by constraint-system layer
    pub covgroups: IndexVec<CovgroupId, CovgroupSchema>,
    pub scoreboards: IndexVec<ScoreboardId, ScoreboardSchema>,
}

pub struct TbFunction {
    pub id: FunctionId,
    pub name: Symbol,
    pub kind: FunctionKind,
    pub params: Vec<TypedParam>,            // empty for run/check
    pub locals: IndexVec<LocalId, TypedLocal>,
    pub blocks: IndexVec<BlockId, BasicBlock>,
    pub entry: BlockId,
    pub owner: Option<TestbenchId>,         // bound impl run/check live on a TB
}

pub enum FunctionKind {
    Run,                                    // impl Test for Tb { run ... }
    Check,                                  // impl Test for Tb { check ... }
    TransactorBody { bus: BusId, method: MethodId },
    SamplerAuto { covgroup: CovgroupId },   // synthesized from auto_sample
    Helper,                                 // free function or static helper
}

pub struct TestbenchSchema {
    pub id: TestbenchId,
    pub name: Symbol,
    pub dut_field: Symbol,                  // typically "dut"
    pub dut_type: Symbol,                   // resolved DUT type name (string;
                                            // ARCH does port-typing, HARC does not)
    pub fields: Vec<TestbenchField>,        // dut, covergroups, scoreboards, ...
}
```

### Basic blocks

```rust
pub struct BasicBlock {
    pub stmts: Vec<Stmt>,                   // straight-line; no sync points
    pub terminator: Terminator,
}

pub enum Stmt {
    Assign(LocalId, Expr),
    DutWrite(PortRef, Expr),
    DutRead(LocalId, PortRef),
    Log(LogLevel, FmtArgs),
    AssertCheck { cond: Expr, on_fail: AssertFail },
    CoverSample(CovgroupId, Vec<Expr>),
    ScoreboardOp(ScoreboardId, ScoreboardOp),
}

pub enum AssertFail {
    IncrementErrors(MsgExpr),               // assert e, "msg"
    Fail(MsgExpr),                          // assert e else fail("msg") — same; bumps errors
                                            // (no separate Fatal because that's a Terminator)
}

pub enum Terminator {
    Jump(BlockId),
    Branch(Expr, BlockId /*then*/, BlockId /*else*/),
    WaitCycles(Expr, BlockId),
    WaitUntil(Expr, BlockId),
    WaitUntilTimeout {
        pred: Expr,
        cycles: Expr,
        on_fire: BlockId,
        on_timeout: BlockId,
    },
    Fork {
        arms: Vec<ForkArm>,
        join: BlockId,                      // join_all target
    },
    Randomize {
        target: LocalId,                    // tuple/transaction local
        constraints: ConstraintRef,         // handle into constraint-IR layer
        succ: BlockId,
    },
    Return,
    Fatal(MsgExpr),
}

pub struct ForkArm {
    pub kind: ForkArmKind,
    pub entry: BlockId,                     // arm body's first block
}

pub enum ForkArmKind {
    Inline,                                 // fork { stmts; } and ...
    BusMethodCall {
        bus_handle: LocalId,                // resolves to transactor instance
        method: MethodId,
        args: Vec<Expr>,
        result: Option<LocalId>,
    },
}
```

### Expressions

Expressions stay tree-shaped. No SSA, no flattening.

```rust
pub enum Expr {
    Literal(Lit, TypeRef),
    Local(LocalId),
    Field(Box<Expr>, FieldId),              // local-or-expr's field
    Index(Box<Expr>, Box<Expr>),            // vec[i]
    BinOp(BinOp, Box<Expr>, Box<Expr>),
    UnaryOp(UnaryOp, Box<Expr>),
    Cast { value: Box<Expr>, to: TypeRef, kind: CastKind },
    Call(CallTarget, Vec<Expr>),            // pure helper calls only — extern fns
}

pub enum CallTarget {
    Helper(Symbol),                         // user-defined helper function
    Builtin(BuiltinFn),                     // crc, log2, etc.
}
```

`Expr` deliberately does **not** include `DutRead`, `Randomize`, or
anything else with a side effect or sync semantics. Those are
`Stmt`/`Terminator`. This is the IR's load-bearing discipline.

### Lvalues and ports

```rust
pub enum LocalRef {
    Local(LocalId),
    LocalField(LocalId, FieldId),
    LocalIndex(LocalId, Expr),
}

pub struct PortRef {
    pub testbench_field: Symbol,            // "dut"
    pub port_path: Vec<Symbol>,             // ["axil_aw_valid"], or ["bus", "req"]
    pub direction: PortDirection,           // In/Out — set during lower from DUT schema
    pub width: u32,                         // resolved width
}
```

`PortRef` is a structured handle, not a string. The lowering pass
resolves dotted DUT access against the DUT's port list (Verilator
header for `--sv`, ARCH `.archi` for `--dut`) and produces a typed
`PortRef`. Codegen never re-parses port names.

## Invariants

`verify` enforces these and aborts before any backend emission if any
fail. The list is the spec.

**Function-level:**

1. `entry: BlockId` resolves to a block in `blocks`.
2. Every block in `blocks` is reachable from `entry` via terminator
   successors.
3. Every `LocalId` used in `Expr`, `Stmt`, or `Terminator` resolves to
   an entry in `locals`.
4. Every `LocalId` is *dominated* by an `Assign`, `DutRead`, or
   `Randomize`-target write on every path from `entry` (loose SSA: a
   local may be reassigned, but reads precede a definitionless local
   are a verify error). Loop-carried locals must be `Assign`'d before
   first read inside the loop.

**Block-level:**

5. Every block has exactly one `Terminator`.
6. Successor block IDs in every `Terminator` resolve to blocks in
   `blocks`.
7. No block contains a `Stmt` that performs a sync. (Today this means
   no IR-level construct outside `Terminator` may suspend — enforced
   by exhaustively typing `Stmt`.)
8. No block is empty unless its terminator is `Return` or `Jump`. (Empty
   blocks with branches are a codegen smell; the lowering pass merges
   them.)

**Cross-IR:**

9. Every `Terminator::Randomize { constraints: ConstraintRef, .. }`
   resolves to a valid `ConstraintRef` in the constraint IR.
10. Every `Stmt::CoverSample(cov, args)`: `args.len()` matches the
    covergroup's declared cover point count.
11. Every `Terminator::Fork::arms[i].kind = BusMethodCall { bus_handle,
    method, args, .. }`: `bus_handle` has type `bus<...>` and `method`
    is declared on that bus.
12. Every `PortRef` resolves against its `Testbench`'s DUT schema with
    matching width and direction.

**Type-level:**

13. `Expr::BinOp` operand types match (after explicit `Cast`).
14. `Assign(local, expr)`: `expr`'s type matches `local`'s declared
    type.
15. `Terminator::Branch(cond, _, _)`: `cond` is `bool`-typed.

Violations are programmer errors (lowering bugs or pass bugs), not user
errors. User errors are caught earlier by the type checker on the AST.

## AST → IR lowering rules

`src/ir/lower.rs` produces a `TbProgram` from a checked AST. The rules
below are mechanical; each AST construct has exactly one IR form.

### Statements within a `run` / `check` / transactor body

| AST construct | IR form |
|---|---|
| `dut.x = expr` | `Stmt::DutWrite(port_ref(dut.x), lower(expr))` |
| `let l = dut.x` | `Stmt::DutRead(l, port_ref(dut.x))` |
| `let l = expr` (no DUT read) | `Stmt::Assign(l, lower(expr))` |
| `let l = expr_containing_dut_read` | Hoist DUT read to a temp first: `DutRead(tmp, port); Assign(l, expr_with_tmp_substituted)`. |
| `l = expr` (reassign) | `Stmt::Assign(l, lower(expr))` |
| `dut.x.field = expr` | `Stmt::DutWrite(port_ref(dut.x.field), lower(expr))` |
| `assert cond [, "msg"]` | `Stmt::AssertCheck { cond, on_fail: IncrementErrors(msg.unwrap_or_default()) }` |
| `assert cond else fail("msg")` | same as above (fail is a synonym for IncrementErrors at this layer) |
| `log(level, "fmt", args)` | `Stmt::Log(level, fmt_args)` |
| `logf("file", level, "fmt", args)` | `Stmt::Log(level, fmt_args_with_file)` (LogLevel carries the optional sink) |
| `cov.sample(a, b)` | `Stmt::CoverSample(cov_id, [lower(a), lower(b)])` |
| `sb.write(...)` etc. | `Stmt::ScoreboardOp(sb_id, op)` |

### Control flow

| AST construct | IR form |
|---|---|
| `wait N cycles` | terminator `WaitCycles(lower(N), next_block)`; rest of body in `next_block`. |
| `wait until cond` | terminator `WaitUntil(lower(cond), next_block)`. |
| `wait until cond timeout N fail("msg")` | terminator `WaitUntilTimeout { pred, cycles, on_fire: next_block, on_timeout: synthesized_block_with_fail }`. |
| `randomize(t) with <body>` | (a) constraint-IR layer lowers `<body>` to a `ConstraintRef`. (b) terminator `Randomize { target: local_of(t), constraints, succ: next_block }`. |
| `if c then A elsif c2 then B else C` | recursive `Branch` chain. Each arm's body lives in its own block; arms reconverge by `Jump`-ing to a common `next_block`. |
| `for i in lo..hi <body>` | header block tests `i < hi`; body block ends with `Jump(header)`; exit block is `next_block`. Loop counter is a synthesized `LocalId`. |
| `while cond <body>` | same shape with the `cond` expression in the header block's `Branch`. |
| `fork A and B and C join_all` | terminator `Fork { arms: [arm(A), arm(B), arm(C)], join: next_block }`. Each arm's body becomes a fresh entry block that ends with `Jump(__arm_done)` where `__arm_done` is a synthetic block that records arm completion and `Jump`s back to a join-coordination block. |
| `return` (implicit at end of `run`) | terminator `Return`. |
| `fatal("msg")` | terminator `Fatal(msg)`. The block after a `Fatal` is unreachable; lowering does not emit one. |

### Synthesized blocks

Three synthesis cases create blocks not present in the source:

1. **`wait until ... timeout` fail handler.** A one-block body
   containing the `fail` action, terminating in `Return` (or in `Jump`
   to a user-specified label if the spec ever adds one).
2. **`auto_sample @(posedge clk)` covergroup samplers.** Each covergroup
   with `auto_sample` produces a `FunctionKind::SamplerAuto`
   `TbFunction` containing a single block: `[CoverSample(cov, ...)]`,
   `Return`. This function is called by the scheduler after every
   `WaitCycles`/`WaitUntil` resume — codegen wires the call in.
3. **Loop counter init.** `for i in lo..hi` synthesizes an initializer
   block: `Assign(i, lo); Jump(header)`. The user-written `lo` and `hi`
   are evaluated *outside* the loop (once each) and stashed in
   synthesized locals if they aren't already pure.

### Function-kind handling

| AST construct | IR form |
|---|---|
| `impl Test for Tb { run <body> }` | `TbFunction { kind: Run, owner: Some(Tb), .. }`, body lowers per the rules above. |
| `impl Test for Tb { check <body> }` | `TbFunction { kind: Check, owner: Some(Tb), .. }`. |
| `transactor T on bus B { method m(args) { body } }` | one `TbFunction` per method, `kind: TransactorBody { bus, method }`, `params` = method args. |
| free helper `fn foo(x: T) -> U { ... }` | `TbFunction { kind: Helper, owner: None, .. }`. |
| `extend test T { run <body> }` | merged at the AST layer (existing `codegen/merge.rs`) *before* IR lowering. IR sees the merged AST. |

## Verifier

`src/ir/verify.rs` exposes:

```rust
pub fn verify_program(prog: &TbProgram) -> Result<(), Vec<VerifyError>>;

pub fn verify_function(prog: &TbProgram, fn_id: FunctionId) -> Result<(), Vec<VerifyError>>;
```

Verifier runs:
- Automatically after every `lower` call in debug builds.
- Automatically after every IR mutation pass in debug builds.
- On-demand via `harc verify-ir Foo.harc`.

`VerifyError` is a typed enum with a source span and a structural
description. Sample errors:

- `UnreachableBlock { fn: FunctionId, block: BlockId }`
- `LocalUseBeforeDef { fn, block, stmt_idx, local }`
- `BadSuccessor { fn, block, terminator_kind, missing_succ }`
- `TypeMismatch { fn, span, expected, actual }`
- `DanglingConstraintRef { fn, terminator_block }`
- `CovgroupArityMismatch { sample_block, expected, actual }`

Every IR-mutating pass must leave the program verifiable. Tests for
each pass include a verify-after assertion.

## Pass interface contracts

Every pass lives in `src/ir/passes/<name>.rs` with the shape:

```rust
pub struct PassOutput { /* pass-specific result */ }

pub fn run(prog: &TbProgram) -> PassOutput;            // read-only pass
// or
pub fn run(prog: &mut TbProgram) -> PassOutput;        // mutating pass
```

The detailed signatures for the initial pass set:

### `classify_sync` (read-only)

```rust
pub enum BlockClass {
    DeviceOnly,        // no DutRead in stmts, terminator does not suspend
    DutObserving,      // contains DutRead but does not host-sync
    HostSyncing,       // log/fatal/randomize-call-that-needs-host
}

pub struct ClassificationTable {
    pub blocks: HashMap<(FunctionId, BlockId), BlockClass>,
}

pub fn run(prog: &TbProgram) -> ClassificationTable;
```

Walks each block's stmts and terminator once. O(IR size).

### `lower_coroutine` (mutating)

```rust
pub struct CoroutineMetadata {
    pub state_enum: HashMap<FunctionId, Vec<BlockId>>,    // state id ↔ block id
    pub transition_table: HashMap<FunctionId, Vec<Transition>>,
}

pub fn run(prog: &mut TbProgram) -> CoroutineMetadata;
```

Rewrites each `TbFunction` (kind Run/Check/TransactorBody/SamplerAuto)
into a normalized form where every terminator either:
- Jumps within the same "state" (block chain with no suspend), or
- Suspends and lists its resume successor by `BlockId`.

The `state_enum` lets `cuda_tb.rs` emit `switch (tb->state) { case N: ... }`
directly. `cpp_tb.rs` is free to ignore the metadata and continue
emitting coroutine `co_await`s.

### `randomize_analysis` (read-only)

```rust
pub enum RandomizeBudget {
    StaticallyBounded(u32),       // proven upper bound on calls per run invocation
    Unbounded,                    // loop with data-dependent count, etc.
}

pub struct RandomizeReport {
    pub per_function: HashMap<FunctionId, RandomizeBudget>,
}

pub fn run(prog: &TbProgram) -> RandomizeReport;
```

Walks the CFG, counts `Terminator::Randomize` reachable from each
`FunctionKind::Run` entry. Loops with literal bounds (`for i in 0..N`
where `N` is a const) count as bounded; loops with DUT-dependent or
parameter-dependent bounds are flagged as `Unbounded`.

The constraint runtime uses `StaticallyBounded(n)` to batch-precompute
`n` solver calls; `Unbounded` falls back to per-call solving. The GPU
backend uses the same data to decide whether a host sync is required
on randomize.

### `hoist_stimulus` (mutating)

```rust
pub struct StimulusBuffer {
    pub fn_id: FunctionId,
    pub locals_to_seed: Vec<LocalId>,      // locals filled from buffer
    pub element_count: usize,
}

pub fn run(prog: &mut TbProgram) -> Vec<StimulusBuffer>;
```

Identifies maximal connected subgraphs of `DeviceOnly` blocks that
write to locals later consumed by `DutWrite`s. Rewrites them to read
from a synthesized per-iteration stimulus buffer instead of computing
inline. The buffer is filled at TB-init time (CPU) or uploaded once
(GPU). Used by the GPU backend; opt-in for CPU.

### `extract_port_set` (read-only)

```rust
pub struct PortSet {
    pub read: HashSet<PortRef>,
    pub written: HashSet<PortRef>,
}

pub fn run(prog: &TbProgram) -> HashMap<TestbenchId, PortSet>;
```

Walks DutRead/DutWrite. Used by `sv_stub.rs` to generate sparse SV-bind
stubs only for the ports the TB actually probes; used by future
trace/wave declarations.

### `verify` (read-only)

```rust
pub fn run(prog: &TbProgram) -> Result<(), Vec<VerifyError>>;
```

Documented above. Runs after every mutation pass.

## Worked examples

### Example 1: simple `run` block (no randomize)

Source — `tests/fixtures/sync_fifo_test.harc` (paraphrased):

```harc
run
    dut.rst = 1
    dut.push_valid = 0
    wait 2 cycles
    dut.rst = 0
    assert dut.empty == 1, "empty after reset"
end run
```

IR after lowering:

```text
fn run_SyncFifoTest [kind=Run, owner=SyncFifoTb]
  locals:
    %0: bool      (DutRead temp for dut.empty)
  entry = b0

  b0:
    DutWrite(dut.rst,        1)
    DutWrite(dut.push_valid, 0)
    -> WaitCycles(Lit(2), b1)

  b1:
    DutWrite(dut.rst, 0)
    DutRead(%0, dut.empty)
    AssertCheck { cond: %0 == 1,
                  on_fail: IncrementErrors("empty after reset") }
    -> Jump(b2)

  b2:
    -> Return
```

Block classifications (`classify_sync`):
- `b0`: DeviceOnly (no DutRead, terminator WaitCycles is *device-allowed*)
- `b1`: DutObserving (DutRead present)
- `b2`: DeviceOnly (empty, Return)

### Example 2: randomize in a loop

Source — `tests/fixtures/axilite_constraint_test.harc` lines 38-56
(elided):

```harc
for i in 0 .. 4
    let p : RegPair
    randomize(p) with
        p.addr == 24
        p.value > 65536
        p.value < 2147483648
        (p.value & 3) == 0
    end randomize
    logf("axi_master.log", info, "constrained[${i}] addr=...")
    axil_write(dut, p.addr, p.value)
    let got = axil_read(dut, p.addr)
    assert got == p.value else fail("constrained[${i}] mismatch...")
end for
```

IR:

```text
locals:
  %i: uint<32>
  %p: RegPair
  %got: uint<32>
  %_lo, %_hi: uint<32>           (loop bounds — already literal)

b_init:
  Assign(%i,   Lit(0))
  -> Jump(b_header)

b_header:
  -> Branch(%i < Lit(4), b_body, b_exit)

b_body:
  -> Randomize { target: %p,
                 constraints: <ConstraintRef into constraint-IR>,
                 succ: b_after_rand }

b_after_rand:
  Log(info, "constrained[{}] addr=0x{:02x} value=0x{:08x}",
      [%i, %p.addr, %p.value])
  -> Jump(b_helper_call_write)              // axil_write is a helper TbFunction
                                            // call; full IR for helper inlining
                                            // omitted; see §"helper calls" below

b_after_write:
  -> Jump(b_helper_call_read)

b_after_read:
  AssertCheck { cond: %got == %p.value,
                on_fail: IncrementErrors("constrained[{}] mismatch...") }
  Assign(%i, %i + Lit(1))
  -> Jump(b_header)

b_exit:
  -> Return
```

`randomize_analysis` on this function returns `StaticallyBounded(4)` —
loop count is a literal, randomize fires once per iteration.

`classify_sync`:
- `b_init`, `b_header`, `b_after_rand`, `b_after_write`, `b_exit`: DeviceOnly
- `b_body`: HostSyncing (Randomize terminator)
- `b_after_read`: DutObserving (helper-inlined DutRead from axil_read)

### Example 3: fork over bus methods

Source (sketch):

```harc
fork
    bus.write(addr, data)
and
    bus.read(other_addr)
join_all
```

IR:

```text
b_pre:
  -> Fork {
       arms: [
         ForkArm { kind: BusMethodCall { bus, write, [addr, data], None },
                   entry: b_arm0 },
         ForkArm { kind: BusMethodCall { bus, read, [other_addr], Some(%r) },
                   entry: b_arm1 },
       ],
       join: b_post,
     }

b_arm0:
  -> Jump(b_arm0_done)
b_arm0_done:
  -> Jump(b_join_wait)                    // synthesized arm-complete record

b_arm1:
  -> Jump(b_arm1_done)
b_arm1_done:
  -> Jump(b_join_wait)

b_join_wait:
  -> WaitUntil(all_arms_complete, b_post) // synthesized join-coordination

b_post:
  ...
```

The "all_arms_complete" expression is a synthesized predicate the
scheduler resolves. `lower_coroutine` consumes the `Fork` directly;
the join-wait pattern is the C++ backend's representation only.

## Helper calls

A `Call(CallTarget::Helper(sym), args)` in an expression position
(e.g., `let got = axil_read(dut, addr)`) needs careful handling because
the helper itself contains `DutRead`/`DutWrite`/sync.

**Rule:** every helper call site is inlined into the caller's CFG at
lowering time. The helper's `TbFunction` remains in the IR (some
backends may emit it as a separate emitted function for debug
readability), but the *control flow* is inlined so the verifier and
analysis passes see one CFG per `run`.

Inlining substitutes:
- The helper's `params` for the call site's `args` (typed `LocalId`s).
- The helper's return value into the caller's destination `LocalId`.

This is the same shape `cpp_tb.rs` already implements via
`field_subs` for transactor bodies — the lowering pass moves it from
codegen into the IR layer.

Helpers without DUT access or sync points (pure expression helpers) do
*not* need inlining; they can stay as `Expr::Call` and lower to a C++
function call directly. The lowering pass categorizes helpers up front.

## Display and dumping

`src/ir/display.rs` produces a textual form roughly matching the
worked examples above. The form is:

- Stable enough to diff in tests (whitespace and ordering deterministic).
- Round-trippable enough for golden-file fixtures, but not necessarily
  reparseable into IR (the IR has no surface syntax).
- Readable enough for human debugging.

CLI: `harc dump-ir Foo.harc` prints the IR after lowering and verify.
`harc dump-ir --pass classify_sync Foo.harc` prints the IR plus the
classification annotations. Used in CI and during local development.

## Open questions

The list narrows from `tb-ir-plan.md` to the things that need a
decision before phase 1 lands:

1. **`Expr::Cast::kind` enum.** Today `cpp_tb.rs` handles signed/unsigned
   conversions, width truncation, and enum-to-bits via context. The IR
   needs them explicit. Proposed: `CastKind = { ZeroExtend, SignExtend,
   Truncate, Reinterpret, EnumToBits, BitsToEnum }`. Punted to phase 1
   implementation but worth front-loading.

2. **`PortRef::direction` for inout ports.** ARCH allows `inout` (rare,
   typically tri-state pad models). HARC needs a representation; today
   it goes through Verilator's `__io` accessors. Proposed: a third
   `PortDirection::Inout` variant carrying both setter and getter
   pathways. Verifier disallows `DutRead` on `Out`-only and `DutWrite`
   on `In`-only.

3. **Scoreboard op vocabulary (`Stmt::ScoreboardOp`).** Blocked on
   spec §7.7 finalization. The IR placeholder is a `(ScoreboardId,
   ScoreboardOp)` pair; `ScoreboardOp` becomes a tagged enum once
   semantics are pinned. Phase 1 can ship with `ScoreboardOp = Stub`
   and refine later — scoreboards are post-MVP.

4. **`Terminator::Switch` vs chained `Branch`.** Switch would simplify
   FSM lowering's `case` emission. Multi-way `Switch` is sugar for a
   `Branch` chain at the IR level, so phase 1 can ship with `Branch`
   only. Add `Switch` if `lower_coroutine`'s pattern matching becomes
   unwieldy.

5. **Inlining transactor `hookable` bodies.** Today `cpp_tb.rs` uses
   `field_subs` to substitute hookable method bodies inline at the
   callsite of `transactor.method()`. The IR equivalent is to keep the
   `TbFunction` separate and emit a function call — but the scheduler
   integration may need inlining to preserve fork semantics. Decision
   pending a small experiment on `axi_agent.harc`.

## Decision log

- 2026-05-20: After the slicing discussion, agreed to do TB-IR as a
  pass-first incremental rollout: build IR types + AST→IR lowering for
  everything, write passes against the IR before writing IR→C++. The
  constraint-IR (per constraint-system-plan.md) lands first as
  independent work. "TB-IR only for randomize" was explored and
  rejected because `Terminator::Randomize` splits a block and forces
  its successors to also be IR, so a half-IR'd function is incoherent.
