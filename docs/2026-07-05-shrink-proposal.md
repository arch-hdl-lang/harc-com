# Design idea: `harc shrink` — automatic failure-case minimization for CRV tests

**Status:** Proposal. Not implemented, not filed as an issue yet — written up
for discussion.
**Date logged:** 2026-07-05.
**Scope:** A new replay primitive plus a `harc shrink` subcommand that turns a
failing constrained-random seed into a minimal reproduction, by mutating and
replaying the decision trace rather than re-deriving stimulus from HARC
semantics.

## 1. Problem

HARC's CRV model is already built around seed determinism and is explicitly
aiming for large seed counts: `spec.md` §7.7 discusses 10K-seed nightly
regressions today (CPU) and describes Phase 7a/7b batch execution (SIMD,
then GPU) scaling to tens of thousands of lanes. Per-test-instance `fatal`
semantics exist precisely so "one bad seed retires from the grid while
siblings continue."

That roadmap solves *throughput*. It does not solve what happens next: when
seed 4821 out of 10,000 fails after several thousand cycles and dozens of
`randomize()` calls, someone has to figure out which of those decisions
actually caused the failure. Today that's a fully manual process — rerun the
seed, add logging, guess which transaction mattered, re-run again. This is
the same "one seed in a haystack" problem every CRV methodology (UVM
included) has always had, and no verification-language tooling — HARC
included — currently automates it.

This is a solved problem in a different corner of testing: property-based
testing frameworks (QuickCheck's `shrink`, Hypothesis) don't just report a
falsifying random seed, they automatically search for the *smallest* input
that still falsifies the property, and report that instead. Fuzzers (AFL/
libFuzzer) do the equivalent with `-minimize`/testcase minimization. HARC is
unusually well positioned to bring the same idea to hardware verification,
because the pieces it needs already exist.

## 2. Why HARC can do this cheaply

Three things are already shipped and do most of the work:

1. **Deterministic replay per seed** (`spec.md` §7.7, §17.4): a HARC run is a
   pure function of its seed (modulo intentionally-external `extern
   function` state, which is out of scope here).
2. **Semantic trace with resolved randomize values**
   (`docs/semantic-trace.md`, `runtime/harc_trace_rt.h:110`): `harc sim
   --record-trace <file>` already emits a `randomize` event per call, in
   program order, carrying the *resolved field values* as a JSON payload
   (`HarcTraceWriter::randomize(cycle, fields_payload)`), plus
   `assertion_failure` events with source location. This is precisely the
   "decision trace" a shrinker needs — it just isn't consumed as an input
   anywhere today; it's write-only, used for post-hoc VCD/log correlation
   (`harc trace-merge`).
3. **A stable failure signature to search against**: the recorded
   `assertion_failure` event (or a `fatal` log event) already identifies
   *which* property failed and *where*, which is exactly the equality check
   a shrink search needs ("does this smaller trace still trip the same
   assertion at the same site?").

The missing piece is small: nothing currently lets a run be *driven by* a
trace instead of a fresh PRNG/solver draw. Add that one primitive and the
rest is a generic, HARC-semantics-agnostic search loop over JSON.

## 3. Proposed design

### 3.1 `--replay-trace <file>` (new runtime primitive)

Add a mode to `harc sim` where, instead of drawing from the PRNG/solver at
each `randomize()` call site, the runtime looks up the next `randomize`
event in a supplied trace file (matched by call-site/sequence index, same
ordering guarantee that already makes trace replay deterministic) and uses
its recorded field values directly. Everything else about the run —
scheduling, DUT evaluation, assertions — proceeds exactly as normal. This
requires no changes to the constraint solver, constraint IR, or codegen
semantics: it's a lookup table substituted at the same call sites that
already exist.

### 3.2 `harc shrink` (new subcommand, orchestration only)

```
harc shrink Test.harc --seed 4821 --dut Foo.arch \
    [--match assert:<name>|fatal] [--max-iters N] [--out shrunk.trace.jsonl]
```

1. Run once with `--record-trace` to capture the failing trace and its
   failure signature (event kind + source span) — already-shipped
   machinery, step 2 above.
2. Run a ddmin-style structured shrink over the trace file as plain data:
   - Numeric fields → binary-search toward 0 (or nearest declared `[range]`
     boundary from the field's schema).
   - Enum fields → try the first-declared variant.
   - Dynamic-length decisions (queue/transaction counts, `schedule`
     permutations, loop trip counts) → try dropping or shortening.
   - After each candidate mutation, replay via `--replay-trace` and compare
     the resulting failure signature to the original. Keep the mutation if
     the signature still matches; otherwise revert and try a smaller
     mutation. Repeat to a fixed point.
3. Emit the minimized trace plus a rendered summary of surviving decisions
   (which fields/transactions actually mattered) — this becomes the artifact
   attached to a bug report instead of "seed 4821, cycle 50,000."

Because the search operates purely on the trace file's recorded values, it
needs no awareness of HARC's constraint language, `keep` clauses, or solver
internals — the solver already did its job once, producing the original
trace; shrinking never calls it again.

## 4. Why this matters now

- It directly leverages already-shipped infrastructure (semantic trace,
  seed determinism); the net-new surface is one replay flag plus a generic
  search loop, not a solver or IR change. Low implementation risk relative
  to most items in `docs/tbir-mvp.md`/`docs/constraint-system-plan.md`.
- It's the natural complement to the Phase 7 CPU-SIMD/GPU batch CRV roadmap
  (`spec.md` §10.1, §7.7): once nightly regressions run in the tens of
  thousands of seeds, the bottleneck stops being simulation throughput and
  becomes triage — a human staring at a handful of failing seeds among
  10,000. Shrinking is what keeps that workflow usable at scale.
- It's a well-understood, well-loved feature borrowed from property-based
  testing (QuickCheck/Hypothesis `shrink`) that current SV/UVM verification
  flows generally lack as a first-class, automatic tool — a concrete,
  demoable differentiator for HARC.

## 5. Non-goals for v1

- Cross-seed generalization (finding a minimal failing seed *class* rather
  than minimizing one trace).
- Any change to constrained-random solving itself — shrinking only mutates
  already-solved values, it never re-invokes Z3.
- GPU/SIMD batch shrinking. This is fundamentally a sequential, single-lane
  search; batch execution stays orthogonal and out of scope.

## 6. Open questions

- Exact trace-event addressing scheme for `--replay-trace` lookup (call-site
  ID vs. strict sequence index) — needs to be robust to minor reorderings
  introduced by the shrink search itself (e.g. dropping a transaction
  shifts the index of every later `randomize` event).
- Whether `extern function` calls with non-deterministic external state
  (spec.md §17, "no `time()` seeding, no static state across calls unless
  explicitly intended") need an explicit opt-out from shrink replay, since
  they're the one documented source of run-to-run nondeterminism.
- Whether field-level shrink strategies should be declarable per-field
  (e.g. `[shrink_hint: ...]`) or purely generic/type-driven for v1.
