# CVDP × HARC verification benchmark — Phase 2a complete

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

## Phase 2b: TB authoring (next)

**Model: I'm the LLM.** Per the user, Claude authors each HARC TB
working only from the prompt + DUT source, never reading the gold
SV TB (there's no on-disk copy of it).

Per-problem cycle:

  1. Read `prompt.txt` + `dut/<dut>.sv`
  2. Author `tb/<dut>_tb.harc` directly
  3. Run `bench/cvdp/score.py <problem-dir>`
  4. If FAIL: iterate (broaden inputs, fix bugs); if PASS, move on
  5. Cap iteration count per problem so the run terminates

Authoring all 67 is many sessions of work. Reasonable midpoints:

  - **Phase 2b-pilot** (next): pick a representative sample — 1
    combinational, 1 simple sequential, 1 multi-clock — and author
    TBs to validate the loop with non-cheating inputs
  - **Phase 2b-scale**: batch the remainder
  - **Phase 2c-analysis**: aggregate Pass@1, identify systematic
    failure modes, decide whether to (a) tighten the prompt
    formulation, (b) extend the HARC language, or (c) accept the
    floor and report.

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
