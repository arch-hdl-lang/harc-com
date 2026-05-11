# CVDP × HARC verification benchmark — Phase 1

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

## Layout

```
bench/cvdp/
├── README.md                                  ← this file
├── score.py                                   ← the scoring loop
└── cid012_gf_multiplier/                      ← one problem directory
    ├── meta.json                              ← top_module + dut_module + target_coverage
    ├── prompt.txt                             ← original CVDP prompt (English)
    ├── dut/
    │   ├── gf_multiplier.sv                   ← CVDP-provided DUT (unmodified)
    │   └── gf_multiplier_top.sv               ← clocked wrapper for HARC's posedge loop
    ├── tb/
    │   └── gf_multiplier_tb.harc              ← candidate HARC testbench
    ├── gold/
    │   └── gf_multiplier_tb.sv                ← original CVDP gold SV TB (reference)
    └── build/                                 ← scorer output (coverage.dat, coverage.info, logs)
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

## Validation: gf_multiplier (CVDP cid012, example dataset)

The DUT is a GF(2⁴) polynomial multiplier mod x⁴+x+1 — 4 inputs × 4 inputs,
purely combinational. Two HARC TBs:

| TB | Strategy | DUT branch coverage | Verdict |
|---|---|---|---|
| `cid012_gf_multiplier/`        | Exhaustive 16×16 sweep + inline software model assertion | 30/33 = **90.91%** | **PASS** (≥90%) |
| `cid012_gf_multiplier_thin/`   | Single A=1,B=1 input — sanity-check the scorer rejects under-coverage | 16/33 = **48.48%** | **FAIL** |

The thorough TB clears the bar exactly where it should; the thin TB fails
cleanly. The scorer discriminates. Phase 1 plumbing validates.

## Known limits + Phase 2 work

- **Clockless DUTs.** HARC codegen unconditionally drives `dut->clk` in the
  main loop. The CVDP gf_multiplier DUT has no clock; we wrap it in a thin
  `gf_multiplier_top.sv` that exposes a phantom `clk`. Generalizing to all
  cid012 DUTs means either (a) per-problem wrapper generation, or (b) a
  HARC codegen change to detect clockless DUTs at Verilator-header probe
  time and skip the posedge loop. (b) is cleaner; punted to Phase 2.
- **Module-name → file-name discovery.** The scorer currently assumes
  the DUT file is named `<dut_module>.sv` (matching CVDP's convention).
  Robust would parse the SV file's `module <name>` line and match by
  parsed name. Fine for now; revisit if violated.
- **Coverage-point parity with IMC.** Verilator doesn't implement
  SystemVerilog covergroups or cross-coverage. For the
  combinational-DUT subset of cid012 this doesn't matter much (branch
  coverage already discriminates well), but problems that genuinely
  want covergroup-level scoring won't have a faithful equivalent.

## Phase 2 plan (next session)

1. Pull the full `cid012` set from
   `https://huggingface.co/datasets/nvidia/cvdp-benchmark-dataset`
   (no auth required) → ~Cid012 problem count TBD from HF.
2. For each problem, programmatically lay out a `bench/cvdp/<id>/`
   directory in the same shape as `cid012_gf_multiplier/`.
3. Probe each DUT's port list (parse `module <name>(...)` or use
   Verilator's `--xml-only` output) to decide whether a `_top` wrapper
   is needed. Auto-generate the wrapper when so.
4. Rewrite each problem's prompt: "Develop a SystemVerilog testbench
   for X" → "Develop a HARC testbench for X. Reference: <link to
   spec.md §7.2 + §8>. Output should be a single `.harc` file at
   `tb/<name>_tb.harc`."
5. Run the prompt through an LLM (gpt-5? claude-sonnet?), score
   Pass@1, log results.

## Quick reference — manual run

```bash
# Build harc with --coverage support
cargo build --release --bin harc

# Score one problem
python3 bench/cvdp/score.py bench/cvdp/cid012_gf_multiplier

# To author a new TB by hand: drop a .harc file into <problem>/tb/
# and rerun the scorer.
```
