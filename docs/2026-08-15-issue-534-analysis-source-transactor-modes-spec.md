# HARC: Preserve analysis-source transactor instance modes in TB-IR

**Date:** 2026-08-15
**Status:** Proposed
**Related:** harc-com#534

---

## Problem Statement

Users can declare a reusable analysis-source `transactor` with persistent state,
callable methods, and output events, then bind it as `active` or `passive` in a
reusable testbench. TB-IR internally routes this source construct through its
composite-component representation. That implementation choice currently changes
the source-language contract: the latest compiler accepts a passive binding only
through a broad exception, rejects the corresponding active binding, and drops the
mode before emission.

The broad exception also accepts `passive` on actual env, agent, and scoreboard
component fields, where transactor instance modes are not legal. In addition,
TB-IR flattens the always-on body and `when active` body into one schema, so it
cannot suppress active-only behavior for a passive instance. This prevents
reliable v1-to-TB-IR migration and makes a reusable passive observer unsafe if its
transactor later gains an active surface.

## Solution

Preserve source-declared transactor semantics across the composite-component
lowering route. A component-path binding will carry its effective transactor mode,
while the shared type schema will retain which fields and executable functions
belong to the `when active` surface.

An active instance will expose and register the always-on and active surfaces. A
passive instance will retain independent persistent state, always-on methods,
observation handlers, output-event publication, and analysis fanout, while
excluding active-only access and registration. A field whose declared type is an
actual env, agent, scoreboard, or sequencer will continue to reject a transactor
mode with a precise source-level diagnostic.

A direct testbench transactor field has no inherited mode and therefore requires
an explicit `active` or `passive` annotation. The no-mode case will be tested as a
precise rejection, matching the v1 backend and documenting that no root-level
default exists.

## User Stories

1. As a HARC verification author, I want an analysis-source transactor to accept a passive binding under TB-IR, so that I can express observation-only ownership without selecting the legacy backend.
2. As a HARC verification author, I want the same analysis-source transactor to accept an active binding, so that its source declaration has consistent mode rules regardless of its internal IR representation.
3. As a reusable testbench author, I want a passive analysis relay to retain persistent state, so that counters and accumulated observations remain available to run and check phases.
4. As a reusable testbench author, I want two passive instances of the same transactor type to own independent state, so that activity in one observer cannot mutate the other observer.
5. As a reusable testbench author, I want passive transactors to retain always-on callable methods, so that tests and analysis bridges can submit observations to them.
6. As a reusable testbench author, I want passive transactors to continue publishing always-on output events, so that scoreboards and coverage collectors still receive observations.
7. As a reusable testbench author, I want analysis fanout from a passive source to preserve declaration order and reach every connected sink, so that testbench-root wiring behaves the same as env-owned wiring.
8. As a reusable testbench author, I want a passive binding to suppress all behavior declared inside `when active`, so that adding a driver surface cannot accidentally make an observer drive or schedule stimulus.
9. As a reusable testbench author, I want an active binding to include the `when active` surface and its registrations, so that active-only event handlers and lifecycle behavior execute as declared.
10. As a HARC verification author, I want calls to active-only methods through a passive instance to fail during compilation, so that orphan active code cannot be invoked accidentally.
11. As a HARC verification author, I want accesses to active-only fields or events through a passive instance to fail precisely, so that mode errors are caught before C++ compilation or simulation.
12. As a HARC verification author, I want a direct testbench transactor field without a mode to receive a clear diagnostic, so that I understand that root-level bindings have no inherited default.
13. As a HARC verification author, I want mode annotations on actual env declarations to remain invalid, so that transactor ownership semantics do not leak into unrelated constructs.
14. As a HARC verification author, I want mode annotations on actual agent declarations to remain invalid, so that declaration kind remains authoritative.
15. As a HARC verification author, I want mode annotations on actual scoreboard declarations to remain invalid regardless of their internal schema route, so that method-bearing and data-only scoreboards follow one source rule.
16. As a HARC verification author, I want illegal mode diagnostics to name the source declaration kind and instance path, so that I can fix the binding without understanding TB-IR classification.
17. As a compiler maintainer, I want the source declaration kind to survive internal component classification, so that validation does not depend on overlapping shape-name exception sets.
18. As a compiler maintainer, I want one shared type schema to support active and passive instances simultaneously, so that mixed-mode instances do not duplicate or mutate global function and schema identities.
19. As a compiler maintainer, I want activation provenance recorded once behind a small schema interface, so that call validation, lifecycle registration, and emission apply the same rule.
20. As a compiler maintainer, I want binding mode represented semantically rather than as a bound-driver boolean, so that cooperative and multithreaded emission use the same ownership model.
21. As a compiler maintainer, I want the verifier to reject malformed mode and active-surface metadata, so that later passes cannot silently construct an inconsistent program.
22. As a compiler maintainer, I want dump-IR output to expose binding mode and activation provenance, so that regressions can be diagnosed without inspecting generated C++.
23. As a migration owner, I want legal active and passive cases to be trace-equivalent between v1 and TB-IR, so that the default backend can replace v1 without source forks.
24. As a migration owner, I want the existing passive analysis-fanout behavior to remain working, so that the partial compatibility already present at tip-of-tree is not regressed.

## Implementation Decisions

- The source declaration kind is authoritative. A transactor remains a
  transactor for binding validation even when its storage and methods use the
  composite-component schema.
- The existing composite-component lowering and event/connect machinery will
  remain the structural representation. Analysis-source transactors will not be
  rerouted into the dedicated DUT-poking transactor representation.
- Each component-path instance binding will carry a semantic mode context.
  Source-declared transactor bindings resolve to `active` or `passive`;
  non-transactor component bindings have no mode of their own. A test-scope root
  `let` may additionally carry an inherited-mode context for transactor
  descendants, matching the existing v1 traversal without pretending the root
  env or agent is itself a transactor.
- The existing bound-driver activity boolean will be replaced or subsumed by the
  semantic binding mode. Multithreaded driver emission will ask whether the
  binding includes the active surface rather than interpreting a backend-specific
  boolean.
- A direct testbench transactor field requires an explicit mode. The mode-less
  case is invalid because there is no parent binding from which to inherit a
  mode.
- A mode attached to a field whose declared type is an env, agent, scoreboard,
  or sequencer is a source-level error. A test-scope root `let root : Env active`
  remains a legal inheritance carrier for descendant transactors, because that
  is established v1 behavior. Diagnostics for truly illegal source must not
  suggest selecting v1.
- One type schema is shared by every instance of a component-path transactor. The
  compiler must never prune or mutate that schema based on the first active or
  passive binding it encounters.
- The schema will retain activation provenance through one active-surface facet.
  It identifies fields and lowered functions originating inside `when active`;
  helper queries hide the facet's representation from validation and emission
  callers.
- Persistent state and the physical emitted struct layout may remain a superset
  for both modes. Mode controls observable access and runtime registration, not
  C++ type duplication.
- Always-on methods, state, output events, observation handlers, and analysis
  connections remain available for passive instances.
- Active-only method calls, field accesses, event accesses, and other executable
  surface uses through a passive instance are rejected during lowering with the
  full instance path.
- Event subscriptions, periodic handlers, cycle-trigger handlers, watchdog
  behavior, and bound-driver workers originating in `when active` are registered
  only for active instances.
- Always-on registrations remain installed for both modes.
- Explicit mode metadata on nested transactor fields will be retained. A child
  transactor's explicit mode overrides its inherited root/parent context; an
  unannotated transactor inherits it. A transactor leaf with no explicit or
  inherited mode is invalid. Mode propagation through an unmoded structural
  env/agent node does not turn that node into an active or passive instance.
- The verifier will enforce that only source-declared transactor schemas own
  active-surface metadata, that transactor bindings have a resolved mode, that
  structural field bindings have no declared mode of their own, and that every
  active-surface reference resolves to its owning schema. A root inheritance
  carrier is verified separately from an instance's own mode.
- The textual IR display will include enough mode and activation information to
  diagnose incorrect lowering without relying on emitted-C++ snapshots alone.
- Parser, AST, and pretty-printer syntax do not change; the feature corrects
  semantic preservation after parsing.

## Detailed Semantic Model

### Terms

- **Declared mode** is the optional `active` or `passive` token written at one
  binding site.
- **Inherited mode** is the mode context passed from a test-scope root or a
  structural ancestor while resolving a descendant path.
- **Effective mode** is the declared mode when present, otherwise the inherited
  mode. Every source-declared transactor leaf must have an effective mode.
- **Activation** classifies a transactor member as `always` or `active-only`.
  The latter means that the member originated inside `when active`.
- **Structural component** means an env, agent, scoreboard, or sequencer. It may
  contain a transactor, but it does not itself gain transactor behavior.
- **Analysis-source transactor** is a source `transactor` lowered through
  `ComponentSchema`, rather than through the dedicated bound/DUT-poking
  `TransactorSchema` route.

These concepts must remain distinct. In particular, an inherited mode passing
through an env is path-resolution context, not an assertion that the env itself
is active or passive.

### Mode resolution

For a root and a component path, the effective-mode algorithm is:

1. Begin with the optional mode on the test-scope root binding.
2. Walk sub-component fields from left to right.
3. When a transactor field has an explicit mode, replace the current inherited
   mode with that mode for the transactor and its descendants.
4. When a transactor field has no explicit mode, use the current inherited mode.
5. When a structural field has no mode, preserve the inherited mode while
   traversing it, but do not assign that mode to the structural instance.
6. Reject a mode written on a structural field at the declaration-validation
   seam. It must not become an inheritance override by accident.
7. Reject any transactor leaf whose effective mode remains unresolved.

This algorithm is shared by field/event access, method calls, connect endpoint
resolution, handler registration, lifecycle registration, and nested emission.
Those consumers must not reimplement path traversal independently.

Examples:

| Source shape | Result |
| --- | --- |
| testbench field `relay : Relay active` | `relay` is active |
| testbench field `relay : Relay passive` | `relay` is passive |
| testbench field `relay : Relay` | invalid: no effective mode |
| test-scope `let top : Env active`; `Env.relay : Relay` | `top.relay` inherits active |
| test-scope `let top : Env active`; `Env.relay : Relay passive` | `top.relay` is passive |
| test-scope `let top : Env`; `Env.relay : Relay passive` | `top.relay` is passive |
| test-scope `let top : Env`; `Env.relay : Relay` | invalid at the unresolved transactor leaf |
| field `child : Env active` | invalid: mode is attached to a structural field |

### Activation rules

The lowering walk must preserve whether each item came from the ordinary body or
the `when active` body. It must not flatten the two iterators before recording
that provenance.

| Source item | Schema representation | Passive behavior |
| --- | --- | --- |
| scalar/record/queue field | field plus activation | storage may exist; active-only access is rejected |
| input/output event field | field plus activation | active-only read, emit, or subscription is rejected/omitted |
| hookable/helper method | function plus activation | active-only call is rejected |
| event `on` handler | function plus activation | active-only registration is omitted |
| periodic handler | function plus activation | active-only service registration is omitted |
| cycle-trigger handler | function plus activation | active-only checker registration is omitted |
| watchdog | function plus activation | active-only watchdog registration is omitted |
| always-on item | existing schema representation | available and registered |

The emitted C++ struct may contain storage and lambda definitions for both
surfaces. This is an implementation superset, not source-level exposure. The
binding mode controls access validation and which callbacks become reachable.

`connect` and lifecycle declarations inside `when active` need explicit handling
because they are not ordinary fields or functions. A connect already supported
for that declaration kind will carry `Activation` on its resolved edge and will
be installed only when the binding includes that activation. Component
lifecycle forms not otherwise represented by the analysis-source component
route remain outside this issue and must produce a precise `Unsupported` result
rather than being silently flattened into always-on behavior. A v1 suggestion is
allowed only when v1 supports the exact rejected form.

## Chosen Internal Design

### Alternatives considered

1. **Add another component-name classifier exception.** This could accept the
   active spelling but would still lose the mode and `when active` provenance.
   It cannot satisfy runtime semantics and is rejected.
2. **Clone or prune a schema for every active/passive instance.** This makes
   function IDs and schema identity depend on encounter order, complicates mixed
   instances, and risks per-instance code duplication. It is rejected.
3. **Keep one schema, add an active-surface facet, and carry mode on bindings.**
   This keeps type identity stable while making instance behavior explicit. It
   is the selected design.

The design should expose a small interface rather than leaking side-table
membership checks throughout lowering and code generation. Illustrative types
(names may change during implementation) are:

```rust
enum InstanceMode {
    Active,
    Passive,
}

enum Activation {
    Always,
    ActiveOnly,
}

struct ActiveSurface {
    fields: Vec<String>,
    functions: Vec<FunctionId>,
}

struct ComponentBindingContext {
    component: ComponentId,
    effective_mode: Option<InstanceMode>,
    inherited_mode: Option<InstanceMode>,
    origin: BindingOrigin,
}
```

`ComponentSchema.kind` remains the authoritative source-kind discriminator.
`ActiveSurface` belongs only to a schema tagged as a source transactor. Queries
such as `field_activation(name)`, `function_activation(id)`, and
`includes(mode, activation)` should be methods or helpers beside the schema.
The exact container may be a side facet or per-member flags, but its externally
visible contract and verifier rules are fixed by this specification.

The current `ComponentFieldBinding.active: bool` is too narrow: it means only
"spawn a multithreaded bound driver" and encodes no passive or unresolved state.
It should be replaced or derived from semantic mode. Codegen may still ask a
helper such as `binding.includes_active_surface()` when deciding whether to
create that worker.

Nested `ComponentFieldKind::Sub` entries need to preserve the declared mode
override for source-declared transactor children. The lowering context's
component-field map should return a `ComponentBindingContext`, not just a
`ComponentId`, so all path operations receive mode together with type identity.

### Source-kind catalog

Validation currently runs before all component schemas are available and relies
on several overlapping name sets. Introduce one declaration catalog keyed by
source type name. Each entry records at least:

- authoritative declaration kind;
- selected IR storage route (`ComponentSchema` or `TransactorSchema`);
- whether a field binding requires a transactor mode;
- whether the declaration can own an active surface;
- any existing bound-bus or DUT-poking classification needed by later passes.

Both validation and schema construction consume this catalog. Shape heuristics
may select an IR route, but they must not redefine the source declaration kind or
mode policy. This localizes the fix and removes the need to order special-case
name sets carefully.

### Lowering phases

Implementation should proceed in these phases:

1. Build the declaration catalog from merged AST declarations.
2. Validate every testbench field and nested component field against the
   catalog. A direct analysis-source transactor field requires an explicit mode;
   a structural field rejects one.
3. Build each shared `ComponentSchema` in two source-body walks: ordinary items
   with `Activation::Always`, followed by `when active` items with
   `Activation::ActiveOnly`. Preserve existing FunctionId reservation order.
4. Store activation metadata at the moment each field or FunctionId is created.
   The pass that lowers bodies must repeat the same source order and assert that
   its classification agrees with pass one.
5. Lower testbench roots into binding records carrying type identity, declared
   mode, inherited mode context, origin, and connect edges.
6. Resolve every component path through one helper that returns the final
   component/member plus effective mode. Use it for calls, reads, writes, emits,
   queue operations, idle/quiesced checks, and connects.
7. Reject an active-only member when the resolved transactor is passive. Reject
   an unresolved transactor mode before emission.
8. Run the IR verifier before dump or emission, as today.

An always-on method body that references any active-only sibling member remains
invalid at schema-lowering time, independent of instance mode. Otherwise the
same shared function would be safe for an active instance and unsafe for a
passive one. An active-only method may reference either always-on or active-only
siblings.

### Concrete code touchpoints

The expected implementation footprint is deliberately narrow:

- `src/ir/mod.rs`: define or reuse the semantic mode type; extend
  `ComponentFieldBinding`; retain nested transactor mode overrides; add the
  active-surface contract to `ComponentSchema`.
- `src/ir/lower/components.rs`: replace the flattening
  `items.iter().chain(when_active...)` walks with activation-aware walks in both
  schema reservation and body lowering. Keep existing FunctionId block order.
- `src/ir/lower/mod.rs`: replace the generic `component_type_names` mode gate
  with source-kind policy; carry binding context in `test_scope_components` and
  `LowerCtx.component_fields`; centralize effective-mode path resolution; apply
  activation checks at member-use sites.
- `src/ir/verify.rs`: add the mode-kind, ownership, uniqueness, and dangling
  active-surface invariants.
- `src/ir/display.rs`: render binding mode, root inheritance context, and member
  activation.
- `src/codegen/tbir/mod.rs`: thread mode context through
  `emit_on_handler_regs`, `emit_lifecycle_checkers`, nested recursion, connect
  setup, and the multithreaded bound-driver decision.
- `tests/tbir.rs`: add focused lowering, diagnostic, verifier-mutation, dump-IR,
  and emitted-C++ assertions.
- `tests/fixtures/`, `tests/run_fixtures.sh`, and
  `tests/tbir_equiv_fixtures.txt`: add the behavioral fixture and both test rows.

No parser or AST edit should be necessary. If implementation appears to require
one, first demonstrate which existing syntax or stored mode is missing; the
current parser already accepts the annotations needed by this issue.

### Emission contract

Emission retains one C++ type and one set of lowered functions per schema. For
each root binding it then:

1. computes the effective mode at each transactor node;
2. emits ordinary object state exactly once per instance;
3. installs all `Always` event, periodic, cycle-trigger, lifecycle, watchdog,
   hook, and connect registrations;
4. installs `ActiveOnly` registrations only when the effective mode is active;
5. recurses into structural children while preserving inherited-mode context and
   honoring explicit transactor overrides;
6. creates a multithreaded queue-fed bound-driver actor only for an active bound
   transactor.

The recursive handler and lifecycle emitters must take the mode context as an
argument rather than scanning source declarations again. Active-only lambda
definitions may appear in emitted C++ for a passive instance, but no passive
registration or legal source expression may make them reachable.

For connect edges, always-on endpoints retain current declaration-order fanout.
If an endpoint is active-only, its activation requirement must travel with the
edge or be validated against the resolved binding before subscription. A passive
binding must never receive an active-only subscription merely because the edge
lives on a shared schema.

### Diagnostics and fallback behavior

The following are `LowerError::Invalid` source errors and must not recommend
`--codegen v1`:

- a direct transactor binding with no effective mode;
- a mode on a structural field;
- an active-only method, field, event, or queue operation through a passive path;
- an always-on method body that references an active-only sibling member.

A genuinely unmodeled placement, such as an unsupported active-only connect or
lifecycle form, may be `LowerError::Unsupported` and may suggest v1 only when v1
actually supports that exact source. A malformed program manufactured after
lowering is a verifier error, not a source diagnostic.

Diagnostics should include the declaration kind, enclosing test/testbench,
fully qualified instance path, member when applicable, effective mode, and one
specific remediation. Example shape:

```text
testbench `AnalysisTb` calls active-only method `drive` through passive
transactor `relay`; declare the binding active or call an always-on method
```

### Verifier invariants

The verifier must enforce:

- only `ComponentKindTag::Transactor` owns a non-empty active surface;
- every active field name resolves exactly once in its owning schema;
- every active FunctionId exists and belongs to a method, handler, lifecycle
  callback, or watchdog in its owning schema;
- active-surface references contain no duplicates;
- every direct or resolved source-transactor binding has an effective mode;
- structural field bindings have no declared transactor mode;
- a nested mode override targets a source transactor, not an env, agent,
  scoreboard, or sequencer;
- component binding names remain unique;
- a multithreaded bound-driver worker is requested only for an active binding;
- connect metadata never references an unknown field or function and preserves
  any required activation classification.

Dump-IR should annotate bindings with `mode=active|passive`, fields and functions
with `activation=always|active-only`, and root inheritance carriers separately
from an instance's effective mode. Per-member annotations are preferred over an
opaque list because they make snapshots and failure triage readable.

## Testing Decisions

- The primary seam is a self-proving end-to-end HARC fixture run through parse,
  merge, TB-IR lowering, verification, C++ emission, and simulation. The same
  fixture will be registered in the v1/TB-IR equivalence harness and compared at
  a fixed seed.
- Tests will assert external language behavior and simulation results rather
  than the internal container chosen for active-surface metadata.
- The primary fixture will contain active and passive instances of one
  analysis-source transactor type. It will prove independent persistent state,
  always-on method availability, output-event fanout, and an active-only side
  effect that occurs only for the active instance.
- A focused negative test will prove that a direct testbench transactor field
  without a mode is rejected consistently and explains that no root-level
  inherited mode exists.
- Focused negative tests will prove that active and passive annotations on actual
  env, agent, method-bearing scoreboard, and data-only scoreboard fields remain
  invalid and name the relevant declaration kind.
- A focused negative test will call an active-only method through a passive
  binding and assert a source-oriented diagnostic containing the method and
  instance path.
- Lowering and verifier tests will cover malformed mode-kind combinations and
  dangling active-surface field or function references. These are supporting
  tests below the primary behavioral seam.
- Dump-IR snapshot coverage will lock the resolved binding modes and activation
  classification, while avoiding snapshots of incidental container ordering.
- Emitted-C++ inspection will be limited to behavior that cannot be observed more
  directly: active-only registration must be absent for the passive instance and
  present for the active instance.
- Cooperative and multithreaded code generation will both be exercised because
  the current binding activity flag participates in bound-driver worker
  selection.
- Existing testbench-owned state and analysis-connect fixtures provide prior art
  for persistent host state, component-field binding, and declaration-order
  fanout.
- Existing transactor state, multi-instance, passive-bound, agent-mode, and
  env-mode fixtures provide prior art for independent state, active-only call
  diagnostics, lifecycle registration, and mode inheritance.
- The equivalence registry and trace-diff harness are the acceptance authority
  for backend compatibility; a clean emitted file alone is insufficient.

### Primary behavioral fixture

Add `tests/fixtures/analysis_source_mode_test.harc`, backed by the existing
minimal `Top` DUT. Its analysis-source transactor should have:

- always-on counters for accepted observations, active-only method calls, and
  periodic ticks;
- an always-on output event;
- an always-on `publish(value)` hookable that increments its observation count
  and emits the event;
- an active-only hookable that increments the active-call counter;
- an active-only `on 1 cycles` handler that increments the periodic-tick
  counter.

The reusable testbench should instantiate one active and two passive bindings of
that same transactor type. It should connect each source to a sink and connect at
least one passive source to two sinks. The run/check phases must prove:

1. all three instances can call the always-on `publish` method;
2. the sinks receive the expected values, including passive-source fanout;
3. each transactor and sink owns independent state;
4. the active-only hookable can be called on the active instance;
5. after waiting several cycles, the active instance's periodic counter is
   nonzero and both passive counters remain zero;
6. a second implementation of the same reusable testbench starts with fresh
   instance state.

Do not assert an exact periodic count: backend setup timing may legitimately
differ while still satisfying the language contract. Log stable PASS markers
rather than scheduler-sensitive values so cooperative trace equivalence remains
meaningful.

Register both test implementations in `tests/run_fixtures.sh` and
`tests/tbir_equiv_fixtures.txt`; the latter must use a fixed seed and compare v1
and TB-IR semantic traces in cooperative mode. The `MT=1` sweep remains
verdict-only because its intra-cycle event order is intentionally nondeterministic.

### Focused lowering tests

Use table-driven inline sources where possible. Each accepted program must also
run `verify_program`; each rejected program must assert the error class and
stable semantic fragments rather than an entire message.

| Case | Required assertion |
| --- | --- |
| direct active analysis source | binding mode is active; program verifies |
| direct passive analysis source | binding mode is passive; program verifies |
| active and passive instances of one type | one ComponentId/schema, distinct binding modes |
| direct transactor with no mode | `Invalid`; path and missing effective mode named; no v1 suggestion |
| env field with active/passive mode | `Invalid`; source kind `env` named |
| agent field with active/passive mode | `Invalid`; source kind `agent` named |
| method-bearing scoreboard with mode | `Invalid`; source kind `scoreboard` named |
| data-only scoreboard with mode | same result as method-bearing scoreboard |
| sequencer field with mode | `Invalid`; source kind `sequencer` named |
| active-only method through passive path | `Invalid`; full path, member, and passive mode named |
| active-only field read/write through passive path | `Invalid`; field and path named |
| active-only event emit/access through passive path | `Invalid`; event and path named |
| always-on method references active-only sibling member | `Invalid` at schema lowering |
| active-only method references always-on sibling member | accepted |
| root env mode inherited by nested transactor | effective child mode matches root |
| nested explicit transactor mode override | explicit child mode wins |
| nested transactor with no effective mode | `Invalid` at the leaf path |
| mode on nested structural field | `Invalid`; it is not treated as an override |

Retain the existing dedicated-transactor mode tests. They are regression
coverage for a parallel IR route and must not be rewritten merely to use the new
component-path representation.

### Verifier mutation tests

Start from a valid lowered program, clone it, corrupt one fact at a time, and
assert a precise verifier failure for:

- removing the effective mode from a transactor binding;
- adding a mode to a structural field binding;
- attaching active-surface metadata to a non-transactor schema;
- naming an unknown or duplicate active field;
- naming an unknown FunctionId;
- naming a real FunctionId owned by another schema;
- placing a mode override on a structural child;
- requesting a multithreaded driver worker for a passive binding;
- referencing an unknown or activation-incompatible connect endpoint.

These tests defend the IR contract. They should not duplicate every source-level
negative case already covered at the lowering seam.

### IR and emitted-C++ assertions

One dump-IR snapshot should show, in a stable order:

- the single shared transactor component schema;
- always versus active-only annotations on representative fields and functions;
- active and passive modes on separate bindings;
- a root inheritance carrier separately from a child effective mode, if that
  metadata is represented in the final IR.

Focused emitted-C++ assertions should verify only structural facts that the
runtime fixture cannot isolate:

- active-only function/lambda definitions are emitted once per schema, not once
  per instance;
- the active periodic/event registration is installed for the active instance;
- the equivalent registration is absent for both passive instances;
- always-on subscriptions exist for active and passive sources;
- multithreaded bound-driver worker construction remains active-only.

Prefer scoped substring counts around each instance setup block over global
counts, because unrelated fixtures and helper lambdas may use similar text.

### Verification sequence

During implementation, run the narrow checks after each layer and the broad
checks before declaring the issue resolved:

```bash
cargo fmt --check
cargo test --test tbir analysis_source_component_mode -- --nocapture
cargo test --test tbir component_mode_verifier -- --nocapture
cargo test --test tbir testbench_owned_analysis_connects_lower_and_emit
cargo test --test tbir transactor_instance_mode_rules
cargo test --release
./tests/run_fixtures.sh
SEED=1 ./tests/run_tbir_equiv.sh
MT=1 SEED=1 ./tests/run_tbir_equiv.sh
```

The final test names are illustrative; use names matching the implemented test
group. If the full MT sweep is too expensive for a local inner loop, run direct
v1/TB-IR `harc sim --mt` invocations for the new fixture, but the complete MT
sweep remains the pre-merge expectation.

### Acceptance criteria

The issue is resolved only when all of the following hold:

- the active and passive analysis-source bindings both lower and simulate under
  TB-IR;
- passive always-on state, methods, events, and fanout behave like v1;
- active-only methods and registrations are present for active bindings and
  unreachable for passive bindings;
- mixed active/passive instances share one schema but own independent runtime
  state;
- illegal and unresolved modes fail as `Invalid` with source-oriented
  diagnostics and no backend fallback suggestion;
- nested inherited and overridden modes retain v1 behavior;
- verifier and dump-IR make malformed or lost metadata visible;
- cooperative v1/TB-IR traces match at the registered seed;
- both codegens pass the new fixture under `--mt`;
- existing testbench-owned analysis fanout and dedicated-transactor mode tests
  remain green.

### Implementation hazards to test explicitly

- The schema pass and body pass reserve FunctionIds in a strict order. Adding
  activation tracking must not reorder methods, event handlers, periodic
  handlers, cycle-trigger handlers, or watchdogs.
- The same source type may be instantiated active first or passive first.
  Reverse declaration order in one unit test to prove schema contents do not
  depend on the first binding encountered.
- Recursive registration currently walks all sub-components. A passive child
  nested below an active root must not inherit active-only registration after an
  explicit passive override.
- Existing classifier sets overlap. Tests must include both a method-bearing
  analysis source and a periodic analysis source so the source-kind catalog, not
  classifier ordering, determines mode legality.
- Bound-bus handler emission rewrites some function bodies for the bus prefix.
  Keep an existing active bound-driver fixture in the targeted regression set so
  replacing `active: bool` does not change that path.
- Covergroup subscriptions to hookable methods must follow the method's
  activation. If active-only hook subscription cannot yet be represented, reject
  it explicitly rather than registering it for a passive instance.
- Shared connect schemas must not leak active-only endpoint registrations from an
  active instance to a passive instance of the same type.

## Out of Scope

- New HARC syntax or new mode keywords.
- Introducing a root-level default mode for direct transactor fields.
- Per-mode C++ struct types or physical removal of active-only storage from
  passive object layout.
- Duplicating a component schema or its lowered functions per instance.
- Rerouting analysis-source transactors into the dedicated DUT-poking transactor
  schema.
- Redesigning the v1 backend beyond any test alignment needed to confirm its
  existing behavior.
- A broad rewrite of nested env/agent mode inheritance; the design must preserve
  its metadata and avoid regressions, but this issue is centered on
  component-path transactor fields.
- Unrelated component classification, queue, connect, wide-value, scheduler, or
  parser refactoring.

## Further Notes

- The original passive reproducer now lowers at current tip-of-tree because a
  recent adjacent change accepts `passive` through the generic
  composite-component gate. This is only partial behavior, not resolution of the
  issue.
- Current TB-IR also accepts a mode-less direct component-path transactor, while
  v1 rejects it because the testbench root supplies no inherited mode. This
  specification treats the v1 behavior as authoritative and documents the
  no-mode case as a rejection.
- The current generic exception accepts `passive` on real component kinds and
  must be narrowed as part of this work.
- The component schema already retains a transactor declaration-kind tag. The
  missing information is the per-instance mode and the activation provenance
  that is currently flattened during lowering.
- Existing coverage documentation records hard `when active` elision as an
  unresolved divergence. This issue closes that divergence for the
  analysis-source component path covered here.
