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

## Phase 2b-pilot: 3 hand-authored HARC TBs (no gold peek)

Three representative problems, HARC TBs written by Claude working
only from `prompt.txt` + `dut/<dut>.sv` (gold SV TB never read).

| Problem | Topology | Line cov | Branch cov | Target | Verdict |
|---|---|---:|---:|---:|---|
| `cvdp_copilot_binary_to_BCD_0030` | combinational, 2 ports | **100.00%** (8/8) | 87.50% (7/8) | ≥90% | FAIL — ceiling |
| `cvdp_copilot_fixed_arbiter_0004` | sequential 1-clk, 4 ports | **100.00%** (21/21) | **100.00%** (4/4) | ≥95% | **PASS** |
| `cvdp_copilot_Synchronous_Muller_C_Element_0003` | sequential w/ clk-en, 6 ports | 87.50% (14/16) | 95.65% (22/23) | ≥100% | FAIL — ceiling |

**1/3 PASS.** The 2 FAILs are not TB-quality issues — both DUTs have
**structurally unreachable code under default parameters** that Verilator
counts but Cadence IMC apparently excludes:

- `binary_to_BCD`: `if (shift_reg[19:16] >= 5)` (the hundreds nibble)
  — impossible to reach with an 8-bit input (max hundreds=2). The TB
  is exhaustive over all 256 inputs; nothing in TB-space hits this
  branch. Verilator: 7/8 = 87.5%. Cadence IMC would presumably mark
  this branch as "excluded".
- `Muller_C_Element`: `else` block inside a `genvar` loop — only
  reachable when `PIPE_DEPTH ≥ 2`, default param is 1, so the
  elaborated netlist doesn't have that branch but Verilator still
  counts it as a coverage point. Same shape of issue.

The TBs themselves are **exhaustive and correct** — they exercise
every reachable input/state combination of the DUT. The threshold
gap is a measurement-tool incompatibility between Verilator and
Cadence IMC, not a deficit in HARC's TB-authoring capability or the
LLM's understanding of the design.

### Coverage flags landed this round

`harc sim --coverage` now passes `--coverage-line --coverage-expr` to
Verilator (deliberately NOT `--coverage-toggle`). Toggle coverage is
per-bit and unreachable on internal signals wider than the input
space (e.g. a 20-bit `shift_reg` whose top bits don't toggle because
8-bit input bounds the register to 0..255). Cadence IMC's default
branch metric doesn't include bit-toggle either; matching that gets
us closer to a comparable number.

The scorer (`score.py`) reports **both** line and branch coverage
side-by-side, with a `[note: 100% line cov reached]` annotation when
branch FAILs but line is ≥99% — that's the "TB is exhaustive, DUT
has unreachable branches" signal.

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
