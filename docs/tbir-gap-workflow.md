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
- [x] Bare native-width HARC/Verilog-sized scalar literals in general value
  positions and direct register address/reset metadata. General expressions
  use v1-compatible signed host-scalar semantics; metadata sites explicitly
  consume only the value. Width-aware cover/index paths remain unchanged.
  Wider sized values and compound address expressions remain separate slices.
- [x] Nested scalar fixed vectors in transaction/struct record state, using
  the recursive fixed-vector element schema already shared by component state.
  Declaration, zero-initialization, whole-record copy/equality, and packed
  layout lower through TBIR. Record randomization remains fenced because both
  emitters currently generate an invalid scalar assignment for the inner
  array.
- [x] Fixed-vector queue elements across testbench, scoreboard, component,
  and transactor-state owners. Nested scalar vectors retain their exact
  recursive `std::array` layout; inferred pop locals and verifier metadata
  carry the aggregate element type, while safe empty-state queries run under
  both emitters.
- [x] Nested scalar fixed-vector event payloads, retaining every recursive
  `std::array` dimension through event schemas, handlers, emits, connects, and
  test-scope channels.
- [x] Record-leaf fixed-vector event payloads, including recursively nested
  vectors, with resolved record IDs carried through schemas, handlers, emits,
  connects, verifier checks, and C++ callback signatures.
- [x] Wide scalar transactor method returns, retaining the declared signedness
  and width through return slots, method schemas, call destinations, sibling
  calls, and exact `_harc_u128`/`HarcWide<N>` C++ signatures.
- [x] One-dimensional fixed-vector persistent transactor state across unbound,
  bound-target, and bound-initiator owners, including exact recursive element
  metadata, whole-array copies, indexed reads/writes, verifier checks, and
  per-instance `std::array` storage. Recursive state-vector element access is
  an explicit follow-up boundary; recursive dynamic lists remain excluded.
- [ ] Remaining state/helper gaps whose tests contain a positive v1 control.

This list intentionally excludes v1 failures such as a blocking bus call
nested in an expression. Those may still be useful TB-IR enhancements, but
they are not retirement blockers and come after the proven migration gaps.

## Faster burn-down

Treat the 117 remaining constructor call sites (118 textual matches including
the `unsupported` helper definition) as an inventory, not 118 separate tasks.
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
The handler-less unbound-transactor event batch routes scalar and declared-
record event fields through the existing component event path, including
direct emit fan-out. It closes the positive-v1 gaps in both declaration
positions while leaving the raw count at 127 because the shared state-field
diagnostic still rejects directional non-event fields.
The native-width sized-scalar batch shares the validated literal parser across
general expressions and direct address-like metadata, using v1-compatible host
scalar values while consuming only numeric values at metadata boundaries. It removes
the dedicated address-site rejection and leaves 126 textual matches (125
constructors plus the helper). A self-checking fixture covers inferred and typed
locals, assignments, helper arguments, comparisons, and DUT writes under both
emitters. Sized values wider than 64 bits still require the wide-word IR, and
compound address forms remain excluded because v1 silently folds them to zero.
The record-valued tseq-yield batch then admits parenthesized and conditional
record expressions through the existing `SeqPush` IR path, removing the bare-
identifier gate and leaving 125 textual matches (124 constructors plus the
helper). Mismatched record/scalar yields retain their measured v1
`EmitsUncompilable` classification.
The signed-scalar connect batch then routes signed component event bridges
through the existing connect IR, leaving 124 textual matches (123 constructors
plus the helper). The single named event-payload batch accepts the only
unambiguous named-argument shape across component-path, self-relative, and
test-scope-local emits, leaving 123 textual matches (122 constructors plus the
helper). Multi-argument named event payloads retain their measured silent-swap
classification.
The component-body dotted-emit batch then resolves child event paths through
the same arbitrary-depth component receiver used by calls and field accesses,
including nested paths below component-typed parameters and lexical shadowing
of same-named self fields. It leaves 121 textual matches (120 constructors plus
the helper). A runtime fixture covers one- and two-level self paths plus the
shadowing parameter case under TBIR. V1 emits self-relative receivers without
`self.` and produces uncompilable C++, so this is intentionally not an
emitter-equivalence row; TBIR is authoritative for the spec-valid form as V1
approaches retirement.
The nested record fixed-vector schema batch then admits
`Vec<Vec<scalar, M>, N>` fields as persistent record values, reusing the
recursive fixed-vector IR, verifier policy, C++ storage, and pack/unpack walks.
The shared non-scalar record diagnostic still covers other aggregate shapes,
so the raw inventory remains 118 textual matches (117 constructors plus the
helper).

The follow-on nested record fixed-vector indexing batch carries every leaf
selection in source order, so record locals and bound responder record state
can read and write `grid[i][j]` without flattening either dimension. Literal
bounds are checked at each layer, component-record paths retain their
component receiver semantics, and the same fixture now observes nonzero lanes
under both emitters. This routes around existing shared fallbacks, so the raw
inventory remains 118 textual matches.

The fixed-vector queue-element batch then admits fully specified
`queue<Vec<scalar, N>>` elements, including recursively nested scalar vectors,
through the queue schema shared by all persistent-state owners. Its old
diagnostic constructor still classifies incomplete and other aggregate element
spellings, so the raw inventory remains 118 textual matches.

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
   cover-selector, native-width sized-scalar, record-valued tseq-yield, signed
   scalar connect, single named event-payload, component-body dotted-emit, and
   component post-eval cycle-trigger, component event-direction, nested record
   fixed-vector schema, and nested record fixed-vector indexing slices
   reduce the count from 174 to 118 textual matches (117 constructors plus the
   helper definition).
   Fixed-vector and scalar dynamic-list queue elements now route through the
   shared queue-element constructor for every owner; these slices retire real
   TBIR gaps without changing that raw textual count because unsupported enum,
   recursive-list, and other non-scalar neighbors still use the same fence.
   The handler-less unbound-
   transactor event slice closes routes around a shared constructor without
   changing the count. Widthless scalar fixed-`Vec` record fields now route
   through the existing
   fixed-array schema and use v1-compatible 64-bit widthless integer spellings
   (`uint`/`UInt`/`bits`, `sint`/`SInt`) and 32-bit builtin-`int` packed widths;
   this also routes around the shared constructor,
   so that slice does not change the count.
   Nested scalar and record-leaf fixed-vector event payloads similarly route
   around the shared non-scalar event constructor. The current raw inventory
   remains 119 textual matches (118 constructors plus the helper definition).
   Wide scalar transactor returns route around the shared width diagnostic, so
   this slice also leaves that raw inventory unchanged.
   Fixed-vector transactor method parameters and whole-vector call arguments
   route through the same shared aggregate decoder/read fence, so this slice
   likewise leaves the raw inventory unchanged.
   Runtime-address passive `record_read` now carries the regblock decoder in
   `Stmt::RecordRead`, and runtime-address passive `record_write` carries the
   symmetric decoder plus owner-binding callback dispatch in
   `Stmt::RecordWrite`. The shared constant-address diagnostic constructor
   remains for unmatched constants, so these slices leave the raw inventory
   unchanged.
   The scoreboard source families did not remove constructors because
   unsupported non-scalar fields share it. Nested bus expressions and sized
   cover widths were also reclassified because v1 rejects or silently
   mis-lowers them.
9. Before a PR, obtain the independent findings-first review required by
   `AGENTS.md`, address its findings, mark the reviewed HEAD, and run
   `scripts/pre_pr_review.sh check`.

## Completion criteria

The migration queue is empty when every v1-runnable source form represented
by the proven tests lowers through TB-IR and has a passing equivalence fixture.
Raw unsupported sites may remain only for constructs that v1 also rejects or
mis-lowers, with diagnostics that say so and do not advertise v1 as an escape
hatch.
