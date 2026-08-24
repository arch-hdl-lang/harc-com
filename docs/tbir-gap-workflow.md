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
- [x] Heartbeat predicates on transactor receivers.
- [x] Component heartbeat predicates (`idle`, `idle_in`, `idle_out`, and
  `quiesced`) in expression, inferred-let, assignment, and discarded-value
  positions, including component-typed parameter receivers.
- [x] Bound-transactor `thread` items routed through the component path.
- [x] Direct coverpoint value gaps with working v1 behavior (sized literals,
  runtime slice/lane selectors, and directly sampled or narrowed wide values).
- [x] Composed wide cover expressions and width-preserving wrapping arithmetic
  (wide unary/binary/ternary operands are coerced through the 1024-bit scalar
  model; known-width `+%`/`-%`/`*%` cover expressions retain their 1–64-bit
  mask; wider wraps remain excluded because v1 rejects them).
- [x] Record-value destination gaps with positive v1 controls: fitting sized
  field defaults, record queue pops into existing locals, and whole-record
  testbench-field copies.
- [x] Whole fixed-vector component state in the C++-valid value positions:
  same-shape equality/inequality and whole-array copies, with direct and
  self-relative component spellings.
- [x] Persistent data-scoreboard unsigned scalar state through 1024 bits,
  including cross-phase reads/writes and width-checked IR assignments.
- [x] Default-constructed record locals inside scalar-valued pure helpers.
- [x] `keep` constraints on randomized `struct` values, including nested
  record prefixing and component-only solver sites, merged through the same
  typed constraint site and Z3 path as transaction keeps.
- [x] A fixed-vector selection inside component and bound-responder record
  state (`bundle.data[i]`, `bundle.records[i].field`), with verifier-checked
  path/index metadata and v1/TBIR runtime parity. Component paths with
  multiple selections remain classified by the shared unsupported diagnostic.
- [x] Persistent whole-record data-scoreboard state, including direct
  testbench fields and env-nested scoreboards, exact record-identity checks,
  and cross-phase whole-record copies.
- [x] Persistent data-scoreboard list declarations for scalar and
  one-dimensional fixed-vector elements, using exact `std::vector` storage
  with verifier-checked element and vector-length metadata plus read-only
  `size()`/`empty()` queries.
- [x] Lazy interpolation calls in immediate assert/assume diagnostics and
  wait-timeout messages. Safely hoistable, unconditionally evaluated calls
  lower inside the failure or timeout CFG arm, preserving non-evaluation on
  success and source order on failure. Statement-producing calls beneath a
  short-circuit or ternary branch and concurrent property messages remain
  outside this CFG-based slice.
- [x] Scalar-element dynamic `list<T>` fields in transaction/struct records,
  including unconstrained draws and bounded `len()`/`sum(...)` constraint
  solving through the shared randomize runtime. Ordinary body-position list
  indexing remains a separate slice. List field `range`, `dist`,
  and `unique` modifiers are rejected as silent v1 mis-lowerings until the
  element-wise modifier semantics are modeled.
- [x] Read-only `.len()`, `.size()`, and `.empty()` queries on dynamic record
  lists across record locals, component record state, and bound-target
  transactor record state, including indexed fixed-vector record paths.
- [ ] Remaining state/helper gaps whose tests contain a positive v1 control.

This list intentionally excludes v1 failures such as a blocking bus call
nested in an expression. Those may still be useful TB-IR enhancements, but
they are not retirement blockers and come after the proven migration gaps.

## Faster burn-down

Treat the 126 remaining constructor call sites (127 textual matches including
the `unsupported` helper definition) as an inventory, not 127 separate tasks.
Maintain a generated migration manifest with one row per executable
source shape: owning lowering function, diagnostic class, v1 evidence,
shared IR primitive, and equivalence fixture. Then:

1. Automatically exclude `Rejects`/`EmitsUncompilable`/`SilentlyMisLowers`
   cases from the retirement-blocker queue while retaining their diagnostics.
2. Cluster the remaining rows by shared IR primitive and implement a whole
   family at once. Method hooks, transactor predicates, bound threads,
   coverpoint values and the remaining record/state/helper handling are the
   current seams.
3. Use one self-checking trace fixture per family, with small unit probes for
   each surface spelling and malformed neighbor.
4. Run targeted lowering/verifier tests per edit, simulations per family, and
   the expensive full equivalence registry only at family boundaries.
5. Track two metrics separately: proven migration blockers removed (primary)
   and raw `unsupported(...)` constructors removed or reclassified
   (diagnostic cleanup). A falling raw count alone can hide no user-visible
   progress.

Composed wide cover expressions are complete: scalar widths now flow through
unary, binary, and ternary cover expressions, and the sampler coerces mixed
operands to `_harc_u128` or `HarcWide<N>` before applying the operator. A direct
coverpoint sample intentionally observes the low 64 bits, matching v1's sample
storage, while wide intermediates retain their width until that boundary.
Known-width wrapping cover arithmetic lowers through the same explicit
`WidthCast::Trunc` representation as general expressions. The self-checking
fixture runs under both emitters and trace-diffs wide add, bit-not, ternary, and
wrapped-nibble samples. Wrapping above 64 bits remains a measured v1 rejection,
not a retirement blocker.

The recommended next family is the remaining state gaps with a positive v1
control. Cluster those sites by the missing shared IR value shape rather than
by source spelling, and keep verifier type metadata in the same patch as each
new lowering path.

Wide unsigned data-scoreboard state is complete: `uint`/`bits` fields through
1024 bits reuse the native, `_harc_u128`, and `HarcWide<N>` carriers already
used by TB-IR scalar locals. The verifier audits the persistent-state schema
and rejects width-losing writes; the existing scoreboard fixture proves a bit
above 128 survives from `run` into `check` under both emitters and trace-diffs
clean. Wide signed scoreboard state remains excluded until the wide carrier
has signed value semantics. This source-shape family shares the
generic unsupported field-type constructor with non-scalar fields, so that
batch left the raw inventory at 153 textual matches. The pure-helper
record-local batch below removes one constructor, leaving 152 textual matches
(151 constructors plus the helper). The struct-keep batch removes the dedicated
rejection and leaves 151 textual matches (150 constructors plus the helper).
The indexed record-state batch removes a proven migration blocker but shares
its diagnostic constructor with still-excluded malformed and multi-selection
component-path shapes, so the raw textual count remains 151.
The whole-record scoreboard batch similarly shared its field-type diagnostic
with the then-unsupported dynamic `list` fields, so it removed another proven
migration family while leaving the raw textual count at 151.
The scoreboard-list batch closes those measured scalar and fixed-vector list
declarations. It shares the same field-type diagnostic with unsupported list
element shapes, so the raw textual count remains unchanged.
The event-routing batch then removes ten dedicated routing rejections, leaving
141 textual matches (140 constructors plus the helper).
The lazy diagnostic-call batch closes the immediate-check and wait-timeout
source families and consolidates two transactor message-context rejections into
one lazy-call diagnostic, leaving 140 textual matches (139 constructors plus
the helper).
The record-list randomize batch closes the scalar-element declaration and
constraint family. Its explicit source boundary for ordinary body-position
list queries reuses the consolidated boundary diagnostic, leaving 140 textual
matches (139 constructors plus the helper).
Subsequent mainline batches reduce that inventory to 131 textual matches. The
component built-in predicate batch removes the two remaining method-routing
rejections (path and component-typed parameter receivers), leaving 129 textual
matches (128 constructors plus the helper). Its existing agent fixture now
trace-checks inferred-let, assignment, and discarded-value predicate uses. The
dynamic record-list query batch then removes its consolidated boundary
constructor, leaving 128 textual matches (127 constructors plus the helper),
and extends the existing record-list runtime-equivalence fixture with all three
query spellings. The wide cover-selector batch removes the shared lowering
gate for DUT lanes, hook-record lanes, and dynamic bit-slice bounds, leaving
127 textual matches (126 constructors plus the helper). Because v1 emits
uncompilable C++ for these wide selectors, their compile/run gate is TBIR-only.

## Review-derived semantic gates

The method-hook batch exposed defects that lowering-only tests could not see.
For each new family, explicitly mark every applicable gate below as covered or
not applicable before requesting review:

1. **Source order and registration time.** Exercise a source action before and
   after registration/declaration; do not collect a construct into an unordered
   side table and replay it at phase entry unless the language says it is
   elaboration-time.
2. **Natural completion versus explicit return.** If behavior fires at function
   exit, separately test fall-through and every explicit-return path. Keep
   natural-return metadata verified as distinct, in-range `Return` blocks.
3. **Phase and capture lifetime.** A closure registered in `run` may fire in
   `check`; captured locals must outlive both phases or the source must be
   rejected precisely. Test mutation through the capture, not only reads.
4. **Declaration order and ABI.** Emit handler/callback bodies only after every
   callable they may reference is declared. Cover signed scalars, records,
   sequences, and component values so closure-vector and method signatures
   cannot drift.
5. **Owner/type isolation.** Include two tests or two same-typed instances and
   prove subscriptions/state do not leak across test owners while intentional
   type-scoped fan-out still reaches siblings.
6. **Generated-name hygiene.** Mutate or author source names that resemble the
   generated identifiers. Generated storage must use a collision-proof name
   and an explicit source-to-storage remap.
7. **Verifier corruption probes.** For every new id/path/side-table relation,
   mutate the lowered IR to an out-of-range, mismatched, duplicate, and
   wrong-kind value where applicable; each corruption must fail verification
   before codegen.
8. **Runtime boundary proof.** A family is not complete at `lower_src` or emitted
   C++ shape. Its self-checking fixture must compile and run under both emitters,
   trace-diff clean, and survive the full equivalence registry at the family
   boundary.

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
   The completed event, queue-query, method-hook, transactor-heartbeat, and
   bound-thread, direct coverpoint-value, composed-wide-cover, record-value
   destination, whole component fixed-vector, and wide unsigned scoreboard
   state, pure-helper record-local, struct-keep, scoreboard-list, and lazy
   diagnostic-call, component-predicate, dynamic record-list query, and wide
   cover-selector slices reduce the count from 174 to 127 textual matches (126
   constructors plus the helper definition). The
   scoreboard source families did not remove constructors because unsupported
   non-scalar fields share it. Nested bus expressions and sized cover widths
   were also reclassified because v1 rejects or silently mis-lowers them.
9. Before a PR, obtain the independent findings-first review required by
   `AGENTS.md`, address its findings, mark the reviewed HEAD, and run
   `scripts/pre_pr_review.sh check`.

## Completion criteria

The migration queue is empty when every v1-runnable source form represented
by the proven tests lowers through TB-IR and has a passing equivalence fixture.
Raw unsupported sites may remain only for constructs that v1 also rejects or
mis-lowers, with diagnostics that say so and do not advertise v1 as an escape
hatch.
