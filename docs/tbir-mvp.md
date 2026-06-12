# TB-IR MVP — shipped status and documented divergences

Status: **Shipped (MVP subset), merged on main.**
Date logged: 2026-06-11.
Companion: [tb-ir-design.md](tb-ir-design.md) (the IR contract this doc
diverges from), [tb-ir-plan.md](tb-ir-plan.md) (delivery plan; phase
table annotated with what the MVP discharged).

This is the honest record of what the TB-IR MVP actually is: which
parts of the design doc are implemented as specified, where the
implementation deliberately deviates and why, and which gates from the
plan doc are discharged versus still owed. Every claim below was
checked against the code at the cited location.

## What shipped

| PR | Content |
|---|---|
| #347 | MVP spine: IR types (`src/ir/mod.rs`), textual form (`src/ir/display.rs`), structural verifier (`src/ir/verify.rs`), AST → IR lowering (`src/ir/lower/`), loop-switch C++ backend (`src/codegen/tbir/`) behind `harc sim --codegen tbir`, and the `harc dump-ir` CLI. |
| #348 | v1-vs-tbir equivalence harness (`tests/run_tbir_equiv.sh` + `tests/tbir_equiv_fixtures.txt`), CI step, negative tests for out-of-subset constructs. |
| #349 | Covergroup lowering + emission (`CovgroupSchema`, `SamplerAuto` functions, `CovReport`/`CovBin`, auto-cross matrices). |
| #350 | Helper handling: pure helpers stay `Expr::Call` and emit as file-scope C++ functions; impure helpers (DUT access or sync) CFG-inline at every call site; recursion rejected with the cycle path. |
| #351 | `wait until` lowering + emission: `Single`/`AllOf` modes, optional `timeout N cycles [fail("...")]` with the v1 diagnostic block shape, plus the `wait_until_counter_test` fixture. |

### Construct subset

From `src/ir/lower/mod.rs`: classic-form and impl-for testbench-bound
tests with a single DUT, declared clocks (time-literal or `domain`
periods), and the core statement set — DUT port read/write,
`let`/assign, `log`/`logf`, inline `assert ... else fail`, `fail`,
`wait N cycles`, `wait until` (single and `all of`, optional timeout),
`if`/`for`/`while`/`repeat`/`loop`/`break`/`continue`, covergroups
with set-literal bins, and helper functions. Everything else —
`transaction`, `randomize`, `agent`/`event`, transactors, buses,
scoreboards, range bins, declared `cross`, `any of`, `fork`, ... —
is rejected at lowering time with `LowerError::Unsupported` naming the
construct and pointing at `--codegen v1`. Lowering never silently
mis-lowers; that property is load-bearing and tested
(`tests/tbir.rs`).

### The 5-fixture equivalence matrix

`tests/tbir_equiv_fixtures.txt` (append-only registry consumed by
`tests/run_tbir_equiv.sh`, wired into CI):

| Fixture | Top | DUT | Exercises |
|---|---|---|---|
| `top_counter_test` | `Top` | `top_counter.sv` | resets, loops, asserts, format args |
| `sync_fifo_test` | `TxQueue` | `sync_fifo.sv` | covergroups, auto-cross, check phase |
| `bus_arbiter_test` | `BusArbiter` | `bus_arbiter.sv` | multi-point covergroup, CovBin reads |
| `wait_until_counter_test` | `Top` | `top_counter.sv` | wait-until single/all-of + timeout diags |
| `rom_lut_test` | `RomLut` | `rom_lut.sv` | impure-helper CFG inlining ×8, coverage |

All five also have full-file insta snapshots locking both the dump-ir
text and the emitted tbir C++ (`tests/tbir.rs`,
`tests/snapshots/tbir__*`), so emitter refactors diff visibly.

## The behavioral gate vs the plan's byte-identical gate

[tb-ir-plan.md](tb-ir-plan.md) phase 4 prescribes a **byte-identical**
`.cpp` diff against the v1 emitter as the migration safety net, and is
explicit that a behavioral diff "hides shape differences". The MVP's
gate is behavioral instead: both codegens must print `ALL TESTS
PASSED` and their semantic JSONL traces must compare clean under
`harc trace-diff` (normalized — backend-implementation noise like
`seq` numbering is ignored; see `check_backends::diff_trace_strings`).

This is not a quiet weakening of the plan's gate; it is a different
deliverable. The plan's phase 4 migrates *v1's own emission* onto the
IR, where byte parity is meaningful and achievable. The MVP instead
added a **second, parallel backend** (`src/codegen/tbir/`) whose
function bodies are a loop-switch over `BlockId` (`while (!__done)
switch (__bb) { case N: ... }`) rather than v1's re-structured control
flow — byte parity with v1 is impossible by construction for that
shape, and forcing it would have meant writing a relooper before
writing anything else. The scaffolding (preamble, `HarcTestContext`,
clock scheduler, log/trace plumbing, dispatcher `main()`) does mirror
the v1 contract closely (see `src/codegen/tbir/mod.rs` and
`runtime.rs`), which is what makes the traces line up cycle-exactly.

> **Superseded 2026-06-12.** The plan's phase-4 gate was redefined to
> behavioral equivalence + full construct coverage (see the
> [tb-ir-plan.md](tb-ir-plan.md) decision log). The byte-parity
> requirement is dropped; this section is kept as the record of why the
> MVP's gate differed from the plan as originally written. Under the
> redefined gate, the tbir backend *is* the phase-4 deliverable, and
> what it owes is coverage (empty `Unsupported` list for v1's feature
> set, full fixture corpus in the equivalence registry), not parity.

## Documented divergences from tb-ir-design.md

Each item names the design-doc shape, the implemented shape, and the
reason. Code locations are authoritative.

1. **`PortRef.direction` / `PortRef.width` are `Option` and currently
   always `None`.** The design says lowering resolves dotted DUT
   access against the DUT's port list (Verilator header on `--sv`,
   ARCH `.archi` on `--dut`) and produces a fully typed `PortRef`. The
   MVP lowering does not consult a DUT port table at all
   (`src/ir/mod.rs`, `PortRef` doc comment). Consequence: design
   invariant 12 (width/direction match, Probe-read-only,
   Force-write-only) is unimplementable today and the verifier does
   not attempt it. `PortAccess` is always `Port` (probes/forces are
   out of subset).

2. **`Expr::Port` exists, with relaxed-but-checked positions.** The
   design's `Expr` deliberately has no DUT-read variant — `DutRead` is
   a `Stmt` and that is called the IR's load-bearing discipline. The
   MVP keeps `Stmt::DutRead` hoisting as the default rule but adds
   `Expr::Port`, permitted in exactly five positions (the
   port-position rule in `src/ir/verify.rs`): wait predicates (the
   scheduler must re-sample the DUT every cycle inside the predicate
   closure — a hoisted temp would freeze the value), format-arg
   expressions and `FailDiag` guards (v1 evaluates failure messages
   lazily at the failure site, after the wait has timed out),
   `DutWrite` values, and `AssertCheck` condition subtrees (v1 parity:
   the assert samples at check time). Everywhere else — `Assign`
   values, `Branch`/`WaitCycles` operands — an inline port read is a
   verify error and lowering hoists through a `DutRead` temp
   (`dut_read_in_let_hoists_to_dut_read_stmt` in `tests/tbir.rs`).

3. **No passes.** `src/ir/passes/` does not exist. `placement`,
   `lower_coroutine`, `randomize_analysis`, `hoist_stimulus`, and
   `extract_port_set` are all unimplemented, as is `TargetProfile`.
   The tbir backend consumes the raw CFG directly; the loop-switch
   shape is precisely what lets it skip `lower_coroutine` (no
   relooping needed — `co_await` inside a `switch` is legal C++20).

4. **All locals hoist as `uint64_t`.** `src/codegen/tbir/func.rs`
   declares every IR local as `uint64_t <name> = 0;` at function top
   (hoisting is forced by the loop-switch — a local must survive
   across `case` arms). v1 emits declared-width C types for typed
   lets (`local_value_c_type`: `uint<75>` → `_harc_u128`, narrow
   widths → narrow C types) and `int64_t` for untyped integer lets.
   Two observable deltas follow: (a) a typed narrow local that
   overflows its declared width truncates on assignment under v1 but
   not under tbir — **narrowing semantics differ**; (b) untyped lets
   holding negative intermediates are signed under v1, unsigned under
   tbir. None of the five fixtures exercises either case, so the
   behavioral gate cannot see this; it is a known, latent divergence.
   Fix path: `IrType` already carries `UInt(Option<u32>)` /
   `SInt(Option<u32>)`; lowering currently leaves locals
   `IrType::Unknown`. Populating widths at lowering (from `let`
   type annotations, as v1's `let_widths` pass does) and emitting
   width-faithful types — or masking at `Assign` — closes it without
   IR shape changes.

5. **Invariant 8 amended in the verifier.** The design's literal text:
   "No block is empty unless its terminator is `Return` or `Jump`."
   That text contradicts the design doc's own worked example 2, where
   `b_header` is an empty block terminated by `Branch` — every loop
   header the lowering rules produce has that shape, and a loop body
   whose first statement is `wait N cycles` lowers to an empty block
   whose terminator *is* the content. `src/ir/verify.rs` (module doc)
   therefore permits empty blocks terminated by `Branch` or a
   suspension as well; only an empty `Fatal` block is flagged, because
   the design synthesizes the fail action into that block's statements
   — emptiness there means the synthesis dropped its body. Two more
   invariants need no runtime check at all: 5 (exactly one terminator)
   and 7 (no suspending `Stmt`) hold by construction of the
   `BasicBlock`/`Stmt` types.

6. **Pragmatic IR nodes not in the design.**
   - `Stmt::FailDiag { guard, args }` (`src/ir/mod.rs`): one
     `wait until ... timeout` diagnostic line. v1 bumps `errors`
     exactly once per timed-out wait — that bump rides the
     `WaitUntilTimeout` terminator's timeout edge — while the header
     and per-sub-predicate "not yet true:" lines print without
     bumping. A guarded/unguarded log-only statement is the smallest
     node that reproduces that contract; reusing `AssertCheck` would
     double-count errors.
   - `Stmt::CovReport` + `Expr::CovBin`: check-phase `cov.report()`
     and `cov.<point>.<bin>` reads. The design routed coverage through
     `Stmt::CoverSample(CovgroupId, Vec<Expr>)`; in the MVP, sampling
     is schema-driven at emission (the bin counters live in the
     emitted covergroup struct, sampled from a `_checkers` closure in
     registration order), and `SamplerAuto` function bodies are empty
     registration markers. Unknown point/bin names are hard lowering
     errors — v1 deferred those to a C++ compile failure.
   - `WaitUntil`/`WaitUntilTimeout` carry `Vec<PredSrc>` + `WaitMode`
     (`Single`/`AllOf`) instead of the design's single `pred: Expr`.
     `PredSrc` keeps each sub-predicate's pretty-printed source text so
     the timeout breakdown names the user's expressions exactly as v1
     does. `any of` stays rejected. The `on_timeout` block also
     **rejoins `on_fire`** rather than terminating in `Return` as the
     design's synthesized-fail-handler rule prescribed — v1 semantics
     are log-FAIL, bump errors once, and continue the test.

7. **`TestSchema.clocks: Vec<ClockSpec>` with resolved `period_ps`.**
   The design references a `TestSchema` but never pins its fields.
   Codegen needs concrete picoseconds for the clock scheduler, so the
   schema carries each declared clock with its period resolved from
   the time literal or `domain ... freq_mhz` declaration at lowering
   time (`src/ir/mod.rs`, `ClockSpec`).

8. **`FmtArg.wide_hex: Option<(usize, bool)>`.** The design-era
   skeleton had a `bool`; emission of a `:WWx`/`:WWX` capture with
   WW > 16 needs the digit width and the case to route through the
   wide-hex runtime helper, so the flag is widened to carry both.

Minor, same spirit: `IndexVec` is a plain `Vec` plus typed id structs;
`FunctionKind` has no `TransactorBody` (transactors are out of
subset); the design's `AssertFail` enum collapsed into a single
`FmtArgs on_fail` because both source forms bump `errors` identically
in v1.

### Verifier coverage summary

Implemented: invariants 1–4, 6, 8 (amended), 10, 15, plus the
port-position rule. By construction: 5, 7. Not implemented: 9 (no
`ConstraintRef` in the IR yet), 11 (no `Fork`), 12 (no DUT port table
— see divergence 1), 13 (not separately checked; the port-position
rule covers the `PortRef` half), 14 and 16 (the v0 front end does not
type-check, so `IrType::Unknown` is the common case and only
locally-determinable `Assign` types are compared).

## Negative tests: where rejection actually fires

The randomize fixture (`axilite_constraint_test.harc`) and the
agent/event fixture (`wait_until_quiesce_test.harc`) are registered as
must-reject tests, but today both trip the **item-level** gate on
their `transaction` declarations before any statement-level construct
(`randomize`, `agent`, `event<T>`) is reached — the file-level scan in
`src/ir/lower/mod.rs` runs before body lowering. The snapshot text
therefore names `transaction`, not the deeper construct. When a future
PR brings `transaction` into the subset, those snapshots will shift to
the statement-level rejection; `tests/tbir.rs` documents this at each
test site so the shift is expected, not alarming.

## Next steps

The remaining work is the plan doc's (gate redefined 2026-06-12 —
see its decision log):

- **Phase-4 completion** (plan phases 4–6): grow the tbir backend to
  v1's full feature set with equivalence-registry rows (including
  expect-fail) for the whole fixture corpus, flip the default to tbir,
  delete v1. No byte-parity step.
- **Passes** (plan phase 7): `placement` over a `TargetProfile`,
  `randomize_analysis`, `extract_port_set`, `hoist_stimulus`,
  `lower_coroutine` for FSM-shaped backends.
- **Subset growth**: transactions/randomize (needs the constraint-IR
  `ConstraintRef` seam), transactors (`CallTarget::TransactorMethod`
  is already declared in `src/ir/mod.rs` but nothing produces it),
  `fork`, scoreboards, range/cross bins, `any of`.
- **Placement-split backends** proceed per the multi-target placement
  model in [tb-ir-design.md](tb-ir-design.md) (tiers, timing classes,
  `TargetProfile` capability checks) once the passes exist.
