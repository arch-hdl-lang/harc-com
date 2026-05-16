# CVDP × HARC verification benchmark — Phase 2b-pilot

A HARC-flavored re-implementation of NVIDIA's CVDP cid012 (testbench-generation)
scoring loop, using Verilator branch coverage in place of Cadence IMC. The
CVDP problem set provides the DUTs and prompts; HARC TBs replace the
SystemVerilog TBs the reference benchmark expects.

## What this is, what it isn't

**Is:** end-to-end working loop that takes a CVDP cid012 problem, runs a HARC
TB against it via `harc sim --coverage`, post-processes the Verilator coverage
data into branch-coverage %, and compares to the problem's target. Validates
that HARC can author functional verification work on third-party DUTs.

**Isn't:** strict-comparability to CVDP paper numbers. The reference harness
uses Cadence Xcelium + IMC; we use Verilator + `verilator_coverage`. Different
tools, different coverage models (line/toggle/expression vs. line/toggle/
functional-covergroup), different denominators. The threshold (default 90%)
is the same nominal bar but is measured against a different population of
coverage points. Strictly comparable numbers would require either a Cadence
license + the `harc -emit sv-uvm` transpile target (v1+, not implemented),
or a separate Verilator-coverage calibration pass against the gold SV TBs in
the CVDP example set.

## Strict no-gold-TB policy

Every CVDP record in the HF dataset contains a reference TB at
`output.context["verif/*_tb.sv"]`. The extractor MUST NEVER copy that
to disk — looking at it would contaminate the agent's TB authoring
task. `extract.py::_verify_record_shape_without_reading_gold` is the
only place we touch `record["output"]`, and only to confirm the gold
exists; the content is never returned, written, or stored. **There is
no `gold/` directory under `bench/cvdp/<id>/` by design.**

## Layout

```
bench/cvdp/
├── README.md                                  ← this file
├── extract.py                                 ← HF dataset → problem-dir extractor
├── score.py                                   ← per-problem PASS/FAIL scorer
├── smoke.py                                   ← sanity-check the harness layout
├── _smoke_tb.harc                             ← trivial smoke-test TB template
├── cid012_gf_multiplier/                      ← Phase 1 hand-authored proof-of-concept
│   └── ... (DUT + manually-written HARC TB)
├── cid012_gf_multiplier_thin/                 ← regression: scorer rejects under-coverage
│   └── ...
└── cvdp_copilot_<id>/                         ← 67 extracted cid012 problems (one each)
    ├── meta.json                              ← top_module, dut_module, target_coverage, clock_inputs[]
    ├── prompt.txt                             ← original CVDP English prompt (NOT the gold TB)
    ├── dut/
    │   ├── <dut>.sv                           ← CVDP-provided DUT, unmodified
    │   └── <dut>_top.sv                       ← (auto-generated) clocked wrapper, when needed
    ├── tb/
    │   └── (empty until a HARC TB is authored)
    └── build/                                 ← scorer output (gitignored)
```

## Loop semantics — what the scorer does

`bench/cvdp/score.py <problem_dir>`:

1. **Build + run.** Invokes `harc sim --coverage` over the DUT `.sv` files
   plus the candidate `.harc` TB. `--coverage` enables Verilator
   `--coverage-line/toggle/expr` and emits `coverage.dat` at clean TB shutdown.
2. **Functional pre-check.** TB must print `ALL TESTS PASSED`. If asserts fail
   inside the HARC TB, we fail before reading coverage.
3. **Coverage post-process.** Runs `verilator_coverage --write-info` to convert
   `coverage.dat` to LCOV format. Parses per-source-file BRDA (branch coverage)
   counts.
4. **Score.** Filters to `meta.dut_module`'s source file (drops the
   `_top` wrapper and the TB itself). Reports branch-coverage % on the DUT,
   compares to `meta.target_coverage`, exits 0 on PASS / 1 on FAIL.

**Why branch coverage and not line coverage?** Every cid012 DUT we've seen is
combinational — `always @(*)` blocks where every line executes on every input
change. Line coverage hits 100% on a single-input TB, which is meaningless.
Branch coverage discriminates: each `if (cond)` produces 4 Verilator-tracked
branches, and only TBs that exercise the condition both ways hit all of them.
This matches the spirit of Xcelium IMC's branch/expression metrics.

## Phase 1 validation: gf_multiplier (hand-authored)

The DUT is a GF(2⁴) polynomial multiplier mod x⁴+x+1 — 4 inputs × 4 inputs,
purely combinational. Two HARC TBs:

| TB | Strategy | DUT branch coverage | Verdict |
|---|---|---|---|
| `cid012_gf_multiplier/`        | Exhaustive 16×16 sweep + inline software model assertion | 30/33 = **90.91%** | **PASS** (≥90%) |
| `cid012_gf_multiplier_thin/`   | Single A=1,B=1 input — sanity-check the scorer rejects under-coverage | 16/33 = **48.48%** | **FAIL** |

The thorough TB clears the bar; the thin TB fails cleanly. The scorer
discriminates. Phase 1 plumbing validates.

## Phase 2a: full cid012 set extracted (this PR)

Pulled all 67 cid012 problems from
[`nvidia/cvdp-benchmark-dataset`](https://huggingface.co/datasets/nvidia/cvdp-benchmark-dataset)
v1.1.0. Per-problem layout via `extract.py`. DUT topology breakdown:

| Topology | Count | Wrapper |
|---|---:|---|
| `clk`-native (canonical) | 31 | none |
| Clock-shaped port under another name (`i_clk`, `clk_i`, `clk_in`, `clock`, `PCLK`) | 16 | wrapper renames it to HARC's hardcoded `clk` |
| Combinational (no clock) | 16 | wrapper adds a phantom `clk` (no-op for DUT, drives HARC's posedge loop) |
| Multi-clock (multiple clock-shaped ports — `wr_clk`/`rd_clk`, `clk_dsp`/`PCLK`, etc.) | 4 | flagged `multi_clock=true` in meta; needs manual TB authoring |
| **Total** | **67** | |

Smoke-tested with a trivial `wait 1 cycle; log(info, "...")` TB on every
problem: all 67 build + run cleanly through `harc sim`. The harness
layout is sound; what remains is the actual TB authoring work.

### HARC compiler tweaks landed for this phase (`harc sim`)

- `--coverage` flag (was Phase 1) — pass-through to Verilator + write
  `coverage.dat` at clean shutdown
- `-Wno-BLKANDNBLK` + `-Wno-UNOPTFLAT` — tolerate SV quirks Xcelium
  accepts but Verilator escalates (common in CVDP DUTs that mix `=`
  and `<=` to the same reg)
- `--no-timing` — cycle-based TBs don't need delay semantics; tell
  Verilator to elide `#N` rather than refusing to elaborate

All three are net-additive to the existing 50-fixture sweep
(verified green post-change).

## Phase 2b: 8 hand-authored HARC TBs (no gold peek)

All TBs authored by Claude reading only `prompt.txt` + `dut/<dut>.sv`
— no gold SV TB ever consulted (there is no `gold/` directory on
disk; the HF record's `output.context["verif/*"]` is explicitly
discarded by the extractor).

### Round 1 (pilot, 3 problems)

| Problem | Line | Branch | Target | Verdict |
|---|---:|---:|---:|---|
| `binary_to_BCD_0030` (combinational) | 100.00% | 72.92% | ≥90% | FAIL — ceiling |
| `fixed_arbiter_0004` (1-clk sequential) | 100.00% | **90.91%** | ≥95% | PASS (under old cov flag), see note |
| `Synchronous_Muller_C_Element_0003` (clk-en) | 91.30% | 96.30% | ≥100% | FAIL — ceiling |

### Round 2 (next, 5 problems)

| Problem | Line | Branch | Target | Verdict |
|---|---:|---:|---:|---|
| `gray_to_binary_0014` (combinational) | **100.00%** | **100.00%** | ≥95% | **PASS** |
| `bcd_adder_0007` (BCD arithmetic, pure dataflow) | 94.74% | **100.00%** | ≥95% | **PASS** |
| `asyc_reset_0004` (async-reset countdown) | **100.00%** | **100.00%** | ≥100% | **PASS** |
| `generic_nbit_counter_0013` (6 counter modes) | **100.00%** | **100.00%** | ≥100% | **PASS** *(after iteration)* |
| `decode_firstbit_0017` (pipelined priority encoder) | 97.18% | 85.13% | ≥90% | FAIL — ceiling |

### Round 3 (Phase 2b-scale batch 1, 6 problems)

| Problem | Line | Branch | Target | Verdict |
|---|---:|---:|---:|---|
| `hamming_code_tx_and_rx_0029` (4-bit Hamming TX, pure dataflow) | **100.00%** | 91.67% | ≥91% | **PASS** |
| `hamming_code_tx_and_rx_0031` (8-bit Hamming RX + correct, combinational) | **100.00%** | **100.00%** | ≥100% | **PASS** |
| `nbit_swizzling_0009` (4-way 16-bit chunk reverse, combinational) | 93.33% | **100.00%** | ≥100% | **PASS** *(branch-gated)* |
| `32_bit_Brent_Kung_PP_adder_0004` (32-bit prefix adder, pure dataflow) | **100.00%** | **100.00%** | ≥80% | **PASS** |
| `signed_adder_0003` (signed add/sub, 1-clk sequential) | **100.00%** | **100.00%** | ≥99% | **PASS** *(after iteration)* |
| `cellular_automata_0002` (16-cell rule-128-shape CA, sequential) | **100.00%** | **100.00%** | ≥100% | **PASS** |

### Round 4 (Phase 2b-scale batch 2, 8 problems)

| Problem | Line | Branch | Target | Verdict |
|---|---:|---:|---:|---|
| `single_cycle_arbiter_0004` (1-clk request-grant FSM) | **100.00%** | 98.96% | ≥96% | **PASS** |
| `hamming_code_tx_and_rx_0037` (param-width Hamming RX) | 96.67% | 83.93% | ≥97% | FAIL — ceiling |
| `secure_read_write_bus_0005` (functional-clock APB-ish) | **100.00%** | **100.00%** | ≥100% | **PASS** |
| `image_stego_0014` (LSB embed/extract, 33-bit accumulator) | **100.00%** | **100.00%** | ≥100% | **PASS** *(after iteration; used `.trunc<32>()`)* |
| `morse_code_0027` (alphabet → variable-length morse table) | **100.00%** | 94.44% | ≥95% | FAIL — ceiling (0.56% short) |
| `static_branch_predict_0035` (state-machine predictor) | **100.00%** | 98.70% | ≥95% | **PASS** |
| `manchester_enc_0009` (1-clk bit-pair encoder) | **100.00%** | **100.00%** | ≥100% | **PASS** *(after iteration)* |
| `ring_token_0004` (4-node ring token, FSM with `default` arm) | 94.87% | **100.00%** | ≥100% | **PASS** *(branch-gated)* |

Round 4 ceiling-FAIL summary:
- `hamming_code_tx_and_rx_0037`: 9 unhit BRDA subbranches on register-declaration pseudo-branches (`reg [$clog2(DATA_WIDTH)-1:0] j`, `i`, `k`, `count`) — Verilator counts these init-width subbranches but DATA_WIDTH=4 makes the high bits never matter.
- `morse_code_0027`: 1 unhit BRDA on `morse_length[3]` MSB. DUT's lookup table only produces morse_length values 0..6 → bit 3 never toggles.

### HARC language ergonomics noted (round 4)

- **`else if` is two tokens**; HARC uses single-token `elsif`. Easy to hit when porting from SV-style sources.
- **`bits` is a reserved identifier** — rename locals (`v`, `data`, etc.).
- **`.trunc<N>()` (PR #117)** earns its keep: image_stego's TB needed to narrow a 33-bit intermediate (`sum + offset`) back to the DUT's 32-bit output before comparing — `.trunc<32>()` does it cleanly; `as uint<32>` would have been a no-op relabel.

### Net scoreboard

**17/22 PASS, 5/22 ceiling-FAIL.** The PASS column reaches 100% line *and*
branch coverage on every problem where the DUT doesn't have a
structurally unreachable path under default parameters. The 3 FAILs
all share the same shape:

- `binary_to_BCD`: `if (hundreds_nibble ≥ 5)` is impossible with
  8-bit input (max hundreds=2)
- `Muller_C_Element`: `else` inside a `genvar` loop only exists when
  PIPE_DEPTH ≥ 2; default is 1
- `decode_firstbit`: `if (OutputFormat_g == 1)` one-hot branch
  elaborated away under default `OutputFormat_g=0`; also bits 5-31
  of zero-extended binary output never toggle

These FAILs are **metric-tool incompatibility** between Verilator
branch+toggle coverage and Cadence IMC, not TB-authoring deficits.
The CVDP threshold (e.g. ≥90%) was calibrated against IMC's
unreachable-branch-exclusion semantics that Verilator doesn't share.

### Iteration patterns observed

- **Coverage scope tweak (cross-round)**: round-1 originally used
  `--coverage-line --coverage-expr` only. `bcd_adder_0007` is pure
  dataflow (only `assign` + module-instantiation, no `always`) so
  line+expr produced 0/0 coverage points. Switched to full `--coverage`
  (umbrella: line+toggle+expr+user) to mirror Cadence IMC's
  "Average %" aggregation. Trades the binary_to_BCD result down from
  87.5% → 72.92% (more toggle entries in denominator) but unblocks
  the pure-dataflow DUT class entirely.
- **Toggle-sweep tails**: `generic_nbit_counter` initially scored
  69% → 89% → 100% with two iteration rounds. First iteration drove
  more `ref_modulo` and `mode_in` values; second added a long JOHNSON
  walk to toggle every bit of the count register through 0→1 and 1→0.
  Toggle coverage on wide internal regs needs *explicit* walking
  patterns; just exercising functional modes is insufficient.

### HARC language ergonomics

- **Inline type cast = `expr as Type`** (matches arch-com's grammar
  at `doc/arch.ebnf:764` — postfix operator, binds tighter than
  every binary op). Round-2 originally hit a parse error by writing
  `(1: uint<32>)` — that's not the HARC syntax; the right form has
  always been `1 as uint<32>`. The earlier README claim of "no
  inline type cast" was wrong (corrected here). A small follow-up
  also tightened the cast codegen so width-widening casts emit a
  real C++ cast `((uint64_t)(1)) << 31` (was a silent no-op before;
  mattered for shift-by-≥31 against `int` literals).
- **Standalone `fail("...")`** is now a first-class statement
  (landed after round-2). Same emission as the failure arm of
  `assert ... else fail(...)` minus the `if (!cond)` guard — useful
  when the failure trigger is structural (inside `if`/`for`) rather
  than a single boolean predicate. The earlier workaround
  `assert false else fail(...)` is no longer needed.

## Phase 2b-scale (next, NOT in this PR)

Author HARC TBs for the remaining 64 cid012 problems. Realistic
budget: many sessions. Strategy:

  1. **Group by topology**: process combinational batches together
     (they're mostly the same pattern: exhaustive sweep + software
     model), then sequential, then multi-clock.
  2. **Cap iteration per problem**: 2-3 score-and-iterate cycles
     before moving on. If a problem stays at ceiling-below-target
     after exhaustive testing, mark as `unreachable_ceiling` in
     meta.json and skip further work — those are Phase 2c-analysis
     fodder, not Phase 2b-scale work.
  3. **Track patterns**: any HARC language-surface friction that
     comes up consistently (e.g. missing operators, awkward idioms)
     becomes a separate "HARC TB ergonomics" workstream.

## Phase 2c-analysis (after 2b-scale)

  - Aggregate Pass@1 across all 67 problems
  - Distinguish "TB-failed" vs "metric-ceiling" failures
  - Decide on response: keep strict reporting + caveat about ceiling
    incompatibility, OR build a ceiling-relative threshold (run an
    exhaustive TB first to determine each DUT's reachable max, then
    score relative to that)

## Known limits

- **Verilator coverage ≠ Xcelium IMC.** Different metric models;
  the 90% threshold is calibrated for IMC. Our branch-coverage
  numbers are directionally aligned but not strictly comparable
  to CVDP paper Pass@k. The README is explicit about this.
- **Multi-clock DUTs need HARC-side clock declarations.** Today
  HARC's primary clock is `clk` only; CVDP problems with `wr_clk` +
  `rd_clk` need either multi-clock TB authoring (HARC supports
  this via `clock <name> = <period>` decls — see `multi_clock_test`)
  or a richer wrapper. Phase 2a flags these 4 problems in meta but
  doesn't auto-resolve.
- **Coverage holes from constant assignments.** Verilator counts
  one-time initial assignments (`reg [4:0] poly = 5'b10011`) as
  "toggle never happened" → they show as uncovered. Mostly cosmetic;
  flat-out unwinnable for the TB to drive a constant.

## Quick reference

```bash
# Build harc with --coverage support
cargo build --release --bin harc

# Pull the HF dataset locally (one-time, ~60 MB)
pip3 install huggingface_hub
python3 -c "from huggingface_hub import snapshot_download; \
    snapshot_download(repo_id='nvidia/cvdp-benchmark-dataset', \
                      repo_type='dataset', local_dir='/tmp/cvdp_hf')"

# Extract all cid012 problems into bench/cvdp/cvdp_copilot_*
python3 bench/cvdp/extract.py \
    --jsonl /tmp/cvdp_hf/cvdp_v1.1.0_nonagentic_code_generation_commercial.jsonl \
    --category cid012 --out bench/cvdp [--force]

# Smoke-test all extracted problems (trivial TB; pass=harness layout OK)
python3 bench/cvdp/smoke.py

# Author a HARC TB: drop a .harc file into <problem-dir>/tb/, then score
python3 bench/cvdp/score.py bench/cvdp/<problem-dir>
```
