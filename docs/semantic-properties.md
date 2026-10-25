# Compiler semantic properties

Run `cargo test --test semantic_properties -- --nocapture`. This target is
included in the normal CI Cargo test step. Python 3, Verilator and libz3 are
required; missing dependencies fail the target. Failures retain sources,
expected observations, SMT and native logs under `HARC_CONFORMANCE_OUT` (or the
system temporary directory). CI already uploads `target/conformance` on failure.

The expectations come from Python integers and stated language contracts,
without calling the HARC evaluator or runtime. Each native backend is checked
against that oracle independently; agreement between two backends is insufficient.
The actual parser, typed lowering, emitted C++/SMT, native compiler and Z3 run.

| Property | Executed domain |
| --- | --- |
| Boolean-origin bitwise expressions retain full 64-bit results | Every pair of false, true, complemented false and complemented true; AND, OR and XOR, including named intermediates |
| Dead constant branches do not evaluate division by zero | Both logical spellings of AND/OR and both ternary arm positions |
| Declared Boolean helper returns equal integer nonzeroness and are canonical 0/1 | Pure, expression-only DUT and full-CFG helper paths; nine signed 64-bit inputs including zero, noncanonical truthy values and both extremes; live 64-bit DUT-port reads in both backends and 128-bit reads in TBIR, at five successive values including high-bit-only values |
| Repeated unary negation has the expected parity and leaves its operand unchanged | Depths 1, 2 and 3, five signed values across the 32-bit boundary |
| Equivalent decimal/hexadecimal/binary negative literals have equal values | Six magnitudes through INT64_MAX, including 2^31, 2^32-1 and 2^32 |
| Signed conditional selection widens the selected arm from its declared sign bit | TBIR; five width pairs across 8/32/65/100/128 bits, both arm orders and both selections; a runtime division creates a masked narrow negative operand |
| A conditional with a wrapping arm retains unsigned common shape | TBIR; three wrapping operators, both arm orders and selections, checked by comparison with zero |
| Enum domains admit every member and exclude every spare bit-vector encoding | Cardinalities 1, 2, 3, 4, 5, 7, 8, 9, 15 and 16; 84 pinned SAT/UNSAT queries |
| Mixed-width shifts preserve count magnitude and arithmetic-right-shift sign | Signed/unsigned 8-bit values, 4/8/16-bit counts, both directions, counts at/above width and 256 when representable; correct result SAT and a contradictory result UNSAT |

This gives 139 native observations for v1, 201 for TBIR and 212 typed-SMT queries.
The signed wide and wrapping-conditional families run only under TBIR because
v1 has documented limitations in those domains. The new wide DUT-port return
check also exposed a v1 limitation: implicit `HarcWide` to `bool` conversion is
ambiguous between its integer conversion operators. That implicit wide-return
case runs in TBIR; v1 is not claimed to support it. Scalar port returns remain
mandatory in both backends. The v1 wide-return failure is retained as a known
limitation, not counted as a passing property.

The typed lowerer does not yet support the mixed Bool/BV equality forms used by
some native solver regressions; those remain explicitly recorded in the fixture
sweep. This suite does not silently claim that gap is proved or implemented.

The new Boolean-helper property exposed a missing lowering conversion: returning
a typed integer into a declared Boolean result slot violated the IR verifier,
while the literal-only regression had passed. Lowering now expresses conversion
as a nonzero comparison before assignment. The verifier remains strict.

These are bounded executable properties, not universal compiler proofs. The
[formal kernel contracts](formal-arithmetic.md) separately quantify all admitted
symbolic inputs. Algebraic identities provide a second specification style for
the bitwise and wrapping kernels; deliberate mutations verify that the properties
reject broken implementations, including Boolean recanonicalization and a skipped
RNG draw. Mathematical contracts still require review against language semantics.

The legacy implicit-wide-return limitation has a durable minimal reproducer:

```sh
cargo run --bin harc -- sim tests/conformance/known_gaps/wide_bool_return.harc \
  --sv tests/conformance/known_gaps/wide_bool_return.sv --top Top \
  --codegen v1 --outdir target/wide-bool-gap
```

v1 currently fails native compilation with ambiguous `HarcWide` to `bool`
conversion. Selecting `--codegen tbir` passes. This reproducer is not a negative
CI assertion that would require preserving the defect; after v1 supports this
conversion, move the wide-port observations into the shared native matrix.

The [expression and model properties](expression-model-properties.md) add
whole-tree interpretation and exhaustive small-domain model-set equivalence
through both native emitters and the actual typed SMT backend.
