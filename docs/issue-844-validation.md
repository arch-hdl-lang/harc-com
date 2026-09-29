# Issue #844 implementation and validation

Base: `822006de`. Branch: `codex/844-range-preferences`.

## Changes

- Added deterministic rejection sampling for inclusive unsigned and signed
  64-bit literal ranges, including full domains and signed extremes.
- Selected range preferences in the shared emitter after distribution and
  enum preferences, with field-domain intersection and conservative fallback
  when literal conversion would disagree with the hard constraints.
- Replaced inference of permanent coverage `BLOCKED` status from preference
  failure with temporary deferral. Failed goals yield to other goals; an
  exhausted sweep permits an unsteered call and retries deferred goals on the
  next call. No extra solver queries are introduced.
- Added executed runtime and both-backend regressions, two registered HARC
  fixtures, and seed/coverage behavior documentation.

## TDD evidence

The executed sampler/codegen regressions failed before implementation.
The finalized diversity fixture also fails on the unchanged base under both
v1 and TB-IR for seeds 1, 2, and 424242: all six runs report collapsed range
values and falsely blocked coverage goals. The same seed matrix passes after
implementation, with complete range coverage. Repeating seed 1 replays the
same randomize trace, different seeds produce different traces, and both
backends agree.

The deferred-selection regression was added and observed failing before its
runtime helpers were implemented. It exercises failed cross and point goals
across groups, sweep exhaustion, retry eligibility, and absence of false
blocked flags. The independent reviewer additionally executed the final
runtime regression under UBSan.

The boundary fixture executes 100/200-bit unsigned carriers, high u64 bounds,
INT64_MIN, signed 63-bit fields, unique-history exhaustion and recycling, and
changing hard constraints. It runs under both emitters and all three seeds.

## Review

Independent reviewer: `/root/review_844`.

Two findings were addressed:

1. Simply removing BLOCKED inference can starve later goals. Temporary
   deferral and sweep reset prevent that starvation.
2. Wide hard-constraint literal emission currently sign-extends scalar
   literals through an int64_t carrier. The range optimizer declines these
   cases when a literal magnitude exceeds INT64_MAX and solver width exceeds
   64, rather than sampling a different interval.

The final independent review reported no outstanding findings or blocking
questions. It checked the complete implementation and tests, independently
ran the UBSan runtime regression, and confirmed the wide-literal issue with
an executed Z3 probe.

## Test environment and results

Local macOS toolchain: Rust 1.94.0, Apple clang 21.0.0, Verilator 5.048,
Z3 4.15.4. The CI-pinned Verilator 5.034/Linux toolchain was not used.

- Focused regression suite: 5 passed, including generated simulation on both
  backends, replay/diversity, adjacent policies, boundary/fallback emission,
  and runtime sampling with UBSan.
- `harc check` on both new fixtures: passed.
- Fixture registration: passed.
- Emission parity: 198 acceptance-parity cases, 0 divergent, 2 known exemptions,
  0 skipped, 0 lost; 19 cases also compared solver text.
- Simulation fixture gate: 200 passed, 0 failed. The boundary fixture was
  rerun successfully after its comparison field was made width-consistent.
- Staged typed-Z3 sweep: 4 passed, including a guard test for the narrowly
  classified INT64_MIN magnitude limitation.
- Final full Cargo run (`HARC_REQUIRE_VERILATOR=1`, `--no-fail-fast`):
  1306 passed, 1 failed, 0 ignored across 31 reported test targets. The sole
  failure is the independently reproduced base failure described below.
- Backend equivalence: 267 passed, 0 failed, including both new fixtures.

The full Cargo run encounters
`differential::a_field_default_too_wide_for_its_u64_slot_is_never_truncated`:
Apple clang rejects an oversized integer literal which the test expects to
compile. The identical failure was reproduced in a clean detached worktree
at the unchanged base with a separate Cargo target directory.

The new boundary fixture also exposed an existing limitation in the staged
(not simulator) typed-Z3 lowerer: it rejects the positive magnitude of
INT64_MIN before unary negation. The sweep explicitly classifies only that
fixture/transaction's sole diagnostic, exact width/value and source span.
Negative guard tests ensure other diagnostics remain failures; executed
simulation still validates the actual extreme on both backends.

Commands used for the final gates:

```sh
HARC_REQUIRE_VERILATOR=1 HARC_NO_LEARN=1 cargo test --release --no-fail-fast
HARC_NO_LEARN=1 JOBS=6 tests/run_fixtures.sh
HARC_NO_LEARN=1 JOBS=4 tests/run_tbir_equiv.sh
JOBS=2 tests/run_emit_parity.sh
tests/check_fixture_registration.sh
```

## Scope and remaining limitations

This fixes preferences for supported literal intervals; it does not provide
uniform sampling of arbitrary constrained solution spaces. Distribution and
enum precedence are unchanged. Named/dynamic bounds, signed fields wider than
64 bits, intervals wider than u64, and the incompatible wide-solver literal
cases retain the existing preference behavior. Existing seeds change for
range fields and for coverage retry sequences.

Coverage reports conservatively leave unproven goals unblocked, including
truly unreachable goals; repeated retry cost remains possible. Z3 UNKNOWN
injection was not added: the preference retry branch handles all non-SAT
results through deferral and never declares those goals blocked. General
constraint fallback diversity, PRNG-path range overflow/bias, and wide hard
literal semantics remain separate work.
