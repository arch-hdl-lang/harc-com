# Expression and solver model properties

Run `cargo test --test expression_model_properties -- --nocapture`.
The normal Cargo test suite includes this target. Python 3, Verilator, a native
C++ compiler and libz3 are required; missing dependencies fail the test.

An independent Python expression tree and interpreter generate expected values.
The oracle does not call the compiler AST, evaluator or solver. The suite checks
both legacy and TBIR emitters independently against those expectations.

| Property | Default executed domain |
| --- | --- |
| Whole expression trees match interpretation | 90 trees, four input environments, file constants and runtime locals, two naming/parenthesization variants: 1,440 observations per backend |
| Typed SMT admits exactly the mathematical model set | 38 signed/unsigned problems, every assignment at widths 2 and 3, selected families at width 4: 2,816 assignments |
| Native solvers admit exactly that same model set | The same 38 problems in each backend, every assignment plus values immediately outside each field domain: 4,120 queries per backend |
| Exact-set checker rejects both extra and missing models | Two mutations of actual emitted SMT: `<` changed to `<=`, and an extra restriction that removes valid models; 32 additional queries |

Trees compose casts, ordinary and wrapping arithmetic, division/remainder,
bitwise operations, shifts, comparisons, logical operations and conditional
selection. Explicit dead branches contain division by zero, exercising lazy
selection. Ordinary signed arithmetic is bounded to avoid undefined overflow;
shift counts are valid and left shifts use unsigned operands. Casts through
64 bits follow the language's carrier-preserving relabel semantics. This suite
does not claim coverage of arbitrary wide arithmetic or undefined inputs.

Model families cover equality, ordering, ordering partition, bitwise subset,
disjoint bits, zero quotient, divisibility and right-shift-to-zero. Division
excludes zero divisors and signed minimum divided by minus one; excluded
assignments are still queried and must be UNSAT. A separately written
mathematical predicate must agree with interpretation at both the declared
bit-vector width and the native 64-bit carrier width before source is emitted.
The chosen predicates have the same model set in both domains.

Every native assignment is pinned by a blocking constraint using runtime loop
locals. SAT results must preserve the full normalized 64-bit values of those
pins. Missing queries, duplicates, unknown solver results, wrong field values,
and either extra or missing models fail the test. Out-of-domain pins must be
UNSAT. Native simulation deliberately reports excluded assignments as UNSAT;
the harness requires all query markers and exact sets rather than treating a
nonzero simulation exit status alone as evidence of success. The normal runtime
footer must report exactly the number of excluded assignments.

This property exposed a TBIR bug: solver snippets had no lexical scalar binding
metadata, so a blocking constraint referring to a loop local failed with
`constraint references unknown name`. Lowering now records visible scalar
LocalIds and the renderer binds them through backend-assigned names. Focused
regressions cover shadowed locals, reserved C++ names, the helper scope fence,
consistent file-constant precedence in constraint value/signedness lookup, and
Boolean captures. Three additional native TBIR queries check mixed-rank
signed/unsigned comparisons and Boolean constraints. Runtime captures wider
than 64 bits fail with a diagnostic; they are not silently truncated.

The default tree seed is `0x45585052`. Override it with
`HARC_EXPRESSION_SEED=123 cargo test --test expression_model_properties -- --nocapture`.
Native solver seeds are fixed at 1 and learning is disabled. Failures retain
source, serialized trees, expected values, emitted SMT, model sets and native
logs under `HARC_CONFORMANCE_OUT` or the system temporary directory. CI's
existing conformance artifact upload retains that directory when configured.
Successful runs remove their scratch directory.

These are sampled expression trees and exhaustive model sets within explicit
small domains. Passing them does not establish that the compiler has no bugs.
See also [semantic properties](semantic-properties.md) and
[formal arithmetic contracts](formal-arithmetic.md).
