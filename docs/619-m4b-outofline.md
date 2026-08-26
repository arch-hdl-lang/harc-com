# #619 M4b — out-of-line, non-capturing emission of reusable testbench lifecycle

Status: **starting** (design spike). Branch: `claude/issue-619-m4b-outofline`, off
`main` including M4a (`6e104dd3`) and M0–M3 (#684).

## Goal

M4a gave the IR **ownership**: each reusable testbench `setup`/`check`/`teardown`
is a single `FunctionKind::TestbenchLifecycle` function, referenced from every
owning test via `Terminator::TbLifecycleCall`. But the emitter **re-inlines** the
callee at each call site (`src/codegen/tbir/func.rs`, the `TbLifecycleCall` arm),
so the generated C++ is still duplicated per test/shard — M4a is trace-identical,
not de-duplicated.

M4b makes the emission **out-of-line**: emit each `TestbenchLifecycle` function
**once** as a real C++ function, and lower `TbLifecycleCall` to an actual **call**
to it. Combined with the common-object split (M1/M2), the shared lifecycle body is
then compiled **once per suite** instead of once per test — the actual
de-duplication #619 is about. This is also where the tbir default switch flips.

## Prerequisites now on main

- **M4a ownership** (`6e104dd3`): `TestbenchLifecycle`/`TestbenchMethod`,
  `TbLifecycleCall`, the sharing desugar + conservative fallback, all behind
  `HARC_TBIR_NATIVE_LIFECYCLE`.
- **M3 context seam** (#684): a deepened `HarcTestContext`
  (`src/codegen/tbir/runtime.rs:636`+) that owns the DUT, clock scheduler
  (`eval_clocks_until`), trace, log, coverage, and error state, exposed via
  methods — the interface a non-capturing shared function reaches runtime
  behavior through.
- **M1/M2 separate-compilation** (`SeparateCppPlan`, `emit_suite_interface` /
  `emit_suite_common`, `src/codegen/tbir/mod.rs:482`+): the common `.hpp`/`.cpp`
  the out-of-line lifecycle function will live in.

## The two hard problems (why M4b > M4a)

1. **Non-capturing parameters.** The re-inlined lifecycle body today reads test-
   frame locals directly (`_tb`, `dut`, `harc_rng`, `_checkers`, cover counters,
   temporal latches). An out-of-line function cannot capture those — they must be
   passed explicitly. M3's `HarcTestContext&` covers DUT/scheduler/trace/log/RNG;
   the owning `_tb` instance is passed as the owner param. Anything still reached
   by capture must move behind the context/owner or be proven test-local.
2. **Suspension across the call.** A lifecycle body may `wait`/`co_await` (the
   M4a happy-path fixture's `setup` calls a `wait`-bearing `reset()`). An
   out-of-line function that suspends must itself be a coroutine
   (`harc_rt::HarcThread`) and the caller must `co_await` it. The re-inline
   sidestepped this by pasting the body into the caller coroutine; M4b must make
   the shared function a coroutine and thread the `ThreadSlot`/suspension through
   the call. This is the crux and the main equivalence risk (per-cycle timing,
   RNG draw order across the suspension boundary).

## Approach (proposed — to be refined by the first investigation slice)

- Add an emit mode (a new flag, or extend `HARC_TBIR_NATIVE_LIFECYCLE`) that, for
  `TbLifecycleCall`, emits a real call to an out-of-line `TestbenchLifecycle`
  function instead of re-inlining.
- Emit the `TestbenchLifecycle` function as a coroutine
  `harc_rt::HarcThread <name>(HarcTestContext& ctx, <Tb>& _tb, ThreadSlot* _slot)`
  (exact signature TBD from the context seam), with its body lowered from the
  once-lowered CFG, referencing `ctx.`/`_tb.` instead of captured locals.
- At the call site, `co_await` the shared coroutine so an inner `wait` suspends
  the caller identically to the inline.
- Keep the M4a re-inline path as the fallback / OFF behavior during bring-up;
  gate the switch flip on equivalence.

## Validation

- Same gate as M4a: `tests/run_tbir_equiv.sh` (serial) must trace-diff clean v1==
  tbir with the out-of-line emission ON, especially the suspending-lifecycle,
  randomize-in-lifecycle, and multi-test-per-testbench rows.
- Structural gate (the point of M4b): under the common split layout, a sentinel
  shared lifecycle body appears **once** in the common `.cpp` and **zero** times
  in any test shard.
- Then flip the tbir default and drop the re-inline dependency (M4a sub-step 4
  part 2 lands here, once out-of-line is proven).

## First slice

Investigate the exact `HarcTestContext` interface and the coroutine/`ThreadSlot`
calling convention M4a's re-inline currently relies on, then emit ONE
non-suspending lifecycle body out-of-line (a `setup` with no `wait`) as a plain
function call — proving the non-capturing parameter passing — before tackling the
suspending-coroutine case.

## Calling-convention findings (M4b investigation slice)

### Which emit path the equivalence gate actually exercises

`harc sim --codegen tbir` calls `tbir::emit` → `emit_selected_tests(…,
EmitTail::Whole)` (`src/codegen/tbir/mod.rs:214`) — the **monolithic** path.
The separate/split paths (`emit_separate_*`, `emit_split_*`) are a different
mode not driven by the equivalence harness. So the first M4b slice targets the
monolithic path only; split/separate shards keep the M4a re-inline (they do not
emit the file-scope definition, so they must not emit a call to it).

Consequence for M3: `emit_selected_tests` uses the **simple** `context_struct`
(`runtime.rs:613`) plus `run_prologue` (`runtime.rs:794`), NOT the deepened
`context_struct_deepened`. The deepened context (with `rng`, `scheduler`,
`eval_clocks_until`, `tick`, `_clocks`) is emitted only into the M1/M2
separate-compilation **common** file (`emit_suite_common` → `context_methods`,
`mod.rs:982`), which the monolithic sim never compiles. **The out-of-line
lifecycle function therefore cannot assume the deepened context** — it reaches
runtime state through the SIMPLE context's members plus the run coroutine's
frame constructs.

### The simple `HarcTestContext` interface (what a shared function can reach)

Simple `context_struct` (`runtime.rs:613`) members, all public:
`dut` (`V<dut>*`), `tfp`/`_wave_path` (`#if HARC_TRACE_ENABLED`), `_trace_time`,
`errors` (int), `_fatal` (bool), `cycle_count` (int), `trace`
(`HarcTraceWriter`), `log_ctx` (`HarcLogContext`), and three
`std::vector<std::function<void()>>` service lists: `_checkers`,
`_post_eval_services`, `_auto_cov_reports`. No methods — it is a plain state
bag; `tick`/`eval_clocks_until`/`sim_log_line` are **coroutine-frame locals**,
not context members.

### What the re-inline reads from the run coroutine's frame

`run_prologue` aliases the ctx members into bare locals (`auto* dut = ctx.dut;
auto& errors = ctx.errors; …`), then `log_helpers_and_seed` (`runtime.rs:962`)
defines `sim_logf_line` / `sim_log_line` as `[&]` lambdas over
`log_ctx`/`trace`/`cycle_count`, plus the `sched`/`_run_slot` scheduler and the
`_run_slot_lambda` coroutine (`mod.rs:3506`). The clock scheduler
(`clocked_scheduler`/`clockless_scheduler`) defines `now_ps`, `clocks_`,
`eval_clocks_until`, `tick`, and the trace-dump lambdas `_harc_trace_dump_next`
/ `_harc_trace_dump_at` — all coroutine-frame locals. The re-inlined lifecycle
body (`func::emit_function`) emits **bare names** (`dut`, `_tb`, `errors`,
`sim_log_line`, `harc_rng`, `_checkers`, …) that resolve against these captures.

Test-frame names a lifecycle body may reference, and where each lives:
- `_tb` — the owning testbench host struct instance (declared per-test in
  `emit_test`, `mod.rs:3054`). Field reads/writes lower to `_tb.<field>`
  (`TbFieldWrite`, `expected`/`seen_setup`, etc.). **Must become a parameter.**
- `dut`, `errors`, `_fatal`, `cycle_count`, `trace`, `log_ctx`, `_trace_time`,
  `_checkers` — all **simple-context members**; reachable as `ctx.<member>`.
- `sim_log_line` / `sim_logf_line` — coroutine-local lambdas; **must be
  reconstructed** inside the out-of-line function from `ctx` members.
- `harc_rng` — a **file-scope global** (`runtime.rs:139`); reachable directly.
- `_slot`/`ThreadSlot`, `tick`, `eval_clocks_until`, `now_ps`, `clocks_`,
  `_harc_trace_dump_*` — coroutine-frame-only; a body that touches these
  **cannot** go out-of-line in the non-suspending slice → kept re-inlined.
- cover counters — file-scope statics (`mod.rs:342`), reachable — BUT a
  concurrent `cover`/property registers a `[&]`-capturing closure into
  `_checkers`, which would capture the out-of-line frame and dangle; such
  bodies are excluded from out-of-line (kept re-inlined).

### Coroutine calling convention (for the future suspending case)

A tbir coroutine is `harc_rt::HarcThread f(harc_rt::ThreadSlot* _slot)`; a `wait`
inside lowers to `co_await harc_rt::wait_cycles(_slot, N)` (and
`wait_until`/`wait_until_timeout`). Precedent for calling one with a slot: the
run body itself — `mod.rs:3500`+ declares `harc_rt::ThreadSlot _run_slot;`,
`sched.slots.push_back(&_run_slot);`, builds `_run_slot_lambda = [&]
(harc_rt::ThreadSlot* _slot) -> harc_rt::HarcThread { … co_return; };`, then
`_run_slot.thread = _run_slot_lambda(&_run_slot);`. Actor bodies
(`func.rs:3613`, `declare_method_slot`) follow the same shape with
`register_actor_slot`. So a suspending out-of-line lifecycle would be a
`harc_rt::HarcThread <name>(HarcTestContext& ctx, <Tb>& _tb, harc_rt::ThreadSlot*
_slot)` and the call site would `co_await <name>(ctx, _tb, _slot);` — passing the
**caller's own `_slot`** so the inner `co_await wait_cycles(_slot, …)` suspends
the caller's slot identically to the inline. (Not yet implemented — see the
suspending-case assessment below.)

### Recommended out-of-line signature

- **Non-suspending (implemented):** `static void
  _harc_lc<name>(HarcTestContext& ctx, <Tb>& _tb)`. Prologue reconstructs the
  ambient names from the two params (`dut`/`errors`/`_fatal`/`cycle_count`/
  `trace`/`log_ctx`/`_checkers` alias `ctx.*`; `sim_log_line`/`sim_logf_line`
  rebuilt as local lambdas); `harc_rng` stays the file-scope global. Call site:
  `_harc_lc<name>(ctx, _tb);`.
- **Suspending (proposal):** `harc_rt::HarcThread _harc_lc<name>(HarcTestContext&
  ctx, <Tb>& _tb, harc_rt::ThreadSlot* _slot)`, called `co_await
  _harc_lc<name>(ctx, _tb, _slot);`. One emit mode cannot cleanly cover both:
  the non-suspending form is a plain function (no `_slot`, no `co_await` at the
  call site), the suspending form is a coroutine `co_await`ed with the caller's
  slot. The eligibility split (below) routes each body to the right form; the
  suspending form is future work.

### Eligibility gate (this slice)

A `TestbenchLifecycle` body is emitted out-of-line iff EVERY terminator is
`Jump`/`Branch`/`Return` (no `Wait*`, `Randomize`, `TbLifecycleCall`, `Fatal`)
and EVERY statement is in a strict whitelist of value/DUT/`_tb`/log/assert forms
(`Assign`, `DutWrite`, `DutRead`, `ProbeRelease`, `RecordInit`,
`RecordFieldWrite`, `TbFieldWrite`, `TbFieldVecElementWrite`, `TbQueuePush`,
`TbQueuePop`, `Log`, `AssertCheck`, `AssumeCheck`) — see
`func::lifecycle_out_of_line_eligible`. Anything outside keeps the M4a
re-inline. Per-testbench this is per-phase: a testbench's `check` can go
out-of-line while its wait-bearing `setup` stays re-inlined (verified against
`testbench_lifecycle_test`).

### Gate wiring / flag

No new env var: out-of-line emission is enabled by the existing
`HARC_TBIR_NATIVE_LIFECYCLE` (which alone produces the `TestbenchLifecycle`
functions + `TbLifecycleCall`s at lowering time). `emit_selected_tests` computes
the eligible set (monolithic `EmitTail::Whole` only), emits each eligible body
once via `func::emit_lifecycle_function` right after `context_struct`, and
threads the FunctionId set into `func::emit_function`; the `TbLifecycleCall` arm
emits a real call for set members and re-inlines everything else. Split/separate
shard paths pass an empty set (re-inline preserved).

## Suspending slice — coroutine out-of-line (M4b crux)

The suspending case extends out-of-line emission to lifecycle bodies that
`wait`/`co_await`. It is implemented in the monolithic path behind the same
`HARC_TBIR_NATIVE_LIFECYCLE` switch.

### The runtime problem, and the parent-drives-child solution

A raw `co_await <HarcThread>` does NOT work: `HarcThread` has no awaiter
interface, and the scheduler resumes a single `slot->thread` handle per
`ThreadSlot` — a nested coroutine has a different handle, so parking the shared
slot inside the child would still make the scheduler resume the *parent*. Rather
than rework `ThreadSlot`/`HarcThread`/the scheduler (which every fixture
depends on), the run coroutine **drives the child directly** on the shared slot:

```cpp
{ auto _lc_sub = _harc_lc<name>(ctx, _tb, _slot); _lc_sub.resume();
  while (!_lc_sub.done()) { co_await harc_rt::harc_lifecycle_yield(); _lc_sub.resume(); }
  _lc_sub.destroy(); }
```

`_lc_sub.resume()` runs the child to its next `co_await wait_*(_slot, …)`, whose
awaiter parks the **shared** `_slot` (WaitCycles/WaitUntil/…) and returns to the
parent. The parent then `co_await harc_rt::harc_lifecycle_yield()` — a new
awaiter whose `await_suspend` touches NO slot field, so it leaves the child's
wait state (and `slot->thread` = the run coroutine) exactly as written. When the
scheduler marks the slot Ready and resumes `slot->thread`, the parent wakes and
re-drives the child. The child's suspensions ARE the parent's suspensions on one
slot — the same `co_await wait_*(_slot, …)` the M4a re-inline pasted straight
into the run coroutine, so timing and per-cycle event order are identical.
`await_ready` is always false, so the parent parks exactly once per child
suspension (no lost or doubled cycle). The only runtime addition is the
`HarcLifecycleYieldAwaiter` / `harc_lifecycle_yield()` (sim-internal; no
user-facing surface).

### Out-of-line signatures

- Non-suspending: `static void _harc_lc<name>(HarcTestContext& ctx, <Tb>& _tb)`,
  called `_harc_lc<name>(ctx, _tb);` (Plain variant).
- Suspending: `harc_rt::HarcThread _harc_lc<name>(HarcTestContext& ctx, <Tb>&
  _tb, harc_rt::ThreadSlot* _slot)`, driven by the loop above (Coro variant).

Both reconstruct the identical ambient-name prologue
(`emit_lifecycle_ambient_prologue`), so the two emitters share one body-text
environment. The classifier `func::lifecycle_shareable_kind` returns
`Some(Plain)` / `Some(Coro)` / `None`, and `emit_function` takes a
`HashMap<FunctionId, LifecycleEmit>` mapping each shared body to its variant;
the `TbLifecycleCall` arm is three-way (plain call / drive loop / re-inline).

### What is shareable vs still-excluded (this slice)

Shareable at all requires EVERY statement in the whitelist
(`stmt_out_of_line_safe`: value/DUT/`_tb`/log/assert). Then terminators decide:

| Construct | Terminator | Verdict |
|---|---|---|
| control flow, `randomize` | `Jump`/`Branch`/`Return`/`Randomize` | Plain-safe |
| `wait N cycles` (no clock) | `WaitCycles(None)` | **Coro** |
| `wait until` | `WaitUntil` | **Coro** |
| `wait until … timeout` | `WaitUntilTimeout` | **Coro** |
| `wait N cycles on <clk>` | `WaitCycles(Some)` | re-inline (needs frame-local `clocks_`/`eval_clocks_until`) |
| method-body `wait` (`reset()`) | `WaitCyclesSync` | re-inline (needs frame-local `tick()`) |
| `after <duration>` | `WaitTimePs` | re-inline (needs frame-local `now_ps`/`eval_clocks_until`) |
| `log(fatal,…)` / `fatal` term | `Fatal` | re-inline (conservative) |
| concurrent `cover`/property/`on`, transactor call | (statement) | re-inline (dangling `[&]` capture / frame-local `tick`) |

Notably, a lifecycle body that `wait`s by calling a testbench METHOD (e.g. the
existing `testbench_lifecycle_test` `setup` → `reset()`) lowers those waits to
`WaitCyclesSync` (synchronous `tick()` loop), so its **setup stays re-inlined**
while its non-suspending **check goes out-of-line (Plain)** — verified. Only a
`wait` written DIRECTLY in the lifecycle body lowers to coroutine `WaitCycles`
and reaches the Coro variant.

### RNG-order risk — does not materialize (excluded upstream)

A `randomize` inside a lifecycle makes the M4a **sharing desugar** classify the
whole testbench UNSAFE to share (side-table state), so no `TestbenchLifecycle`
function is minted at all — the body is never emitted out of line and falls back
to per-test re-inline. `tb_lifecycle_rand_suspend_test` confirms this: 0
out-of-line lifecycle symbols, v1==tbir trace-clean. So the RNG-draw-order
question across a coroutine boundary is moot in this slice — such bodies are
never shared. (If a future slice wants to share them, it must first make the
sharing desugar prove RNG order, then the Coro emitter already handles the
`Randomize` terminator via the file-scope `harc_rng` global.)

### First-resume / per-cycle-timing risk — clean

`tb_lifecycle_wait_setup_test` (setup waits directly → Coro coroutine, bound by
two impls) trace-diffs clean v1==tbir under the switch. The parent-drives-child
model preserves the exact scheduler interaction: the child's first statements
run during the same `bootstrap()` pass the inline body would (driving `rst`
before the first posedge), and each subsequent wait parks the shared slot for
the same cycle count. Unit test `m4b_suspending_setup_emitted_once_as_coroutine`
pins the shape: one coroutine definition, two drive loops, one plain check.

## Split/common slice — cross-shard de-duplication (the #619 payoff)

Monolithic emission de-dups within one translation unit. The point of #619 is
compiling the shared lifecycle body **once per suite** across split shards. This
slice puts the out-of-line definitions in the split/common `.cpp` and makes
every shard call them.

### Entry point

`harc sim --codegen tbir --cpp-split tests --cpp-split-layout common`
(`main.rs`, `CodegenKind::Tbir if split.layout == CppSplitLayout::Common`) drives
`plan_separate_tests` → `emit_separate_interface_with_prefix` (→ `suite.hpp`) →
`emit_separate_common_with_prefix` (→ `common.cpp`) → `emit_separate_shards`
(→ `test_<Name>.cpp` / `shardN.cpp`) → dispatcher (`main.cpp`).
`--cpp-split-group-size N` controls tests-per-shard (1 → one shard per test).
This is a DIFFERENT mechanism from the self-contained split
(`emit_split_shard` → `emit_selected_tests(EmitTail::ShardBody)`), which emits a
complete standalone TU per shard.

### Ambient-context reconciliation (deepened vs simple) — none needed

The split shard's run body is emitted by the SAME `emit_test` / `run_prologue`
the monolithic path uses: it constructs `HarcTestContext ctx;` and aliases
`auto* dut = ctx.dut; auto& errors = ctx.errors; …`, and uses the file-scope
`harc_rng`. The split header declares the DEEPENED `HarcTestContext`
(`context_struct_deepened`), but that struct is a strict SUPERSET of the simple
one — every member the ambient prologue reads (`dut`, `tfp`, `_trace_time`,
`errors`, `_fatal`, `cycle_count`, `trace`, `log_ctx`, `_checkers`) exists in
both. `harc_rng` is `extern` in the header and defined once in `common.cpp`. So
the out-of-line body reaches ambient state through `HarcTestContext& ctx` +
`<Tb>& _tb` + the `harc_rng` global identically to monolithic — the SAME
`emit_lifecycle_ambient_prologue` works verbatim, no reconciliation required.
(The deepened context's own methods `start`/`tick`/`finish` remain unused by the
current run body; the run body still uses `run_prologue`'s inline scheduler.)

### What lands where now

- **`suite.hpp`** (interface): a forward declaration per shareable body —
  `harc_rt::HarcThread _harc_lc<name>(HarcTestContext&, <Tb>&, harc_rt::ThreadSlot*);`
  (Coro) / `void _harc_lc<name>(HarcTestContext&, <Tb>&);` (Plain, EXTERNAL
  linkage). Replaces the old `M2: … prototypes would follow here` placeholder.
- **`common.cpp`**: the single external-linkage DEFINITION of each body
  (`emit_shared_lifecycle_defs(…, static_linkage = false)`), after the helper
  defs and `context_methods`.
- **`test_<Name>.cpp` / shard**: the `TbLifecycleCall` lowers to the real call
  (`_harc_lc<name>(ctx, _tb);`) or the parent-drives-child drive loop
  (`co_await`), NOT a re-inline. Genuinely-ineligible bodies still re-inline.

Linkage: monolithic and self-contained-split TUs keep `static` Plain defs
(internal, unchanged — no regression); only the common layout emits `void`
(external) + header prototype. The Coro def is external in every layout (it was
already, and monolithic verified clean), so only the prototype is layout-specific.

### Structural evidence (real build + run, switch ON, group_size 1 → 2 shards)

`tb_lifecycle_wait_setup_test` (Coro setup + Plain check, two impls):
- `common.cpp`: setup coroutine DEF ×1, check DEF ×1.
- `suite.hpp`: setup + check PROTOTYPEs.
- `test_WaitSetupSix.cpp` / `test_WaitSetupTwo.cpp`: setup DEF ×0, drive-loop
  ×1, check-call ×1 each.
- Built and ran: `ALL TESTS PASSED`.

Same shape for `tb_lifecycle_nowait_test` (Plain-only): setup+check DEF ×1 in
common, ×0 in each shard, called from both. Switch OFF: zero `_harc_lc` symbols,
`ALL TESTS PASSED` (regression guard). Pinned by unit test
`m4b_split_common_layout_emits_lifecycle_once_in_common`.

## Default flip — native lifecycle is now the tbir default (M4a sub-step 4 part 2)

Native out-of-line testbench-lifecycle lowering is now the **default** for
`--codegen tbir`, removing tbir's dependency on v1's lifecycle-copying desugar
for the default path (the core #619 "Required IR Changes" goal).

- `native_lifecycle_enabled()` (`src/ir/lower/mod.rs`) now defaults ON: unset ⇒
  native. `HARC_TBIR_NATIVE_LIFECYCLE=0` is the debug opt-OUT retained for the
  transition (issue M8); any other value keeps native on. The re-inline /
  historical-inline fallback arm is UNCHANGED — ineligible bodies
  (clock/method/time waits, side-table registrants, unshareable testbenches)
  still fall back; the flip only chooses the desugar, not the per-body emitter
  decision.
- Test harness: `tbir_lifecycle_ownership::with_switch(false)` now sets `=0`
  (explicit opt-out), not "unset" — because unset now means ON.

### Churn scope (full)

Flipping the default changes tbir emission for every impl-for test with a bound
testbench. Enumerated by running `cargo test` with the default flipped:

- `tests/tbir.rs::testbench_lifecycle_dump_ir_snapshot` — the ONLY changed
  expectation. The IR-dump snapshot moved from the historical inlined form
  (each test's run/check carrying a private copy of the setup reset + check
  assert) to the native ownership form: `fn0 __tb_lifecycle_..._Setup
  [TestbenchLifecycle(tb0, setup)]` and `fn1 ..._Check` lowered ONCE, with each
  test's run/check now carrying `TbLifecycleCall { fn0/fn1, … }` edges instead
  of inlined bodies. This is the #619 ownership change itself, proven
  trace-equivalent by the equivalence sweep (below); only the structural IR
  shape changed, no behavior. Snapshot updated via `cargo insta accept`.
- No other test expectation changed. `tests/codegen.rs`, `tests/tbir_split.rs`,
  `tests/tbir_separate.rs`, `tests/common_split_e2e.rs` all stayed green
  (they assert structure/trace-equivalent shape, not the old inlined byte form).
- The only remaining `cargo test` failure is the pre-existing local-only
  `differential::a_field_default_too_wide_for_its_u64_slot_is_never_truncated`,
  unrelated to this change (green in CI).

### Gate: the equivalence sweep now runs the flipped DEFAULT directly

`HARC=./target/release/harc JOBS=1 ./tests/run_tbir_equiv.sh` with NO env var
is now a direct gate on native lowering: 213/0. The opt-out
`HARC_TBIR_NATIVE_LIFECYCLE=0 …` (historical inline) is also 213/0.
