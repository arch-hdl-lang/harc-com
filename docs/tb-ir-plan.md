# Plan: TB-IR — typed intermediate representation between AST and codegen

Status: **In delivery.** MVP subset shipped (PRs #347–#351, 2026-06):
IR types + lowering + verifier + a parallel `--codegen tbir` C++
backend gated on behavioral trace-equivalence. See
[tbir-mvp.md](tbir-mvp.md) for the shipped scope and documented
divergences. The phase-4 gate was redefined on 2026-06-12 (see the
decision log): behavioral trace-equivalence + full construct coverage,
not byte parity. Phases 4–6 (close coverage, flip default, delete v1)
and the passes (phase 7) remain open — statuses annotated in the table
below. Date logged: 2026-05-20; gate amended 2026-06-12.
Scope: Insert a typed, CFG-shaped testbench IR between HARC's AST and
the C++ codegen so that multiple backends (the current C++ TB, a future
CUDA backend for GPU sim, and the existing SV-bind stub emitter) consume
the same compiled form. Today the path is **AST → C++**, all in one
15k-LOC monolith (`src/codegen/cpp_tb.rs`); a second backend would either
clone that monolith or thread flags through it. Both options compound
technical debt with every feature added afterward.

## Motivation

The codegen is at the point where the next major work item — a
device-side TB emitter for GPU co-sim with arch-com — cannot land
cleanly without forcing a choice between two bad options:

1. **Clone `cpp_tb.rs`** into a near-identical `cuda_tb.rs` and accept
   permanent divergence-debt. Every harc feature added after the clone
   has to be implemented twice with no shared invariant. This is
   exactly how 15k-LOC monoliths become 30k-LOC monoliths.

2. **Thread `--target cuda` flags through `cpp_tb.rs`** and special-case
   the differences. Every C++-vs-CUDA codepath split inside a function
   adds a conditional, and the existing flag-branched logic for
   `--sv`/`--dut`/`--mt` already makes the file hard to reason about
   end-to-end.

The right move is the one [constraint-system-plan.md](constraint-system-plan.md)
already articulates for the solver path: put a typed elaboration layer
between HARC syntax and any backend. The constraint system plan covers
the *data* side (record schemas + constraint IR for the solver
boundary); this plan covers the *control* side (a CFG IR for the
codegen boundary). The two layers are independent but compose: the
constraint IR ends up referenced from a TB-IR `Randomize` terminator.

Independent benefits the IR unlocks, beyond enabling a second backend:

- **Static sync-point classification.** Today, "does this block of TB
  code observe the DUT?" is a property a human reader infers from
  reading C++ output. With the IR it becomes a one-pass annotation.
- **Pure-stimulus hoisting.** Stimulus that doesn't depend on a DUT
  read can lift out of the per-cycle path. Currently invisible.
- **Randomize call-count analysis.** Required for the GPU backend (to
  decide whether Z3 calls can be batch-precomputed on the host vs.
  forcing a per-batch host sync), but also informs whether the
  constraint solver path needs the back-pressure logic from the
  constraint-system plan.
- **Verifier.** Generated C++ bugs surface in `g++` errors or runtime
  scheduler crashes. IR-level well-formedness checks (every block
  reachable, terminator successors typed, etc.) catch a class of
  codegen bugs at the IR boundary instead.

## Non-goals

- **No expression-level SSA.** TB expressions are short. SSA adds
  machinery without buying anything for the workloads HARC compiles.
- **No IR-level optimizer in v1.** `lower → verify → emit` only.
  Dead-write elimination, CSE, and similar passes wait until a measured
  need exists.
- **`pretty.rs` (1734 LOC) stays AST-based.** `harc fmt` is about
  source preservation, not IR transformation.
- **`constraints.rs` stays mostly as-is.** It's already factored out of
  `cpp_tb.rs`; it just gets called from a `Terminator::Randomize`
  handler instead of inline.
- **No change to user-visible syntax or semantics.** The IR is an
  internal refactor. `harc check`/`harc sim`/`harc fmt` behavior is
  unchanged.

## Architecture

### Core IR types

```rust
// src/ir/mod.rs

pub struct TbProgram {
    pub functions: IndexVec<FunctionId, TbFunction>,
    pub records:   IndexVec<RecordId,   RecordSchema>,    // from constraint IR
    pub covgroups: IndexVec<CovgroupId, CovgroupSchema>,
}

pub struct TbFunction {
    pub name:    Symbol,
    pub kind:    FunctionKind,            // Run, Check, TransactorBody, ...
    pub params:  Vec<TypedParam>,
    pub locals:  IndexVec<LocalId, TypedLocal>,
    pub blocks:  IndexVec<BlockId, BasicBlock>,
    pub entry:   BlockId,
    pub source:  SourceSpan,
}

pub struct BasicBlock {
    pub stmts:      Vec<Stmt>,           // straight-line, no sync points
    pub terminator: Terminator,
    pub source:     SourceSpan,
}

pub enum Stmt {
    Assign(LocalId, Expr),
    DutWrite(PortRef, Expr),
    DutRead (LocalId, PortRef),          // explicit, so sync analysis is trivial
    Log     (LogLevel, FmtArgs),
    AssertCheck(Expr, MsgExpr),
    CoverSample(CovgroupId, Vec<Expr>),
    ScoreboardOp(ScoreboardId, ScoreboardOp),
}

pub enum Terminator {
    Jump(BlockId),
    Branch(Expr, BlockId, BlockId),
    WaitCycles(Expr, BlockId),
    WaitUntil (Expr, BlockId),
    WaitUntilTimeout {
        pred: Expr, cycles: Expr,
        on_fire: BlockId, on_timeout: BlockId,
    },
    Fork(Vec<ForkArm>, BlockId /* join_all target */),
    Randomize {
        target: LocalId,
        constraints: ConstraintRef,       // owned by constraints.rs
        succ: BlockId,
    },
    Return,
    Fatal(MsgExpr),
}
```

### Defining property

Every sync point is a **block terminator**, not a statement inside a
block. That single discipline is what makes both lowering paths
(C++ coroutines, device-side FSM) mechanical:

- The C++ backend lowers each terminator to a `co_await ...` call into
  the existing `harc_rt` scheduler.
- The CUDA backend lowers each terminator to a state transition in a
  tagged-FSM-per-run scheme; per-block code is a `case` arm.

If sync points were allowed inside `stmts`, each codegen would need its
own statement-splitter pass. Pushing them to the terminator slot avoids
that entirely.

### Intentional design choices

- **`DutRead` is its own `Stmt`, not an expression.** Most important
  shape decision in the IR. It makes "this block depends on a DUT
  observation" a *syntactic* property — `placement` is one walk over
  `stmts`. Treating DUT reads as expressions would force every codegen
  and every pass to bottom-out into expression traversal to find them.

- **`Randomize` is a terminator, not a `Stmt`.** It's a potential
  host-sync point on any placement-split backend; placing it in the
  terminator slot forces every backend to make an explicit decision
  about it instead of having it slip through inside a block.

- **Locals interned in `IndexVec<LocalId, _>`.** Lets transformation
  passes rewrite locals without touching every `Expr`. Names live in the
  span/debug-info path, not in the structural IR.

- **Expressions stay tree-shaped, not flattened.** TB expressions are
  short by construction (a few field accesses, a comparison, an
  arithmetic op). Flattening to triples would inflate the IR without
  enabling any pass we want to write.

- **`ConstraintRef` is a handle into the constraint IR**, not a copy.
  The constraint-system plan owns that data; TB-IR just references it.

### Module layout

```
src/
  ir/
    mod.rs                       # IR type definitions + Display
    lower.rs                     # AST → IR
    verify.rs                    # well-formedness checks
    passes/
      placement.rs               # placement tier + timing class per block,
                                 # capability-checked against a TargetProfile
      lower_coroutine.rs         # CFG → tagged FSM (split backends' input)
      randomize_analysis.rs      # statically-bounded vs unbounded randomize counts
      hoist_stimulus.rs          # pure-stimulus subgraphs → upload buffers
      extract_port_set.rs        # DUT ports the TB reads/writes (sparse trace)
  codegen/
    cpp_tb.rs                    # IR → C++ (existing monolith, shrinks)
    sv_stub.rs                   # IR → SV bind stubs (existing, refactor to IR)
    merge.rs                     # unchanged
    (placement-split backends consume the same IR; spec §10 roadmap)
```

Each pass is independently testable and ~100-300 lines. Today every
one of these is either impossible or a flag branch inside
`cpp_tb.rs`.

## Worked example: AST → IR → C++

Take `tests/fixtures/sync_fifo_test.harc` lines 24-40 (paraphrased):

```harc
run
    dut.rst = 1
    dut.push_valid = 0
    wait 2 cycles
    dut.rst = 0
    assert dut.empty == 1, "empty after reset"
end run
```

After AST → IR lowering, the `run_SyncFifoTest` `TbFunction` has three
basic blocks:

```text
fn run_SyncFifoTest() {
  entry = b0

  b0:
    DutWrite(dut.rst,        1)
    DutWrite(dut.push_valid, 0)
    -> WaitCycles(2, b1)

  b1:
    DutWrite(dut.rst,        0)
    DutRead (l0,  dut.empty)
    AssertCheck(l0 == 1, "empty after reset")
    -> Jump(b2)

  b2:
    -> Return
}
```

The C++ backend lowers this to the same coroutine output the v1
emitter produces today:

```cpp
HarcThread run_SyncFifoTest() {
    harc_assign(dut->rst,        1);
    harc_assign(dut->push_valid, 0);
    co_await wait_cycles(_slot, 2);
    harc_assign(dut->rst, 0);
    if (!(harc_read(dut->empty) == 1)) {
        sim_log_line(LOG_ERROR, "empty after reset");
        errors++;
    }
    co_return;
}
```

The CUDA backend (future) lowers it to a tagged FSM:

```cpp
enum class State_SyncFifoTest { B0_Enter, B0_Wait, B1, Done };

__device__ void step_SyncFifoTest(TbState* tb, DutState* dut, uint32_t cycle) {
    switch (tb->state) {
      case B0_Enter:
        dut->rst        = 1;
        dut->push_valid = 0;
        tb->wait_until_cycle = cycle + 2;
        tb->state = B0_Wait;
        return;
      case B0_Wait:
        if (cycle < tb->wait_until_cycle) return;
        tb->state = B1;
        // fallthrough
      case B1:
        dut->rst = 0;
        if (!(dut->empty == 1)) {
            tb->errors++;
            tb->log_pending = LogTag::EmptyAfterReset;  // host drains
        }
        tb->state = Done;
        return;
      case Done:
        return;
    }
}
```

Same IR, two backends, no shared codepath inside the codegen — the
discipline that makes the second backend cheap.

## Passes (what the IR unlocks)

| Pass | Purpose | Consumed by |
|---|---|---|
| `placement` | Tag each block with a placement tier (co-located / near-processor / host-service) + timing class (cycle-exact / timing-tolerant), capability-checked against a `TargetProfile`. One walk over stmts + terminators. | placement-split backends; `harc dump-ir --pass placement` diagnostics |
| `lower_coroutine` | CFG → tagged FSM. Adds explicit state IDs to each block, computes transition table. | split backends directly; `cpp_tb` continues to use the CFG as coroutine source. |
| `randomize_analysis` | For each `Terminator::Randomize`, prove static upper bound on call count per `run`. If unbounded, mark for host-sync. | split backends' solve-strategy selection (replay table vs host sync); reports to user. |
| `hoist_stimulus` | Identify pure-stimulus connected subgraphs; group as upload buffers. | split backends' stimulus pre-staging. |
| `extract_port_set` | DUT ports actually read/written. | sparse trace declarations; SV-probe `bind` stub generation. |
| `verify` | Well-formedness: entry reachable, terminator successors valid, locals SSA-ish in scope, etc. | Runs on every IR mutation in debug builds. |

Each pass is ~100-300 LOC, single responsibility, independently tested
against fixtures.

## Migration / staged delivery

Staged rollout, equivalence-gated. The load-bearing safety net is
**behavioral trace-equivalence**: for every fixture, v1 and tbir at the
same seed must both reach the same verdict (`ALL TESTS PASSED`, or the
same expected failure for expect-fail rows) and their normalized JSONL
traces must compare clean under `harc trace-diff`
(`tests/run_tbir_equiv.sh`, enforced in CI). This replaces the original
byte-identical-`.cpp` gate — see the 2026-06-12 decision-log entry for
the rationale.

| Phase | Status | Scope |
|---|---|---|
| 1 — IR types module | **done for the MVP subset** (PR #347) | `src/ir/mod.rs` + `Display` impl + unit tests on hand-built IR snippets. Reviewable on its own; no production code uses it yet. |
| 2 — AST → IR lowering | **done for the MVP subset** (PRs #347, #349–#351; `src/ir/lower/`, `harc dump-ir` shipped) | `src/ir/lower.rs`. New `harc dump-ir Foo.harc` CLI for manual inspection. `cpp_tb.rs` still emits from AST. |
| 3 — IR verifier | **done for the MVP subset** (PR #347; runs unconditionally before every tbir emission, not only debug builds) | `src/ir/verify.rs`. Runs after every `lower` in debug builds. |
| 4 — IR → C++ (`--codegen tbir`) | **in delivery.** The loop-switch backend shipped (PRs #347–#351) and IS this phase's deliverable — gate redefined 2026-06-12 (decision log). Behavioral trace-equivalence enforced in CI over the registry (`tests/tbir_equiv_fixtures.txt`); coverage growing. | Phase-complete when: (a) every fixture in the full corpus is in the equivalence registry — including expect-fail rows so fatal/assert paths are compared, not just passing runs; (b) the lowering's `Unsupported` list is empty for v1's feature set (transactions/randomize, transactors, fork, scoreboards, agents/events, range/cross bins, `any of`, `--mt`/split); (c) `run_tbir_equiv.sh` green in CI across all of it. **No byte-parity requirement** — the trace-equivalence harness is the safety net. |
| 5 — Flip default to tbir | not started; unblocked by phase 4 | After one release cycle of clean equivalence CI at full coverage. Keep v1 reachable via `--codegen v1` as escape hatch for one release. |
| 6 — Delete v1 emission | not started | After one release cycle of tbir-as-default with no escapes. Deletes `cpp_tb.rs` emission paths; shared scaffolding consolidated per issue [#355](https://github.com/arch-hdl-lang/harc-com/issues/355). |
| 7 — Passes land | **started** — `lower_coroutine` shipped (`src/ir/passes/lower_coroutine.rs`, `harc dump-ir --pass lower-coroutine`; read-only side-table, see design-doc note) | `placement`, `randomize_analysis`, `extract_port_set` remain. These are net-new functionality and don't need parity gates; gate on their own unit tests. |
| 8 — Placement-split backends consuming IR | unblocked after 5 | The spec §10 roadmap execution targets. Out of scope for this plan; each backend carries its own plan + `TargetProfile`. |

Phase 4 is the long pole — now paced by construct coverage, not parity
debugging. The original plan prescribed a byte-identical `.cpp` diff
and warned that a behavioral stdout diff "hides shape differences".
Both halves of that reasoning were superseded by what actually shipped:
the tbir backend's loop-switch shape makes byte parity with v1
impossible by construction (forcing it would mean writing a relooper
first, pure overhead for a shape we intend to delete), and the
equivalence harness is not a stdout diff — it compares normalized
semantic JSONL traces per fixture per seed, including expect-fail rows
that pin down fatal/assert behavior. Byte parity was always a *means*
(catch silent behavior change during migration); the trace harness now
provides that end directly, against both DUT backends.

## What's hard

Same shape as the separate-compilation refactor: `cpp_tb.rs` is built
around `[&]`-capturing lambdas and shared mutable scope across the
emission of one test. The fields the lambdas capture (`_checkers`,
`tick`, `dut`, `_run_slot`, etc.) are exactly the things the IR has to
make explicit.

Three concrete pain points worth flagging:

- **Transactor bodies and hookable methods.** These currently emit as
  lambdas whose substitution rules live inside `field_subs`. The IR
  needs to represent them as named `TbFunction`s with explicit
  parameters; the substitution logic moves to AST → IR lowering and
  never reaches codegen.

- **Forks across TLM bus methods.** `fork`/`join_all` on `bus.method()`
  calls currently relies on the scheduler's pending-fork list. In the
  IR, `Terminator::Fork(Vec<ForkArm>, join_block)` makes the join
  target a first-class successor — that mostly lines up, but the
  ForkArm representation needs to handle bus-method dispatch (an
  indirect call through a transactor handle) cleanly.

- **`covergroup.sample()` site lowering.** Today `sample()` is a method
  on the C++ covergroup struct, called inline. As a `Stmt::CoverSample`
  it needs to carry the bound expression list — straightforward — but
  the *implicit* posedge-driven samplers (`auto_sample @(posedge clk)`
  from spec §6) need to be lifted into a synthesized basic block that
  every `run` function jumps through after each `WaitCycles`/`WaitUntil`
  resume. That synthesis is new logic.

## Migration cost estimate

~2-4 months of focused work for one engineer, paced by:

- IR types + AST → IR lowering: 3-4 weeks.
- IR → C++ to full construct coverage with equivalence CI over the
  whole fixture corpus: 6-8 weeks. The long pole. Each remaining
  construct (transactions/randomize, transactors, fork, scoreboards,
  agents/events, range/cross bins, `--mt`/split) is a lowering slice +
  emission slice + registry rows.
- v1 deletion: 1-2 weeks after a clean release cycle.
- Passes (`placement`, `randomize_analysis`, `extract_port_set`):
  ~1 week each, can land in parallel with the coverage work since they
  consume the IR and don't perturb the C++ output.

Out of scope here but follows directly: placement-split backends land
after phase 5; each is estimated at 6-10 weeks given the IR exists.

## Open questions

1. **Does `Stmt::Assign` need width-typed sub-variants** (UAssign,
   SAssign, BitsAssign) or is the type on `TypedLocal` enough? The
   constraint IR already separates signed/unsigned at the field level;
   probably enough to defer to that.

2. **Do we want `Terminator::Branch(Expr, _, _)` to be the only
   conditional, or also a multi-way `Switch`?** Switch would simplify
   FSM lowering. Multi-way is just sugar for a chain of branches at the
   IR level, so v1 can ship with `Branch` only and add `Switch` later
   if a pass wants it.

3. **How should the IR represent `extend test T`?** Today merge happens
   in `codegen/merge.rs` at AST level before emission. Probably stays
   AST-level — merging IR functions is harder than merging AST nodes
   and provides no benefit.

4. **Where do scoreboards live?** `Stmt::ScoreboardOp` is a placeholder
   in this doc; the actual op vocabulary needs to be pinned down once
   the v1 scoreboard semantics from spec §7.7 stabilize. The IR plan
   should not block scoreboard work, but the two need to converge
   before phase 4 ships.

## Decision log

- 2026-05-20: After scoping a GPU co-sim backend with arch-com, agreed
  to refactor `cpp_tb.rs` onto an IR *first*, rather than land the GPU
  backend as a `cpp_tb.rs` clone and accept divergence-debt.
  Constraint-system plan and separate-compilation plan both already
  pull in the same direction (typed IR between syntax and backend);
  this plan covers the control-flow side.
- 2026-06-12: **Phase-4 gate redefined** (maintainer decision):
  behavioral equivalence + full construct coverage, dropping the
  byte-identical-`.cpp` requirement. The MVP (PRs #347–#353) shipped a
  parallel loop-switch backend for which byte parity with v1 is
  impossible by construction; the alternative — migrating v1's exact
  emission shape onto the IR — would reproduce, on top of the IR, the
  re-structured control flow we intend to delete in phase 6. The
  byte-parity gate's purpose (catch silent behavior change) is served
  directly by the normalized trace-equivalence harness
  (`harc trace-diff` over `tests/run_tbir_equiv.sh`, in CI), extended
  with expect-fail rows so fatal/assert paths are compared too.
  Consequence: the shipped tbir backend is no longer a "parallel
  detour" — it *is* phase 4, and phases 5–6 (flip default, delete v1)
  gate on its coverage reaching v1's full feature set. The structural
  dedup items in issue
  [#355](https://github.com/arch-hdl-lang/harc-com/issues/355)
  (scaffolding raw-string duplication, loop-switch emitter dup) become
  live phase-4/6 work rather than optional cleanup.
