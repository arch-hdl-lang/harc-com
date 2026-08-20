# TB-IR gap-repair workflow

## Scope

TB-IR is the product path. The v1 backend is used only as a temporary
behavioral oracle for source forms that it demonstrably compiles and runs;
this work does not add features to v1 or preserve v1-only architecture.

The audit baseline is `origin/main` at `e17b938d` (2026-08-20). It contained
174 `unsupported(...)` constructor call sites under `src/ir/lower` and no
such sites in `src/codegen/tbir`. That raw count is an inventory, not a work
queue: some sites cover several source shapes, and some diagnostics were
incorrectly classified as gaps even though v1 rejects the same source.

## Source of truth and ordering

Work the following evidence in order:

1. A v1-accepted fixture that TB-IR rejects. This is the highest-priority
   migration blocker.
2. A `tests/tbir.rs` case that asserts `LowerError::Unsupported` and also
   proves the corresponding v1 source emits, compiles, or runs. These tests
   are the existing list of real, out-of-corpus gaps.
3. Remaining raw `unsupported(...)` sites. Probe v1 before treating one as a
   compatibility gap. If v1 rejects or mis-lowers it, reclassify the
   diagnostic and keep it out of the v1-equivalence queue.

Within the proven list, prefer a shared IR/runtime mechanism that removes a
family of rejections over a one-site exception. Current implementation order:

- [x] Component event-field subscriptions in a run/check body
  (`on component.event(payload)`).
- [x] Discarded `queue.size()` / `queue.empty()` statements across all five
  queue-owner spellings.
- [x] Method-hook paths: statement-position registration, nested component
  receivers, and non-transactor component receivers.
- [ ] Heartbeat predicates on transactor receivers.
- [ ] Bound-transactor `thread` items routed through the component path.
- [ ] Coverpoint target/value gaps with working v1 behavior (sized literals,
  supported width forms, and other explicitly probed cases).
- [ ] Record/state/helper gaps whose tests contain a positive v1 control.

This list intentionally excludes v1 failures such as a blocking bus call
nested in an expression. Those may still be useful TB-IR enhancements, but
they are not retirement blockers and come after the proven migration gaps.

## Faster burn-down

Treat the 167 remaining constructor call sites (168 textual matches including
the `unsupported` helper definition) as an inventory, not 167 separate tasks.
Maintain a generated migration manifest with one row per executable
source shape: owning lowering function, diagnostic class, v1 evidence,
shared IR primitive, and equivalence fixture. Then:

1. Automatically exclude `Rejects`/`EmitsUncompilable`/`SilentlyMisLowers`
   cases from the retirement-blocker queue while retaining their diagnostics.
2. Cluster the remaining rows by shared IR primitive and implement a whole
   family at once. Method hooks, transactor predicates, bound threads,
   coverpoint values, and record/state/helper handling are the current seams.
3. Use one self-checking trace fixture per family, with small unit probes for
   each surface spelling and malformed neighbor.
4. Run targeted lowering/verifier tests per edit, simulations per family, and
   the expensive full equivalence registry only at family boundaries.
5. Track two metrics separately: proven migration blockers removed (primary)
   and raw `unsupported(...)` constructors removed or reclassified
   (diagnostic cleanup). A falling raw count alone can hide no user-visible
   progress.

Heartbeat predicates on transactor receivers are the recommended next family:
the remaining positive-v1 tests converge on receiver resolution plus existing
activity-stamp state, making it the next promising shared seam.

## Per-gap implementation loop

1. Keep a positive v1 control and a nearby negative control in
   `tests/tbir.rs`; do not infer behavior from emitter text alone when the
   construct has runtime effects.
2. Add or extend the smallest typed IR node/schema that represents the
   behavior. Validate references and payload/type shape in `ir/verify.rs`.
3. Lower every equivalent surface spelling through the shared path and keep
   precise `Invalid`/`NotImplemented` diagnostics for malformed neighbors.
4. Emit from TB-IR without replaying AST lowering in the backend.
5. Convert the former rejection test into a lowering + verifier test.
6. Add a self-checking fixture to `tests/tbir_equiv_fixtures.txt`; runtime
   effects must be observable in assertions or semantic traces.
7. Run, in increasing cost order:

   - targeted Rust tests;
   - `harc check` and both emitters' `sim --emit-only` paths;
   - both simulations plus `harc trace-diff` for changed fixtures;
   - `tests/run_emit_parity.sh`;
   - `tests/run_tbir_equiv.sh`;
   - the complete Rust test suite.

8. Recount `unsupported(...)` sites and record which family disappeared.
   The completed event, queue-query, and method-hook slices reduce the call-site
   count from 174 to 167. Nested bus expressions were also reclassified because
   v1 rejects them too.
9. Before a PR, obtain the independent findings-first review required by
   `AGENTS.md`, address its findings, mark the reviewed HEAD, and run
   `scripts/pre_pr_review.sh check`.

## Completion criteria

The migration queue is empty when every v1-runnable source form represented
by the proven tests lowers through TB-IR and has a passing equivalence fixture.
Raw unsupported sites may remain only for constructs that v1 also rejects or
mis-lowers, with diagnostics that say so and do not advertise v1 as an escape
hatch.
