# #619 M4a — preserve reusable testbench ownership in the TB-IR

Status: **in progress** (design spike + first slice). Branch:
`claude/issue-619-m4a-ir-ownership`.

M4a is the IR-only half of #619's Milestone 4: stop the TB-IR lowering from
inlining each reusable testbench's lifecycle/methods into every bound test, and
instead represent them once with explicit call edges. No split-output machinery
is touched here (that is M1/M2); no out-of-line non-capturing emission is done
here (that is M4b). The gate is the existing v1↔tbir equivalence harness.

## Current data flow (verified on `main` @ 45396c78)

1. `lower_program` (`src/ir/lower/mod.rs:930`) captures `tb_of_test` (which
   testbench each impl-for test binds) **before** desugaring clears
   `for_testbench` (`:933`).
2. It then calls v1's desugaring:
   `let file = crate::codegen::cpp_tb::desugar_impl_for_test_in_file(file)`
   (`:945`). For every `impl … for Tb` test, `desugar_impl_for_test_in_file`
   (`src/codegen/cpp_tb.rs:23811`) **copies** the bound testbench's
   `setup`/`check`/`teardown` lifecycle bodies into a synthetic `sim` ScopeDecl
   on that test, and rewrites bare field refs to `_tb.<field>`
   (`cpp_tb.rs:23934`+). After this pass, a testbench's lifecycle body exists as
   N independent copies — one per bound test — and the copies are
   indistinguishable from test-owned statements.
3. Downstream lowering builds `FunctionKind::Run`/`Check` per test from the
   desugared bodies. There is **no** `FunctionKind` for reusable testbench
   lifecycle or methods — the ownership is already gone by the time the IR is
   built (`FunctionKind`, `src/ir/mod.rs:1890`).

## The equivalence coupling — and why it is more tractable than feared

The load-bearing worry (see #619 comment plan, PR 1) was randomize-site
numbering. Verified detail:

- The solver problem table is built from the **desugared** file
  (`src/ir/lower/mod.rs:1902`, `build_typed_solver_problem_table(&file)`), and
  `randomize_problem_ids` is keyed by the randomize target's **source span**
  `(span.start, span.end)` (`:1906`).
- Because desugaring copies bodies by `clone()`, every copy of a lifecycle
  randomize site carries the **same original source span**. The span→problem_id
  map therefore already collapses the N copies to one id
  (`cpp_tb.rs` comment confirms "a testbench lifecycle phase lands in BOTH"
  tables, `lower/mod.rs:1938`).

**Consequence:** `problem_id` is span-keyed, i.e. source-derived, so lowering a
lifecycle body **once** (shared) yields the *same* `problem_id`s as copying it N
times. The static id assignment is stable under share-vs-copy.

What must still be preserved is the **runtime draw order**: the point at which
the shared lifecycle call executes must be exactly where the inlined body used
to run, in the required lifecycle order (issue §"Lifecycle Ordering", steps
1–9), with DUT wired before first setup. If the call sits where the inline sat,
per-cycle event order and RNG draw order are unchanged.

Other span/order-keyed identities to re-verify before/after each slice:
`PropertyCheckId`, `CoverCheckId` (`SideTables`, `lower/mod.rs:1896`) — same
span-keyed reasoning expected, but assert it with a trace-diff.

## Chosen direction (per #619 §"Required IR Changes")

Native impl/testbench lowering that retains ownership, replacing the TB-IR
dependency on lifecycle-copying desugaring. v1 keeps `desugar_impl_for_test_in_file`
unchanged (Non-Goal: no v1 refactor).

New `FunctionKind` variants (names per issue, flexible):
`TestbenchLifecycle { testbench, phase }`, `TestbenchMethod { testbench }`, and
test-owned `TestRun`/`TestCheck` (today's `Run`/`Check`, gaining a `test`
owner). Reusable bodies lower once; each owning test carries explicit call
edges in lifecycle order.

## Slicing (each slice a full producer→consumer vertical — `-D warnings` forbids dead variants/fields)

- **Slice 1 (this branch, first):** introduce the ownership representation and a
  tbir-native lifecycle-lowering path **behind an internal switch**, emitting
  `TestbenchLifecycle` functions + call edges, with the emitter **re-inlining**
  them at the call site so generated C++ (and thus the trace) is unchanged.
  Gate: full equivalence sweep clean. This proves the seam with zero observable
  delta before any sharing is attempted.
- **Slice 2:** testbench methods (`TestbenchMethod`) the same way.
- **Slice 3:** flip the switch to make native lowering the tbir default for
  impl-for tests; delete the tbir dependency on the desugaring for lifecycle.
  Gate: equivalence + negative-diagnostic fixtures.

M4b (out-of-line, non-capturing emission) then consumes this ownership; it is a
separate branch and depends on M3's context seam.

## Producer mechanism (concrete — mapped from the code)

The producer (Slice 2) is what constructs the ownership variants. The exact
seam is now pinned:

### Where the inlining happens

`desugar_impl_for_test_in_file` (`src/codegen/cpp_tb.rs:23811`) builds a
`tb_lifecycle` ScopeDecl from the bound testbench's `Lifecycle` items, then
merges it into each test's `sim` scope via `merge_lifecycle_blocks`
(`cpp_tb.rs:24189`+), in this exact order:

- `sc.setup    = [wire _tb.dut = dut] ++ tb.setup ++ impl.setup`
- `sc.check    = tb.check ++ impl.check`
- `sc.teardown = impl.teardown ++ tb.teardown`

lower_test then splits the merged scope: `run_stmts = lets ++ bare_before ++
sc.setup ++ sc.run`, `check_stmts = sc.check ++ sc.teardown ++ bare_after`
(`src/ir/lower/mod.rs:5361`). After the merge the tb-owned and test-owned
statements are concatenated within each phase block — the boundary is gone.

### Constraints that shape the design

1. **`desugar_impl_for_test_in_file` is shared across 9 call sites** (6 in v1's
   `cpp_tb.rs`, 1 in `solver/problem_table.rs:323`, 1 in tbir lowering
   `lower/mod.rs:945`). v1 must be untouched (Non-Goal), so native behavior
   cannot be a global change to this function — it must be an opt-in the tbir
   lowering path selects.
2. **DUT-wire-before-setup is load-bearing** (`cpp_tb.rs:24167`): the
   `_tb.dut = dut` wire must precede the first `_tb.dut.*` read, so any shared
   `tb.setup` call must sit *after* the wire.
3. **The testbench component decl keeps its `Lifecycle` items after desugar** —
   desugar copies them into tests but does not strip the component. So native
   lowering can read the phase bodies from the `components` map
   (`lower/mod.rs:966`) and lower them once, independent of the folded copies.

### Chosen seam: opt-in lifecycle-call desugaring + marker lowering

- Add an opt-in to the desugar entry (a `share_lifecycle: bool` parameter or a
  tbir-only sibling) that, when set, replaces the `tb.setup/check/teardown`
  splice in `merge_lifecycle_blocks` with a single synthetic **marker call**
  statement (e.g. an internal `__tb_lifecycle_<phase>()` call), keeping the wire
  and the impl-owned statements exactly where they are. Only tbir's `:945` call
  passes `true`; every v1 / solver call site passes `false` and stays
  byte-identical.
- In lowering: (a) lower each bound testbench's `Lifecycle` phase body **once**
  into a `TestbenchLifecycle { testbench, phase }` function (from the
  `components` map); (b) lower each marker call to an explicit IR call edge to
  that function, positioned exactly where the inlined block used to be.
- Emitter (re-inline intermediate): a call to a `TestbenchLifecycle` function is
  emitted by **re-inlining** the callee body at the call site, so generated C++
  — and the trace — is byte-identical to today. Only with M3's context seam
  (M4b) do these become real out-of-line calls.

### Why this preserves the trace

- `problem_id`s for randomize sites inside lifecycle are span-keyed, so lowering
  the body once yields the same ids as N copies.
- Re-inlining at the marker position reproduces the exact statement order the
  merge produced, so runtime draw order and per-cycle event order are unchanged.
- The wire stays first; the shared `tb.setup` call sits immediately after it,
  preserving the DUT-wired-before-setup invariant.

### Sub-steps

1. **[done — commit e32501e0]** Switch plumbing: `desugar_impl_for_test_sharing_lifecycle`
   substitutes a `__harc_tb_lifecycle_<phase>()` marker for each testbench
   lifecycle body; tbir lowering selects it under `HARC_TBIR_NATIVE_LIFECYCLE`.
   Defaulted OFF; unit-tested; `cargo test` green.
2. Lower testbench `Lifecycle` phases → `TestbenchLifecycle` functions (behind
   the switch), appended to `prog.functions`; verify accepts them.
3. Marker-call → call-edge lowering + emitter re-emit. Gate:
   `tests/run_tbir_equiv.sh` clean with the switch ON.
4. Flip the switch to tbir-default; remove the tbir dependency on lifecycle
   inlining. Gate: equivalence + negative-diagnostic fixtures.

### Sub-step 2–3 design decision (precedent found)

Testbench METHODS (`_tb.reset()`) are already **CFG-inlined at lowering time**
by `lower_tb_method_call` (`src/ir/lower/stmts.rs:330` — "CFG-inlined like an
impure helper"). That inline is precisely the pattern that carries **no
ownership** — it is what M4a replaces. So the marker must NOT be lowered by the
same CFG-inline (that would reproduce today's no-ownership state); it must lower
to a **call edge** to a once-lowered `TestbenchLifecycle` function, and the
**emitter** must re-emit that function's blocks at the call site (a new emitter
capability — today the emitter never inlines a call, because lowering already
did). This is the atomic 2–3 unit:

- add a `CallTarget::TestbenchLifecycle { function }` (or equivalent) + lower the
  marker to it, reusing the `_tb`-field resolution the method-inline path uses;
- lower each phase body ONCE into the `TestbenchLifecycle` function (the desugar
  already rewrites the body to `_tb.<field>` before marker substitution — capture
  that rewritten body so the re-emitted output is byte-identical);
- teach the tbir emitter to expand a `TestbenchLifecycle` call inline.

It cannot be verified in halves: switch-ON needs the marker lowered AND the
emitter expansion together, then `tests/run_tbir_equiv.sh` (serial Verilator)
must diff clean. This is the multi-iteration core of M4a.

## Validation

- Primary gate every slice: `tests/run_tbir_equiv.sh` (serial — concurrent
  Verilator sweeps invent phantom failures), plus `cargo test`.
- Watch rows with: randomize inside lifecycle, suspending lifecycle
  (`wait`/`co_await`), property/cover checks in lifecycle, multiple tests
  sharing one testbench.
