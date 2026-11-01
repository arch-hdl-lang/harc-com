# Formal arithmetic contracts

Run the source-bound checks with:

```sh
python3 proofs/arithmetic/test_runner.py
python3 proofs/arithmetic/run.py
```

The default suite runs 30 CBMC obligations and 12 Kani harnesses. Inputs are
symbolic: a passing obligation covers every input admitted by its assumptions,
not a random sample. The separate `Formal arithmetic` workflow runs these
checks on pushes and pull requests. Missing tools, failed assertions, incomplete
unwinding, unrecognized success output and timeouts fail the run.

## Coverage and limits

| Production code | Contract | Domain |
| --- | --- | --- |
| `runtime/harc_thread_rt.h`: `harc_mask_u128`, `harc_trunc_u128`, `harc_negate_u128` | Low-bit mask, truncation, modular negation | Every 128-bit input; declared width 0..128 |
| `harc_slt_u128` | Signed order equals unsigned order after sign-bit bias | Every pair of 128-bit storage inputs; width 0..128 |
| `harc_shl_u128`, `harc_shr_u128`, `harc_ashr_u128` | Logical shifts and signed arithmetic right shift, including saturation | Every 128-bit storage input and 64-bit unsigned shift amount; width 0..128 |
| `harc_sext_u128` | Every result bit preserves source data, replicates its declared sign, or clears destination padding | Every 128-bit storage input; source/destination widths 0..128, including narrowing |
| `harc_u64_shift_count`, `harc_u128_shift_count` | Clamp the mathematical count to the limit without discarding high bits | Every 64/128-bit count and every 64-bit limit, including zero and UINT64_MAX |
| `harc_sdiv_u128`, `harc_smod_u128` | Native signed quotient/remainder; truncation toward zero; dividend-signed remainder; explicit zero-divisor and modular MIN/-1 policies | Complete equivalence at each width 1..8; zero-divisor policy separately covers every width 0..128 |
| `harc_word_add`, `harc_word_sub` | Addition/subtraction modulo storage capacity | All stored bits at word counts 1, 2, 4, 5, 8, 32 (32..1024 bits) |
| `harc_word_mask`, `harc_word_sext` | Each output bit preserves an input bit, extends the declared sign bit, or clears padding | Same six capacities; all source/destination widths from zero through capacity, including narrowing |
| `src/numeric_semantics.rs`: `fold_binary` (no longer called by the compiler: constant folding is now exact at ARCH result widths in `src/exact_int.rs`; re-targeting this harness is a follow-up) | Constant arithmetic values, comparison/Boolean result kinds, signedness, wrapping masks, shift validation, division/error dispatch | All 64-bit operands and metadata flags; wrapping widths 0..64, plus rejection of absent/out-of-range widths |
| `fold_binary`: algebraic cross-checks | Bitwise idempotence, self-XOR, absorption, De Morgan; wrapping addition/subtraction round-trip | Every 64-bit operand and metadata flag; every wrapping width 0..64 |
| `runtime/harc_random_rt.h`: `harc_rng_range` | Mathematical inclusive modulo mapping, output bounds, exactly one draw for ordered ranges and none for degenerate ranges | Full signed int64 domain and either half-domain with every uint64 draw; all signed 8-bit bounds with all uint8 draws; every int64 degenerate/reversed pair |
| `solver_binary_is_signed` | Declared-width promotion policy and operand symmetry (the mixed signed/unsigned arm is now unreachable from source: the compiler rejects a mix before the solver; see below) | All three operand classifications and widths 1..1024; separate symmetry/conservative-fallback checks for arbitrary optional `u32` widths |

**Zero-divisor and MIN/-1 policies are kernel-internal, not language
semantics.** HARC makes runtime `/` and `%` by zero, and signed `MIN / -1` /
`MIN % -1` at the declared width, FATAL errors at every width (spec §2).
Emitted code reaches `harc_sdiv_u128` / `harc_smod_u128` (and the `HarcWide`
twins and native `/` / `%`) only through the checked entry points
`harc_sdiv_i64`, `harc_smod_i64`, `harc_sdiv_u128_checked`,
`harc_smod_u128_checked`, `harc_wide_sdiv_checked`, `harc_wide_smod_checked`,
`harc_checked_div` and `harc_checked_mod`, which raise the fault before the
kernel runs. The `x / 0 = 0`, `x % 0 = x` and modular `MIN / -1` results
proved above are therefore unreachable from emitted code; the obligations that
pin them (`u128-divzero`, and the zero/overflow cases of the u128 signed
division equivalence) still describe the kernels but no longer the language,
and are due to be re-derived against the checked entry points. The checked
entry points, and the <= 64-bit shift helpers — `harc_shl_uw` /
`harc_shl_iw` / `harc_shr_uw` / `harc_ashr_iw` for an operand of exact
declared width `W < 64` (the result keeps `W` bits; a count >= `W`
saturates) and `harc_shl_u64` / `harc_shr_u64` / `harc_ashr_i64` for a
width-erased 64-bit value (a count >= 64 saturates) — are covered by the
executable host-compiler test `tests/runtime_arith_guards_cpp.rs`, not by a
proof.

Full C++ signed division/remainder equivalence at widths 32, 64 and 128 is
**unresolved**: exploratory SAT/SMT runs timed out. Attempt these additional
obligations explicitly; a timeout is a failing result:

```sh
python3 proofs/arithmetic/run.py --group u128 --extended-division --timeout 600
```

The Rust division harness proves selection of Rust's native 64-bit signed or
unsigned operators, correct result metadata, and rejection of zero/overflow.
It relies on the native operators' language semantics. It does not verify the
Rust compiler's implementation of division. Add/subtract/multiply and comparison
references use wider mathematical carriers; wrapping checks compare low-bit
residues. Error paths are proved separately from successful division/wrapping.

The solver contract preserves the current policy: two signed operands are
signed; two unsigned operands are unsigned; untyped literals take the other
operand's classification. For a mixed signed/unsigned pair, two widths below
32 select signed; otherwise the signed operand must have the greater declared
width. Since the R16 typing decision (spec §2.4), a constraint operator mixing
operands of declared, different signedness is a compile-time error, so that
mixed arm no longer receives source programs; the Kani contract still proves
the function as written and is due to be re-derived to the narrower domain.
This is a contract for the existing decision function, **not a proof
that every HARC expression or emitted C++ operation has matching semantics**.
AST classification, width inference, literal typing, casts, ternary evaluation,
SMT sort construction, and complete solver emission remain outside this proof.
Literal spelling, helper conversion and conditional widening are additionally
checked through bounded executable compiler properties, independently of these
kernel proofs. See [semantic properties](semantic-properties.md).

Wide multiplication/division, arbitrary word counts, converting constructors,
and coroutine scheduling are also outside this suite. Normal conformance and
native runtime tests still exercise the surrounding integration.

## Binding proofs to production

The runtime's public `HarcWide` wrappers call the word kernels proved here.
They pass the actual `std::array` storage, keeping constructor and container
behavior outside the sliced proof. Sign extension fills words before applying
the destination mask, avoiding a bit-by-bit loop.

`run.py` takes the marked function bodies verbatim from the current runtime
header; missing, reversed or duplicated markers fail extraction. It prepends
only fixed-width type aliases and appends the harness. It does not translate
or maintain a second implementation of the functions. The analysis assumes
32-bit `uint32_t`, 64-bit `uint64_t`, 128-bit unsigned carriers, distinct valid
output/input arrays for addition/subtraction, and arithmetic signed right shift
on the supported target. Array/pointer checks and unwinding assertions remain
enabled. A universally quantified bit index checks wide extension at every
output position. The loops are bounded by the selected storage capacity.

The scalar sign-extension and shift-count functions are extracted from their
unique production signatures through their unindented closing braces. Missing,
duplicate or unterminated functions fail extraction. These obligations check
the runtime functions, not whether every compiler emitter selects them or
passes the correct logical width; those integration contracts need separate
executable properties.

The RNG template is extracted verbatim from its unique signature through its
unindented closing brace. Missing/duplicate signatures or missing terminators
fail extraction; invalid extractions fail compilation. Its harness models the
RNG callback with an arbitrary value and a checked draw counter. A signed
128-bit mathematical reference represents cardinality 2^64 without overflow.
The report hashes both runtime headers and all harnesses.

The compiler and the standalone Kani harness import the same Rust numeric
module. Both active native solver renderers call its promotion function;
constant-fold AST traversal calls its binary evaluator. The `ast::BinaryOp`
re-export preserves existing callers. Frontend inference and diagnostics stay
with their original owners.

The runner writes logs, generated C++ units, commands, tool versions, source
SHA-256 hashes and statuses to `target/formal/`. It kills the invocation's
process group on timeout. CI uploads that evidence even when a check fails.
A result applies to those exact inputs and assumptions; it is not a
whole-repository absence-of-bugs claim or an independently checked proof
certificate. CBMC, Kani, their compilers and selected SAT/SMT solvers remain
part of the trusted toolchain.

## Toolchain and sensitivity checks

CI pins CBMC 6.11.0 (the Ubuntu package is checksum-verified), Kani 0.67.0,
and Z3 4.15.4 through the `z3-solver==4.15.4.0` wheel in an isolated Python
environment. Ubuntu 24.04's Z3 4.8.12 timed out on the 1024-bit extension
obligation at 300 seconds; the proof and timeout remain unchanged.
Kani supplies its own CBMC 6.8.0 and Rust nightly; the C++ runner uses the
separately installed CBMC. Z3 is required for the wide 1024-bit extension
obligation and selected successful Rust arithmetic paths. Error-path harnesses
use Kani's default SAT solver: with this toolchain, Z3 produced a spurious
counterexample for `0u64 << 64i64` returning the expected out-of-range error.
Native replay and the SAT proof rejected that counterexample. Do not override
all Kani solvers with `--solver z3` without checking this limitation.

On macOS, `brew install cbmc z3` installs the C++ tools. Install Kani using
`cargo install --locked kani-verifier --version 0.67.0`, followed by
`cargo kani setup`. A temporary installation works with `PATH` and `KANI_HOME`.
See the [CBMC documentation](https://diffblue.github.io/cbmc/) and
[Kani installation guide](https://model-checking.github.io/kani/install-guide.html).

`test_runner.py` checks fail-closed result classification and deliberately
injects ten defects into temporary copies: reversed signed comparison,
dropped word carry, missing sign extension, logical instead of arithmetic
constant right shift, Boolean bitwise recanonicalization, canonicalizing bitwise
AND, skipping the full-domain RNG draw, dropped scalar sign fill, leaked scalar
source padding, and discarded high shift-count bits. Each must produce a real assertion counterexample;
compilation failure or timeout does not count as defect detection.
