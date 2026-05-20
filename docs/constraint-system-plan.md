# Constraint System Plan

HARC's current constraint-randomization path is a working v0: transaction
`keep` clauses and `randomize(t) with ...` bodies are merged at the call site
and emitted directly as an inline Z3 block in C++ testbench codegen. That keeps
early examples useful, but it is not the right semantic foundation for a robust
HVL. The v1 path should put a typed elaboration layer between HARC syntax and
any solver backend.

This document defines that foundation. The first implementation step is
non-invasive: extract schemas and constraint metadata from the parsed AST while
leaving generated C++ behavior unchanged.

## Architecture

Constraint handling should flow through four layers:

1. **Record elaboration** builds a schema for every struct and transaction:
   fields, widths, signedness, enum domains, non-random markers, defaults,
   attributes, record-level keeps, and `when` subtype bodies.
2. **Typed constraint IR** lowers `keep`, `randomize with`, field attributes,
   and relation expansions into a checked expression tree with origins and
   source spans.
3. **Solver backend** translates the typed IR to Z3 or another solver. It owns
   bit-vector widths, signed vs unsigned operators, enum finite domains, named
   assertions, model extraction, and unsat reporting.
4. **Runtime randomization** owns deterministic seeding, distribution behavior,
   uniqueness history, queued solve delivery, and blocking solve calls.

The existing `src/codegen/cpp_tb.rs` Z3 string emission remains the behavioral
source of truth until each layer replaces it with matching tests.

## Elaboration Model

The elaborated record schema records the facts needed before solver lowering.
Transactions and structs use the same field and constraint schema; transactions
remain the randomization entry point, while structs provide reusable aggregate
field and keep definitions.

- field path, source span, type class, bit width, signedness, enum domain
- `!` non-random status and whether the field has a default
- field attributes such as `[range]`, `[dist]`, and `[unique]`
- top-level `keep` constraints with their origin
- `when` subtype discriminants and nested fields/keeps for future guarded
  subtype lowering

Nested record fields are represented by dotted paths such as `hdr.addr`, with
the path components preserved structurally. When a transaction contains a
struct field, the struct's keeps are represented as prefixed constraints on
the containing record, so future solver lowering sees the same semantic field
paths that current codegen already emits.

Relations are collected as named constraint sets with parameter schemas and
body shape. Expansion should eventually preserve origin chains so diagnostics
can say which relation contributed a failing or unsupported constraint.

## Solver Boundary

The solver backend should consume typed IR, not raw AST or emitted C++ text.
That boundary lets HARC reject unsupported constraints early and keeps solver
semantics independent of the C++ backend.

Required solver semantics for v1:

- exact bit-vector width for every numeric field
- signed operators for `sint<N>` comparisons, division, and modulo
- unsigned operators for `uint<N>`, `bits<N>`, `bit`, and enum domains
- finite-domain constraints for enums
- named assertions for `keep`, `with`, relation, subtype, and attribute origins
- model extraction that respects non-random fields and enum domains

Unsupported forms should produce compile-time diagnostics with spans. The
solver path should not silently replace unsupported expressions with `true`.

## Randomization Semantics

Randomization must be deterministic per seed. The long-term runtime model is:

- fast PRNG sampling for unconstrained and simple per-field cases
- typed solver calls for cross-field constraints and relation-expanded bodies
- explicit handling for `[dist]`, `[unique]`, and solve-order hints
- reproducible model diversity that does not rely on ad hoc static blocking
  caches alone

Transaction fields are random by default. HARC should not add a SystemVerilog-
style `rand` marker for ordinary fields or future aggregate fields. The existing
`!` marker is the opt-out: `!field` means the field is non-random and retains
its current/default value unless user code assigns it directly.

`dist` and field distribution attributes are semantic weights, not hard solver
constraints. The implementation may use rejection sampling, randomized solver
objectives, or a hybrid strategy, but the chosen behavior must be documented
and stable under a seed.

Current v1 codegen uses deterministic seeded candidate preferences for ordinary
solver-backed randomization. Those preferences are added on a temporary solver
stack and dropped if they conflict with hard user constraints, so a sampled
preference cannot create a false UNSAT. Persistent cross-call history is
reserved for explicit policies such as `[unique]`, not used as ordinary
diversity.

`solve_order(a, b, c)` is solver scheduling metadata, not a boolean
constraint. The v1 code generator should validate that its arguments are
fields of the randomize target and use the metadata to order free-field
preference sampling without changing the hard solution space; richer
distribution effects can be added in the typed solver backend once sampling
order is explicit.

Auto coverage goals are solver preferences, not hard constraints. The v1
runtime derives goals for enum variants, bool values, literal `[range]`
min/max endpoints, and natural numeric min/max endpoints where the solver path
can represent the field without truncation. Natural unsigned numeric goals also
include capped walking-one and walking-zero patterns so wide buses get useful
bit-position pressure without requiring redundant `[range]` attributes. The
runtime then derives capped pairwise crosses from those goals. Uncovered goals
and crosses should be preferred ahead of ordinary seeded samples when the
participating fields remain unconstrained; explicit user constraints and
non-random fields must exclude the corresponding auto goals. If an auto
coverage preference makes the first solver check UNSAT, the runtime should mark
that generated goal as blocked, skip it on later attempts, and retry without
treating the preference as a hard failure. The same runtime state should report
hit/miss/blocked summaries at test end so users can see which generated
preferences still need attention.

The current scaffold supports full-width unsigned `uint<N>`/`bits<N>` solver
preferences and model extraction up to the language target of 1024 bits, so
wide-bus auto min/max and walking patterns are represented structurally instead
of being truncated through `uint64_t`. Signed values above 64 bits remain a
typed-lowering follow-up because their comparison and division semantics need
explicit sign-extension rules in the solver backend.

Future aggregate constraints should keep the same default-random rule. A random
list/array should look like an ordinary transaction field, not `rand list<...>`.
The list length should be exposed as a built-in list property or method, for
example `items.len()`, rather than as a separate random field that users must
manually keep coherent. A future syntax could model `items : list<uint<8>>` and
use ordinary constraints such as `items.len() <= 16`; solver lowering can infer
the finite unroll bound from those length constraints and lower
`sum(items[0..items.len()]) == total` by unrolling guarded element terms. The
exact syntax is still open, but the random/non-random split is not: aggregate
fields are random unless prefixed with `!`.

Declared covergroup crosses are separate from these solver preferences:
`cross cp_a, cp_b, ...` is a functional coverage declaration. It is sampled at
the covergroup trigger, records only bin combinations hit in that same sample
event, and reports missing bin tuples at `report()` time. It does not by itself
constrain or steer randomization; solver steering comes from the auto coverage
preference path above.

`[unique]` is also a randomization policy, not a hard field invariant. It
should steer fields that remain unconstrained after `keep`, relation expansion,
and `randomize ... with` constraints; explicit user constraints and direct
assignments must win. A unique field should avoid repeats until its currently
legal value space is exhausted, then clear/recycle history and retry rather
than reporting UNSAT. The runtime history should be designed so a future
solver-result cache can be queried across a selected group of tests without
making type-level attributes impossible to override.

## Queued vs Blocking Randomize

Queued randomize is the default v1 performance path: constraints that depend
only on transaction state and compile-time constants can be solved off-cycle
and delivered through a result queue.

`blocking randomize` is required when constraints depend on current runtime
state that cannot be safely precomputed. The compiler should eventually perform
dependency analysis and either:

- permit queued solving for static or captured-snapshot constraints, or
- require/blocking-lower the call when live DUT or simulation state is read.

The current codegen ignores the parsed `blocking` flag. That is acceptable for
v0 but must not be the v1 architecture.

## Diagnostics

Every lowered constraint needs an origin:

- transaction-level `keep`
- randomize-with body
- relation expansion
- field attribute
- future `when` subtype guard/body

UNSAT reporting should include the transaction type, randomize site, active
origins, and named solver assertions. Unsupported syntax should fail during
typed lowering, before C++ emission.

## Migration Order

1. Add foundation IR and schema extraction without behavior changes.
2. Lower the currently supported constraint subset into typed IR.
3. Replace ad hoc Z3 emission with exact-width, signedness-aware solver
   lowering.
4. Enforce non-random field semantics.
5. Lower `when` subtype constraints with discriminator guards.
6. Replace diversity blocking cache with deterministic seed-driven sampling.
7. Implement principled `[dist]`, `[unique]`, and `solve_order`.
8. Add queued vs `blocking randomize` architecture and runtime dependency
   analysis.
