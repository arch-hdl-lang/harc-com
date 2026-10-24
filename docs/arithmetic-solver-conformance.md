# Arithmetic and solver conformance

Run the deterministic conformance target with:

```sh
cargo test --test arithmetic_solver_conformance -- --nocapture
```

It runs automatically in the ordinary Cargo suite, including CI's release
build. Python 3.10+, a C++ compiler, Verilator and libz3 are required. Missing
dependencies are failures, never successful skips. No Python packages are
needed. For a custom Z3 installation set `HARC_CONFORMANCE_Z3_LIB` to its
shared library.

## Independent oracle and checks

`tests/conformance/arithmetic_solver.py` calculates expectations with Python's
arbitrary-precision integers. It does not import a HARC evaluator, runtime
helper, or solver to compute expected values. Signed division uses absolute
integer division and restores the sign, truncating toward zero. Remainder is
`a - quotient(a,b)*b`; Python's `%` is deliberately not used for signed
remainder. Explicit wrapping operations mask to the declared operand width.

The Cargo harness generates a corpus, exports its typed constraint problems
through the actual Rust SMT emitter, and executes the SMT using the installed
Z3 C API. It then compiles generated HARC programs through **each native
emitter**, using a small inert SystemVerilog DUT. This tests emitted arithmetic
and active native randomization, not merely the runtime helpers in isolation.
Agreement between emitters alone is insufficient: every observed value must
match the independent oracle.

The normal seed produces 5,967 TBIR arithmetic comparisons, 4,720 v1 arithmetic
comparisons, 256 SAT/UNSAT queries per native emitter, and 208 typed SMT queries.
The normal corpus includes:

- Every input pair for signed and unsigned 2- and 3-bit arithmetic.
- Boundary and fixed-seed values at widths 8, 31, 32, 63, 64, 65, 100, 127,
  128, 129 and 256, including sign-bit and carrier transitions.
- In-range `+`, `-`, `*`; bitwise operators; six comparisons; nonzero division
  and remainder; shifts; and explicit `+%`, `-%`, `*%` through 64 bits.
- Mixed signed 65/100/128-bit arithmetic in both operand orders. Inputs include
  a narrower negative quotient, whose masked representation must sign-extend.
- Same-type signed/unsigned solver division and remainder at widths 3, 8, 32
  and 64. Each correct pinned constraint has a separate contradictory query.
- Native mixed signed/unsigned 64-bit division, remainder and ordered
  comparisons, including negative operands converted to unsigned at equal rank.

For SAT, the runner validates the returned pinned fields and native expression
value. For UNSAT, **every individual query** must emit its UNSAT diagnostic
between its own begin/completion markers. Other failing queries cannot hide a
false SAT. The process must also finish with nonzero status; a crash, timeout,
missing output, unknown result, duplicate observation or unexpected observation
fails the target. UNSAT paths never evaluate division on zero/default fields.

The oracle has known-answer and quotient/remainder identity checks. The output
checker is also tested against injected wrong values, missing results,
duplicates and unexpected IDs. Generated operands are shared within a width
batch; native binaries are built once and reused across test selections.

## Explicit coverage boundaries

This is a bounded suite, not a proof of compiler correctness. Native TBIR
arithmetic covers all listed widths. Native v1 covers arithmetic through 64 bits
and ordered/equality comparisons through 128 bits. Its other wide operations
are outside this target's coverage. Mixed native comparisons use direct
predicates or their negation, rather than equality with a named Boolean constant. The typed SMT checks cover same-type
division/remainder; the mixed-rank checks exercise the active native solvers.
Existing targeted tests still cover contextual literals, constants and nested
mixed-width operands; this suite complements them.

Ordinary arithmetic cases that overflow their declared result range are
excluded, as are zero divisors and signed minimum divided by -1. Explicit wraps
exercise overflow without relying on native signed-overflow undefined behavior.
Shift counts are within the declared width. Wide signed inputs are assembled
from small limbs where wide casts are unsupported. Negative native inputs use
canonical decimal literals: legacy v1's C++ hexadecimal-literal rank behavior
is a separate input-typing limitation, not an arithmetic oracle.

## Reproduce and extend

The default corpus seed is `0x48415243`. To run additional reproducible samples:

```sh
HARC_CONFORMANCE_SEED=0x1234 HARC_CONFORMANCE_EXTENDED=1 \
  cargo test --test arithmetic_solver_conformance -- --nocapture
```

A failure prints its artifact directory. Successful runs remove their scratch
directory; failing runs retain `cases.json` (full concrete inputs and expected
values), generated `.harc` programs, exported SMT, native build/run logs and,
when a failing case is identified, `failure.json` and `repro.harc`. Keep the
printed seed and concrete case ID when reporting a failure. The corpus seed is
separate from the native solver seed, which stays fixed at 1.

Set `HARC_CONFORMANCE_OUT` to choose the parent artifact directory. CI uses
`target/conformance` and uploads failure artifacts for seven days, excluding
native object directories and core dumps. Native builds and executions have
bounded timeouts; timed-out native subprocess groups are terminated. Each SMT
query has a ten-second Z3 timeout, and `unknown` is a failure.

For direct native replay with retained sources and the same compiler:

```sh
python3 tests/conformance/arithmetic_solver.py --phase native \
  --harc target/debug/harc --out /tmp/harc-conformance-replay --seed 0x48415243
```

The `typed` phase requires the `.smt2` files exported by the Cargo harness; the
script alone does not substitute a handwritten SMT translation for the compiler.

## Fault-detection checks at introduction

Deliberately restoring the active emitter's old signed `%` dispatch and changing
exported SMT from `bvsrem` to `bvsmod` each failed at `s0034`: signed 3-bit
`-4 % 3` must be `-1`, not `2`. Bypassing signed wide operand normalization
failed at `a05919`: a masked signed65 quotient `-2` added to signed100 zero
must remain `-2`. Its saved minimal HARC reproducer also failed independently.
All temporary mutations were removed before the final validation. These are
specific sensitivity checks, not a claim of complete mutation coverage.

For a shorter native debugging replay, the standalone script accepts
`--only solver --backend tbir` (or `--only arithmetic --backend v1`). The normal
Cargo target always runs both suites and both backends.
