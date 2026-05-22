# Design: Constraint IR — typed elaboration and solver boundary

Status: **Proposed (RFC, not yet implemented).**
Date logged: 2026-05-20.
Companion: [constraint-system-plan.md](constraint-system-plan.md)
(motivation + migration order), [tb-ir-design.md](tb-ir-design.md)
(TB-IR Randomize terminator references the IR specified here),
[tb-ir-plan.md](tb-ir-plan.md) (overall refactor sequence).

This document specifies the concrete shape of the constraint IR: the
typed expression form that replaces the untyped `ConstraintExpr` scaffold
in `src/constraints.rs`, the solver-backend interface that consumes it,
and the runtime randomization API that the typed IR plugs into. The
plan doc covers *why*; this doc is what you read to implement it.

The constraint IR is the **first thing to land** in the broader
refactor sequence: it is independent of the TB-IR (control-flow CFG),
and the TB-IR's `Terminator::Randomize { constraints: ConstraintRef,
.. }` is just a handle into the IR specified here.

## Scope

This doc covers layers 2–4 of the four-layer architecture in
`constraint-system-plan.md`:

- **Layer 1 — Record elaboration.** Already shipped. Out of scope; the
  schemas this layer produces (`TxnSchema`, `TxnFieldSchema`, etc.)
  remain the upstream input.
- **Layer 2 — Typed constraint IR.** This doc specifies the typed
  expression form, the AST → IR lowering, and the verifier.
- **Layer 3 — Solver backend.** This doc specifies the solver-trait
  boundary, the SMT lowering rules, and the Z3 backend's
  responsibilities (named assertions, UNSAT origins, model extraction).
- **Layer 4 — Runtime randomization.** This doc specifies the
  generated-code API (`harc_solve_*` family), the seed derivation
  rules, the queued-vs-blocking call shape, and the runtime semantics
  of `[dist]`, `[unique]`, `solve_order`.

The doc does **not** cover:

- Constraint *syntax* — the surface language is fixed by the parser
  and spec.
- The TB-IR control-flow layer — see `tb-ir-design.md`.

## Current state audit

Concrete starting point as of `2026-05-20` on `main`:

| Layer | Status | Where it lives | Gap |
|---|---|---|---|
| 1 — Record elaboration | ✅ shipped | `src/constraints.rs:267 elaborate_constraints()` produces `TxnSchema`, `FieldTypeSchema`, `EnumDomainSchema`, `RelationSchema`, … | None for v1. |
| 2 — Typed IR types | ⏳ scaffold only | `src/constraints.rs:196 ConstraintExpr` enum + variants for binop/unop/range/set/relation-call. `lower_constraint_expr_inner` at L1162 builds it. | **Untyped.** `IntLiteral(String)`, no `BV{width,sign}` on nodes, no field-type lookup on `FieldRef`, no enum-domain check on bare identifiers. The IR captures *shape*, not *types*. |
| 2 — Lowering | ⏳ partial | `lower_constraint_expr_inner` covers binop/unop/range/set/relation-call/foreach. | No type checking, no width inference, no diagnostics on width mismatch or signed-mixed-with-unsigned. |
| 3 — Solver backend | ❌ not extracted | Inline in `src/codegen/cpp_tb.rs:8051 emit_solver_block` and helpers. Builds Z3 input as C++ string literals at codegen time. | Z3 lowering owns its own width/sign decisions, duplicates AST traversal, and is not testable in isolation. |
| 4 — Runtime randomization | v0 | `src/codegen/cpp_tb.rs:583+` emits `harc_rng_*` SplitMix64 helpers; per-field auto-pref steering at L8711+; per-call `random_seed` derived from RNG at L8192. | No batching, no queue, no `blocking` vs queued split, ad-hoc per-attribute steering. `[unique]` history is per-test in static state; `[dist]` is rejection-sampled inline; `solve_order` is partially honored. |

Migration target: layers 2–3 land as a clean typed pipeline, layer 4
lands as a runtime library that the C++ codegen calls into rather than
inlines.

## Layered handoff

```
                    ┌───────────────────────────────────────┐
   parser ───────►  │  AST (Expr, randomize-with body)      │
                    └───────────────┬───────────────────────┘
                                    │
                                    ▼
   src/constraints.rs    ┌──────────────────────────────────┐
   (LAYER 1)             │  elaborate_constraints()         │
                         │      ↓ TxnSchema, FieldSchema    │
                         └──────────────┬───────────────────┘
                                        │
                                        ▼
   src/constraints/      ┌──────────────────────────────────┐
   typed_lower.rs        │  lower_constraint_typed()        │
   (LAYER 2)             │      ↓ CTypedExpr (typed)        │
                         │  verify_constraint_problem()     │
                         └──────────────┬───────────────────┘
                                        │      CTypedProblem
                                        │      (+ ConstraintRef
                                        │       for tb-ir use)
                                        ▼
   src/solver/           ┌──────────────────────────────────┐
   {z3.rs, mod.rs}       │  trait Solver                    │
   (LAYER 3)             │  Z3Backend: Solver               │
                         │      ↓ SmtProblem                │
                         │  solve(seed) -> Model            │
                         └──────────────┬───────────────────┘
                                        │
                                        ▼
   runtime/              ┌──────────────────────────────────┐
   harc_random_rt.{h,    │  harc_solve<T>(...)              │
   cpp}  (LAYER 4)       │  harc_queue_solve<T>(...)        │
                         │  harc_unique_pick<T>(...)        │
                         │  harc_dist_pick<T>(...)          │
                         └──────────────────────────────────┘
```

Each downward edge is a typed interface that can be unit-tested
without the layer below. Codegen reaches into layer 4 only.

## Layer 2 — Typed constraint IR

### Why the current `ConstraintExpr` is not enough

The existing enum (`src/constraints.rs:196`) is the AST shape, just
renamed. It carries no widths, no signedness, no enum-domain check
on bare identifiers, no resolved field types. Every consumer
(Z3 lowering, runtime, future GPU pre-solver) has to re-traverse the
field schema to recover types — duplicating work and creating drift.

The v1 IR carries **resolved type on every node**. The solver layer
consumes types only; it never re-reads the field schema.

### Core types

All types live in `src/constraints/typed.rs`. Source spans elided in
this sketch; every variant carries one.

```rust
pub struct CTypedProblem {
    pub problem_id: ConstraintProblemId,    // stable handle for runtime
    pub origin: ProblemOrigin,              // call site / record name
    pub env: FieldEnv,                      // field path → CType + non-random + attrs
    pub constraints: Vec<CTypedClause>,
    pub solve_order: Option<Vec<FieldPath>>,
}

pub struct CTypedClause {
    pub origin: ConstraintOrigin,           // existing enum from layer 1
    pub expr: CTypedExpr,                   // must be CType::Bool
    pub assertion_name: String,             // for Z3 named assertions / UNSAT diagnosis
}

pub struct CTypedExpr {
    pub kind: CExprKind,
    pub ty: CType,                          // resolved type — non-optional
    pub span: SourceSpan,
}

pub enum CExprKind {
    BvLit(u128, /* width */ u32, /* sign */ Sign),  // numeric literal, width inferred
    BoolLit(bool),
    EnumLit { domain: EnumDomainId, variant_idx: u32 },
    FieldRef(FieldPath),                              // resolves via FieldEnv
    Unary(CUnaryOp, Box<CTypedExpr>),
    Binary(CBinaryOp, Box<CTypedExpr>, Box<CTypedExpr>),
    InSet { expr: Box<CTypedExpr>, set: Box<CTypedExpr> },  // set has CType::Set(elem)
    InRange { expr: Box<CTypedExpr>, lo: Option<Box<CTypedExpr>>, hi: Option<Box<CTypedExpr>> },
    Set(Vec<CTypedExpr>),
    Range { lo: Option<Box<CTypedExpr>>, hi: Option<Box<CTypedExpr>> },
    FieldMethodCall { target: Box<CTypedExpr>, method: BuiltinMethod, args: Vec<CTypedExpr> },
    ForAll { var: LocalId, iter: Box<CTypedExpr>, body: Box<CTypedExpr> },
    // foreach lowers to ForAll once the iter range is known.
}

pub enum CType {
    BV { width: u32, sign: Sign },        // covers UInt<N>, SInt<N>, Bits<N>, Bit
    List { elem: Box<CType>, max_len: Option<usize> },
    Bool,
    Enum { domain: EnumDomainId },
    Range { elem: Box<CType> },           // value-typed range expression
    Set   { elem: Box<CType> },           // bag of values of homogeneous type
    Bottom,                               // type-error marker; not solver-visible
}

pub enum Sign { Unsigned, Signed }

pub enum CUnaryOp { Neg, LogicalNot, BitNot }

pub enum CBinaryOp {
    // Arithmetic on BV: BV(w, s) × BV(w, s) → BV(w, s).  Implicit width/sign
    // mismatch is a verify error; AST lowering inserts explicit Cast nodes
    // upstream.
    Add, Sub, Mul, Div, Mod,
    // Comparison on BV → Bool. signed-vs-unsigned is operator-dispatched.
    Eq, Ne, Lt, Le, Gt, Ge,
    // Logical on Bool × Bool → Bool.
    LogicalAnd, LogicalOr,
    // Bitwise on BV(w, s) × BV(w, s) → BV(w, s).
    BitAnd, BitOr, BitXor,
    // Shifts: BV(w, s) × BV(w', Unsigned) → BV(w, s). w' may differ.
    Shl, Shr,
}

pub enum BuiltinMethod {
    Len,           // list aggregate: items.len() → BV(unsigned)
    // … extension point for spec-defined record/aggregate methods
}

pub struct FieldEnv {
    pub fields: BTreeMap<FieldPath, FieldInfo>,
    pub enums:  BTreeMap<EnumDomainId, EnumDomainSchema>,
    pub relations: BTreeMap<RelationId, RelationSchema>,
}

pub struct FieldInfo {
    pub ty: CType,
    pub non_random: bool,
    pub has_default: bool,
    pub attrs: Vec<FieldAttrSchema>,        // [range], [dist], [unique], [within]
}
```

### Defining property

**Every `CTypedExpr` carries its resolved `CType`.** No node is
type-deferred. The solver layer dispatches operators on those types
without looking at any external schema. The runtime layer reads
`FieldInfo` for `[dist]`/`[unique]`/`solve_order` semantics only.

A `CType::Bottom` node may exist transiently during lowering, but
`verify_constraint_problem()` rejects any problem containing `Bottom`
before it reaches the solver — so the solver only ever sees
well-typed expressions.

### Field paths

```rust
pub struct FieldPath(pub Vec<Symbol>);   // ["hdr", "addr"] for hdr.addr
```

Same shape as today's `ConstraintFieldRef` but normalized to a single
canonical representation. Nested struct fields collapse to a single
path at elaboration; `when`-subtype-guarded fields carry an
implicit-guard suffix that the SMT lowering converts into an
`(=> guard ...)` wrapper.

### Width semantics

Widths follow IEEE 1800 (Verilog) rules adapted to checked HARC:

- Literal width inference: a bare `0x18` in `p.addr == 0x18` inherits
  `p.addr`'s width. A literal in an open context (e.g., `let l = 0x18`)
  takes the minimum width that fits, unsigned by default.
- Mixing widths in `Add`/`Sub`/`Mul`: lowering rejects with diagnostic
  `width mismatch: <w1> vs <w2> at <span>; insert an explicit cast`.
  Spec §11.6 width-widening is **not** automatic in constraints —
  hidden widening hides bugs.
- Mixing signedness: same, rejected with explicit-cast diagnostic.
- Comparison operators dispatch on operand signedness: signed
  operators if both signed, unsigned if both unsigned, otherwise
  rejected.

This is stricter than today's behavior. Migration includes a one-shot
sweep over the 56 fixtures to add explicit casts where the v0
emission silently coerced.

## Layer 2 — AST → IR lowering

`src/constraints/typed_lower.rs` exposes:

```rust
pub fn lower_problem(
    elab: &ConstraintElaboration,
    target: &TxnSchema,
    randomize_with_body: Option<&[Expr]>,    // None = bare randomize(t)
    site_span: SourceSpan,
) -> Result<CTypedProblem, Vec<LowerError>>;
```

The lowering merges, in order:

1. The target transaction's `keep` constraints (from `TxnSchema::keeps`).
2. Any `when`-subtype-guarded constraints reachable from the active
   discriminator path.
3. The `randomize-with` body, if any (each top-level expression in
   the body becomes one `CTypedClause`).
4. Field-attribute-derived constraints (`[range]`, `[within]`).

Relation calls are expanded at lower-time by substituting actual
arguments into the relation body and inlining the result into the
current problem. No `RelationApply` node is present in finished typed
IR. Recursive relations are an error.

### Per-AST-node lowering rules

| AST `Expr` | `CExprKind` | Notes |
|---|---|---|
| `IntLit(n)` | `BvLit(n, w, s)` | `w`, `s` inferred from operator/field context; if ambiguous, smallest unsigned BV that fits. |
| `BoolLit(b)` | `BoolLit(b)` | |
| `Ident("VARIANT")` (where `VARIANT` is an enum variant) | `EnumLit { domain, variant_idx }` | Domain resolved via `FieldEnv::enums`. |
| `Ident("FIELD")` (single name resolving to a field) | `FieldRef(FieldPath::single(FIELD))` | Bare names valid only inside transaction-level `keep`. |
| `target.field` | `FieldRef(FieldPath::of(target, field))` | `target` resolves to the randomize subject in `randomize-with`, or the enclosing transaction in `keep`. |
| `BinOp(+/-/*//%, a, b)` | `Binary(Add/Sub/…, a, b)` after type-check | Reject width/sign mismatch; require both BV. |
| `BinOp(==/!=/</…, a, b)` | `Binary(Eq/Ne/Lt/…, a, b)` | Dispatch signed/unsigned. Mixed → reject. Bool/Enum equality lowers identically. |
| `BinOp(&&/\|\|, a, b)` | `Binary(LogicalAnd/LogicalOr, a, b)` | Both operands must be Bool. |
| `BinOp(&/\|/^, a, b)` | `Binary(BitAnd/BitOr/BitXor, a, b)` | Both must be BV with matching width/sign. |
| `UnaryOp(!, a)` | `Unary(LogicalNot, a)` | a must be Bool. |
| `UnaryOp(~, a)` | `Unary(BitNot, a)` | a must be BV. |
| `expr in [a, b, c]` | `InSet { expr, set: Set([a,b,c]) }` | Set elem-type must match expr. |
| `expr in [lo .. hi]` | `InRange { expr, lo, hi }` | lo/hi must be BV matching expr. Open range allowed (`Option`). |
| `relation(args...)` | Expand to inlined body expressions | Recursive relations rejected. Block-form relations at top level contribute one clause per body expression; nested block-form calls collapse to an `&&` chain. |
| `target.method(args)` | `FieldMethodCall { target, method, args }` | v1 supports list `Len` only; everything else rejected. |
| `foreach (var in iter) <body>` | `ForAll { var, iter, body }` | Only valid as a top-level clause; v1 supports list, set, and range iterables. |

### Type-error diagnostics

Every type error produces a `LowerError` with span and a structured
description. Sample errors (full list in `src/constraints/typed_lower.rs`):

- `WidthMismatch { lhs_width, rhs_width, lhs_span, rhs_span, op }`
- `SignednessMismatch { lhs_sign, rhs_sign, op_span, op }`
- `BareIdentNotEnumOrField { name, span }`
- `RelationArityMismatch { relation, expected, actual, span }`
- `RecursiveRelation { relation, span }`
- `BuiltinMethodNotSupportedV1 { method, span }`
- `ForeachIterNotBounded { iter_span }`

These surface to the user before solver code is ever emitted, matching
the plan doc's "unsupported syntax should fail during typed lowering,
before C++ emission" goal.

### Verifier

`src/constraints/typed.rs::verify_constraint_problem(problem) -> Result<(), Vec<VerifyError>>`
enforces:

- Every `CTypedExpr` has a concrete `CType` (no `Bottom`).
- Every `Binary` operand pair has matching widths/signs per op rules.
- Every `FieldRef::FieldPath` resolves in `FieldEnv`.
- Every `EnumLit::variant_idx` is in range for its domain.
- Every clause's top-level expr is `CType::Bool`.
- Every `RelationApply` was expanded (none present in finished problem).
- `solve_order` references only fields in `env`.

Verifier runs after `lower_problem` in debug builds; in release, it
runs once at compile time on every elaborated problem.

## Layer 3 — Solver backend

### Trait surface

`src/solver/mod.rs`:

```rust
pub trait Solver {
    type Problem;
    type Model;

    fn build(&self, problem: &CTypedProblem) -> Result<Self::Problem, SolverBuildError>;
    fn check(&self, problem: &Self::Problem, seed: u64) -> SolverResult<Self::Model>;
    fn extract(&self, model: &Self::Model) -> SolvedTuple;
}

pub enum SolverResult<M> { Sat(M), Unsat, Unknown }

pub enum SolverBuildError {
    Verify(Vec<VerifyError>),
    Unsupported { feature: &'static str, detail: String },
}

pub struct SolvedTuple {
    pub fields: BTreeMap<FieldPath, FieldValue>,
}

pub enum FieldValue {
    BvBits(Vec<u64>),         // little-endian words; carries width via FieldEnv
    Bool(bool),
    EnumIdx(u32),
}
```

The trait is solver-agnostic. Z3 is the v1 implementation; future
backends (CVC5, on-device rejection sampler for GPU) can implement the
same surface. The build step always runs `verify_constraint_problem`
before solver-specific lowering, so malformed typed IR fails before a
backend can emit inconsistent SMT.

### Z3 backend

`src/solver/z3.rs::Z3Backend: Solver`. Lowering rules:

| `CExprKind` | Z3 SMT-LIB-ish lowering |
|---|---|
| `BvLit(n, w, _)` | `(_ bvN w)` |
| `BoolLit(b)` | `true` / `false` |
| `EnumLit { domain, idx }` | `(_ bvIDX W)` where `W = ceil_log2(\|domain\|)`. Plus an axiom on each enum-typed field: `bvult(field, |domain|)`. |
| `FieldRef(path)` | the declared Z3 variable for that field |
| `Binary(Add, a, b)` | `(bvadd a b)` |
| `Binary(Sub, a, b)` | `(bvsub a b)` |
| `Binary(Mul, a, b)` | `(bvmul a b)` |
| `Binary(Div, a, b)` | `(bvudiv a b)` if unsigned, `(bvsdiv a b)` if signed |
| `Binary(Mod, a, b)` | `(bvurem ...)` / `(bvsrem ...)` |
| `Binary(Lt, a, b)` | `(bvult ...)` / `(bvslt ...)` |
| `Binary(LogicalAnd, a, b)` | `(and a b)` |
| `Binary(BitAnd, a, b)` | `(bvand a b)` |
| `Binary(Shl, a, b)` | `(bvshl a b)` — `b`'s width must match `a`'s (zero-extend `b` at lowering if narrower) |
| `Unary(Neg, a)` | `(bvneg a)` |
| `Unary(BitNot, a)` | `(bvnot a)` |
| `Unary(LogicalNot, a)` | `(not a)` |
| `InRange { expr, lo, hi }` | `(and (bvule lo expr) (bvule expr hi))` for unsigned; signed picks `bvsle` |
| `InSet { expr, Set(es) }` | `(or (= expr e0) (= expr e1) ...)` |
| `FieldMethodCall { Len }` | the list aggregate's length field |
| `ForAll { var, iter, body }` | unrolled by solver lowering; iter must be statically bounded or list-bound |

### Named assertions and UNSAT origins

Every `CTypedClause` becomes a separately-named Z3 assertion:

```smt
(assert (! <expr> :named c_42))
```

Where `c_42` is `assertion_name`. On UNSAT, the solver returns the
unsat core, which maps assertion-name → origin. Diagnostics surface
as:

```
randomize(RegPair) at axilite_constraint_test.harc:42:9: UNSAT
  contributing constraints:
    - p.addr == 24    (RandomizeWith @ axilite_constraint_test.harc:43:13)
    - p.addr != 24    (TransactionKeep RegPair @ axilite_constraint_test.harc:11:5)
```

The plan doc's "UNSAT reporting should include the transaction type,
randomize site, active origins, and named solver assertions" is
satisfied entirely from `Solver::unsat_origins` plus the
`ConstraintOrigin` enum from layer 1.

### Soft preferences

Auto-coverage goals (the `_pref_<tag>_<n>` mechanism today at
`cpp_tb.rs:8711`) become solver-side soft assertions:

```smt
(assert-soft (= p.value <preferred-value>) :weight 1 :id pref_p_value)
```

The runtime decides which preferences to emit per call (driven by
seed-stable cycling over auto-goal targets). The solver layer exposes:

```rust
fn check_with_prefs(
    &self,
    problem: &Self::Problem,
    seed: u64,
    prefs: &[SoftPref],
) -> SolverResult<Self::Model>;
```

`SoftPref` carries field path + preferred value + weight + id, all
derived in the runtime layer.

## Layer 4 — Runtime randomization API

`runtime/harc_random_rt.h` + `runtime/harc_random_rt.cpp`. The
generated C++ from `cpp_tb.rs` calls into this API rather than
inlining Z3 string construction.

### Call shape

```cpp
// Blocking: solve now, return on success, abort test on UNSAT.
template <typename T>
void harc_solve(T& target, harc_problem_id pid, harc_seed sd);

// Queued (default for static-state-only constraints): may consume a
// pre-solved tuple from the queue; falls back to inline solve.
template <typename T>
void harc_solve_queued(T& target, harc_problem_id pid, harc_seed sd);

// Variant with explicit blocking marker — the parser already records
// the user's `blocking randomize(...)` form; codegen routes accordingly.
template <typename T>
void harc_solve_blocking(T& target, harc_problem_id pid, harc_seed sd);
```

`harc_problem_id` is a stable handle assigned at compile time per
`(TxnSchema, randomize-with site)` pair. The runtime maintains:

- A table `harc_problem_id → CTypedProblem` (serialized into emitted
  C++ as `extern const HarcProblem _harc_problem_<n>`)
- A solver instance keyed by problem-id (lazy-init per first solve)
- A pre-solve queue per problem-id

### Seed derivation

Every solve takes a 64-bit seed. The runtime derives it deterministically:

```cpp
harc_seed sd = harc_seed_from(
    /*global*/  harc_rng_state,
    /*site_id*/ HARC_CALL_SITE_ID_<n>,        // compile-time per site
    /*iter*/    harc_call_iter_<n>++          // compile-time per site counter
);
```

This is the v1 replacement for the v0 `harc_rng_next() & 0x7fffffff`
approach. Determinism contract: same `HARC_SEED` → same sequence of
solver inputs → same sequence of solved tuples (modulo solver version,
flagged separately).

### `[dist]`, `[unique]`, `solve_order`

These are runtime semantics. The IR carries them as `FieldAttrSchema`
on `FieldEnv::FieldInfo`; the runtime reads them at solve time.

**`[dist]`**: rejection-sample the field after the solver returns the
satisfying *space*, weighted by the user's bins. v1 emits the dist as
a `harc_dist_pick<W>(bins)` call after the model is extracted; if the
solver's answer doesn't lie in a non-zero-weight bin, the runtime
resamples within the satisfying space (bounded retry count; on
exhaustion, fall back to uniform within the satisfying space and warn).

**`[unique]`**: per-field history kept in `runtime/harc_random_rt`. On
each call, the previous-value set is added as a soft "preference for
distinct" — actually a *hard* assertion `(not (= field prev_1))`
repeated for each `prev_i`, capped at history size 128. On UNSAT (no
distinct value left), runtime clears the history for that field, logs
a warning, and retries.

**`solve_order(a, b, c)`**: the solver layer is asked to fix `a` first
(by check + extract), then push it as a constant and solve `b`, then
`c`. The IR carries this on `CTypedProblem::solve_order`. Z3 backend
expresses each fixed field as `(assert (= a <chosen>))` between sub-solves.

### Queued solve path

For problems whose constraints depend only on transaction state and
compile-time constants (the common case), the runtime pre-solves a
queue of `N` tuples between cycles (where `N` is tunable, default 16).
On `harc_solve_queued`, the call returns a queued tuple immediately;
the queue is refilled async (in v1, on the host thread between TB
schedule ticks).

For `blocking randomize`, the runtime always solves inline. The
compile-time dispatcher (in `cpp_tb.rs`, eventually in
`codegen/cuda_tb.rs`) picks the call based on the parsed marker plus
the typed dependency analysis from layer 2:

- v0 emits `harc_solve` always (ignoring `blocking`).
- v1 emits `harc_solve_queued` if `lower_problem` proves no
  runtime-state dependency, `harc_solve_blocking` otherwise.

Dependency analysis is a layer-2 pass:
`fn problem_depends_on_runtime(problem: &CTypedProblem, ctx: &SiteContext) -> bool`.

## Migration plan

Eight phases, mapped from the plan doc's migration order. Each phase
has a parity gate against the 56 fixtures.

| Phase | Status | Scope | Parity gate |
|---|---|---|---|
| 1 — Typed IR types | shipped | `src/constraints/typed.rs` with `CTypedExpr`, `CType`, `CTypedProblem`, `FieldEnv`, Display impls, and typed schema handles. | Self-tests only; no production codepath uses it yet. |
| 2 — AST → typed IR lowering | shipped | `src/constraints/typed_lower.rs`. Lowers the supported constraint subset alongside existing codegen, returning structured errors for deferred constructs. | `lower_problem` fixture sweep: no panics; clean lowerings are now verifier-checked. |
| 3 — Verifier | shipped | `verify_constraint_problem`. Runs over every clean fixture-sweep lowering and rejects malformed hand-built IR before any solver backend consumes it. | Self-tests on hand-built IR with intentional violations + fixture sweep verifies every cleanly lowered problem. |
| 4 — Solver backend trait + Z3 SMT scaffold | scaffold shipped | `src/solver/{mod.rs,z3.rs}` plus `src/solver/problem_table.rs`. Reads verified `CTypedProblem`s and renders an SMT-LIB/Z3-shaped problem with declarations, enum domain axioms, named assertions, and assertion-origin mapping. Rust-side Z3 execution is still deferred. | Unit tests cover unsigned/signed BV lowering, enum/range lowering, verifier handoff, unsupported aggregate declarations, and typed problem table extraction. `tests/typed_z3_fixture_sweep.rs` walks all fixtures and builds the Z3 scaffold for every clean table entry. |
| 5 — Runtime randomization library | scaffold started | `runtime/harc_random_rt.{h,cpp}` plus `src/solver/runtime.rs`. Defines the runtime solve API shell, deterministic seed derivation helper, stable typed-problem descriptor manifest, problem lookup helper, call-site counters, C++ problem table emission, per-`randomize` metadata touches, an unconstrained-record queued runtime handoff that delegates to the existing PRNG callback, runtime-owned Z3 seeding for inline solver paths, and runtime solve-status construction for UNSAT. `cpp_tb.rs` still owns inline Z3 solving for constraints. | Unit tests verify descriptor stability, compile the C++ runtime scaffold as C++20, assert generated C++ carries/touches runtime problem/call-site metadata, assert unconstrained randomize routes through the runtime shell, assert constrained inline Z3 paths use runtime-derived seeds, and assert UNSAT constructs runtime status while preserving diagnostics. Later parity gate runs all randomize-using fixtures end-to-end after codegen starts calling the runtime solver. |
| 6 — `[dist]` / `[unique]` / `solve_order` migration | not started | Move runtime semantics from cpp_tb.rs inline emit into harc_random_rt.cpp; codegen calls library functions. | Fixture diff: every `[dist]`-using test produces same distribution histogram over N seeds (Kolmogorov-Smirnov tolerance). |
| 7 — `when` subtype lowering | shipped | Layer-2 expansion of `when` guards into guarded implications for keeps/range attrs, including nested `when` and foreach keeps. | Typed-lowering tests verify guarded clauses; full runtime parity still uses existing `cpp_tb.rs` path until backend execution lands. |
| 8 — Delete v0 inline Z3 emission | not started | Remove `emit_solver_block` + helpers from `cpp_tb.rs`. | One release cycle clean on phase 4-7 work. |

Phases 1–3 can land in parallel; they don't affect production
behavior. Phase 4 is the long pole — the SAT-outcome parity gate is
the load-bearing safety net, equivalent to byte-identical for the
TB-IR refactor.

## Worked examples

### Example 1: simple cross-field constraint

Source — `tests/fixtures/axilite_constraint_test.harc`:

```harc
transaction RegPair
    addr  : uint<8>
    value : uint<32>
end transaction RegPair

// ...
randomize(p) with
    p.addr == 24
    p.value > 65536
    p.value < 2147483648
    (p.value & 3) == 0
end randomize
```

After lowering:

```text
CTypedProblem {
  problem_id: P0,
  origin: RandomizeWith @ axilite_constraint_test.harc:43,
  env: {
    p.addr  -> FieldInfo { ty: BV{8, Unsigned},  non_random: false, attrs: [] },
    p.value -> FieldInfo { ty: BV{32, Unsigned}, non_random: false, attrs: [] },
  },
  constraints: [
    CTypedClause {
      origin: RandomizeWith @ :43,
      expr: Binary(Eq, FieldRef(p.addr): BV{8,U}, BvLit(24, 8, U): BV{8,U}): Bool,
      assertion_name: "c_p_addr_eq_24",
    },
    CTypedClause {
      expr: Binary(Gt, FieldRef(p.value): BV{32,U}, BvLit(65536, 32, U)): Bool,
      assertion_name: "c_p_value_gt_65536",
    },
    CTypedClause {
      expr: Binary(Lt, FieldRef(p.value): BV{32,U}, BvLit(2147483648, 32, U)): Bool,
      assertion_name: "c_p_value_lt_2g",
    },
    CTypedClause {
      expr: Binary(Eq,
              Binary(BitAnd, FieldRef(p.value), BvLit(3, 32, U)): BV{32,U},
              BvLit(0, 32, U)): Bool,
      assertion_name: "c_p_value_align_4",
    },
  ],
  solve_order: None,
}
```

SMT lowering for clause `c_p_value_align_4`:

```smt
(assert (! (= (bvand p_value (_ bv3 32)) (_ bv0 32)) :named c_p_value_align_4))
```

### Example 2: enum domain + width-checked literal

Source:

```harc
transaction BurstReq
    kind : BurstKind     // enum BurstKind = WRAP | INCR | FIXED
    len  : uint<4>
end transaction BurstReq

randomize(b) with
    b.kind == INCR
    b.len < 8
end randomize
```

Lowering:

```text
env: {
  b.kind -> FieldInfo { ty: Enum{BurstKind}, ... },
  b.len  -> FieldInfo { ty: BV{4, Unsigned}, ... },
}
constraints: [
  Binary(Eq, FieldRef(b.kind): Enum{BurstKind}, EnumLit{BurstKind, 1}: Enum{BurstKind}): Bool
  Binary(Lt, FieldRef(b.len): BV{4,U}, BvLit(8, 4, U): BV{4,U}): Bool
]
```

`BvLit(8, 4, U)` — width 4, value 8 fits. Had the user written `b.len < 16`, lowering would error with `BvLitOutOfRange { width: 4, value: 16, span: ... }`.

### Example 3: `[unique]` + `solve_order`

Source:

```harc
transaction CmdReq
    op  : uint<4>  [unique]
    arg : uint<32>
end transaction CmdReq

randomize(c) with
    c.arg > 0
    solve_order(c.op, c.arg)
end randomize
```

Lowering:

```text
env: {
  c.op  -> FieldInfo { ty: BV{4, Unsigned}, attrs: [Unique] },
  c.arg -> FieldInfo { ty: BV{32, Unsigned}, attrs: [] },
}
constraints: [
  Binary(Gt, FieldRef(c.arg): BV{32,U}, BvLit(0, 32, U)): Bool
]
solve_order: Some([c.op, c.arg])
```

Runtime semantics:
1. Solver fixes `c.op` first. Before solve, runtime queries
   `harc_unique_history[c.op]` and emits hard `(not (= c.op prev_i))`
   assertions.
2. After SAT, extract `c.op`; push as constant in the solver, push value
   into history.
3. Re-check with `c.arg > 0`; extract `c.arg`.
4. Return tuple.

The runtime API call from the generated code:

```cpp
harc_solve_queued(c, /*pid=*/P_CMDREQ_0, /*seed=*/sd);
```

## Open questions

1. **`Bottom` propagation vs early-exit.** When type-checking
   encounters a width mismatch, should lowering produce a `Bottom`-typed
   node and continue (collecting more diagnostics) or stop at the first
   error? Stopping risks under-reporting; continuing risks cascade
   noise. Proposed: collect up to 5 diagnostics, then stop. Tunable.

2. **Aggregate length field representation.** `items : list<uint<8>>`
   now lowers as `CType::List { elem, max_len }`, and `items.len()` lowers
   to `FieldMethodCall { Len }` with an unsigned BV result. Later solver
   backends may choose whether to materialize that as a synthetic length
   field or a backend-native aggregate operation.

3. **Soft preferences and seed determinism.** Auto-coverage soft prefs
   today derive from `harc_rng_next()` at emission time — meaning the
   emitted C++ is *not* deterministic across runs. v1 pushes pref
   selection into the runtime (seed-derived), which is correct. But
   what about reproducing a v0 failure under v1? Accept seed-stable
   non-equivalence between v0 and v1; document loudly.

4. **Z3 model extraction for wide BV.** Field values > 64 bits today
   spill into the `sim_log_line("FAIL", ...)` path at `cpp_tb.rs:9008`
   (the "not a uint64 numeral" branch). v1 must handle wide BV models
   correctly — `FieldValue::BvBits(Vec<u64>)` is the typed
   representation, but the C++ side of layer 4 needs a wide-tuple
   delivery shape. Spec'd in `runtime/harc_random_rt.h` once concrete.

5. **Queue refill and TB scheduling.** Queued solve assumes "between
   cycles" is a host-thread-available moment. For the future GPU
   backend, "between cycles" doesn't exist — the kernel is running.
   The GPU codegen will need a coarser refill granularity (batch-end).
   Document the host-vs-device delta in `tb-ir-design.md` once the
   queue refill semantics land.

6. **Relation expansion blow-up.** Recursive relations are rejected,
   but mutually recursive ones aren't yet. Worth a verifier check.
   Also: deep non-recursive relations can inline to large bodies;
   probably fine for v1 but worth a size warning.

## Decision log

- 2026-05-20: After scoping TB-IR refactor, agreed to land constraint
  IR (this doc) first. The TB-IR's `Terminator::Randomize` carries a
  `ConstraintRef` handle into the IR specified here; the two refactors
  are independent and the constraint side can ship without any TB-IR
  changes.
- 2026-05-20: Decided to enforce strict width/signedness matching at
  lowering rather than auto-widening per IEEE 1800 §11.6. Auto-widen
  in constraints hides bugs and makes solver diagnostics confusing.
  Fixture sweep needed before phase 4 to insert explicit casts where
  v0 silently widened.
- 2026-05-20: Decided runtime layer 4 lives in
  `runtime/harc_random_rt.{h,cpp}` as a separate library, not as
  inline emission in `cpp_tb.rs`. Matches the
  `runtime/harc_thread_rt.h` pattern already established for the
  scheduler.
