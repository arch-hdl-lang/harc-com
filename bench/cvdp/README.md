# CVDP × HARC verification benchmark — Phase 2b-pilot

A HARC-flavored re-implementation of NVIDIA's CVDP cid012 (testbench-generation)
scoring loop, using Verilator semantic control coverage in place of Cadence IMC. The
CVDP problem set provides the DUTs and prompts; HARC TBs replace the
SystemVerilog TBs the reference benchmark expects.

## What this is, what it isn't

**Is:** end-to-end working loop that takes a CVDP cid012 problem, runs a HARC
TB against it via `harc sim --coverage`, post-processes the Verilator coverage
data into a semantic control-coverage %, and compares to the problem's target. Validates
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
├── cid012_gf_multiplier_thin/                 ← thin control-score sanity case
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
   `coverage.dat` to LCOV format for line coverage, and parses Verilator's
   typed `coverage.dat` records directly for control coverage.
4. **Score.** Filters to `meta.dut_module`'s source file (drops the
   `_top` wrapper and the TB itself). Reports semantic control coverage on the DUT,
   compares to `meta.target_coverage`, exits 0 on PASS / 1 on FAIL.

**Why control coverage and not line coverage?** Many cid012 DUTs are
combinational — `always @(*)` blocks where every line executes on every input
change. Line coverage can hit 100% on a single-input TB. Semantic control
coverage counts Verilator `v_branch` and `v_expr` points, which is closer to
Xcelium IMC's branch/expression metrics while avoiding toggle-as-branch noise.

**Why not LCOV `BRDA` directly?** Verilator's raw `coverage.dat` keeps point
types in the `page` field (`v_branch`, `v_toggle`, `v_expr`, ...), but
`verilator_coverage --write-info` can flatten non-branch points such as
`v_toggle` signal-bit coverage into LCOV `BRDA` rows. The scorer therefore
uses LCOV `DA` rows for line coverage and raw `page=v_branch/...` plus
`page=v_expr/...` records for semantic control coverage.

## Phase 1 validation: gf_multiplier (hand-authored)

The DUT is a GF(2⁴) polynomial multiplier mod x⁴+x+1 — 4 inputs × 4 inputs,
purely combinational. Two HARC TBs:

| TB | Strategy | DUT control coverage | Verdict |
|---|---|---|---|
| `cid012_gf_multiplier/`        | Exhaustive 16×16 sweep + inline software model assertion | 4/4 = **100.00%** | **PASS** (≥90%) |
| `cid012_gf_multiplier_thin/`   | Single A=1,B=1 input | 4/4 = **100.00%** | **PASS** (≥90%) |

Both TBs cover the semantic control points. The thin TB no longer rejects under
the corrected metric because its weaker exploration only shows up in toggle
coverage, which is intentionally excluded from control scoring.

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

## Phase 2b: authored HARC TB scoreboard (semantic control metric)

All TBs are authored from `prompt.txt` + `dut/<dut>.sv`; the gold SV TBs are
not materialized on disk. Scores below use the current scorer behavior:

- line coverage comes from LCOV `DA` rows emitted by `verilator_coverage --write-info`;
- control coverage comes from raw Verilator `coverage.dat` records with `page=v_branch/...`
  or `page=v_expr/...`;
- `page=v_toggle/...` records are intentionally excluded from control coverage because
  Verilator 5.034 can flatten them into LCOV `BRDA` rows;
- DUTs with no branch/expression control points fall back to line coverage for the verdict;
- structurally unreachable Verilator points are listed in a generated `.vlt`
  control file and mirrored by the scorer while parsing coverage.

This corrects earlier README rows that treated toggle coverage as branch coverage,
then removes known structural holes from Verilator's denominator before scoring.

| Problem | Line | Control | Scored Metric | Target | Verdict |
|---|---:|---:|---|---:|---|
| `cid012_gf_multiplier` | 100.00% | 100.00% | control | ≥90% | **PASS** |
| `cid012_gf_multiplier_thin` | 100.00% | 100.00% | control | ≥90% | **PASS** |
| `32_bit_Brent_Kung_PP_adder_0004` | 100.00% | n/a | line | ≥80% | **PASS** |
| `MSHR_0003` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `Synchronous_Muller_C_Element_0003` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `adc_data_rotate_0009` | 100.00% | 100.00% | control | ≥92% | **PASS** |
| `afi_ptr_0004` | 96.25% | 100.00% | control | ≥80% | **PASS** |
| `apb_dsp_op_0006` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `apb_dsp_unit_0003` | 100.00% | 100.00% | control | ≥98% | **PASS** |
| `apb_history_shift_register_0003` | 100.00% | 100.00% | control | ≥98% | **PASS** |
| `asyc_reset_0004` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `bcd_adder_0007` | 100.00% | n/a | line | ≥95% | **PASS** |
| `binary_to_BCD_0030` | 100.00% | 100.00% | control | ≥90% | **PASS** |
| `cdc_pulse_synchronizer_0017` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `cellular_automata_0002` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `csr_using_apb_0005` | 100.00% | 100.00% | control | ≥90% | **PASS** |
| `decode_firstbit_0017` | 100.00% | 93.33% | control | ≥90% | **PASS** |
| `encoder_8b10b_0026` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `endian_swapper_0004` | 99.24% | 98.41% | control | ≥92% | **PASS** |
| `fixed_arbiter_0004` | 100.00% | 100.00% | control | ≥95% | **PASS** |
| `generic_nbit_counter_0013` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `gray_to_binary_0014` | 100.00% | 100.00% | control | ≥95% | **PASS** |
| `hamming_code_tx_and_rx_0029` | 100.00% | n/a | line | ≥91% | **PASS** |
| `hamming_code_tx_and_rx_0031` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `hamming_code_tx_and_rx_0037` | 96.67% | 100.00% | control | ≥97% | **PASS** |
| `IIR_filter_0016` | 100.00% | 100.00% | control | ≥95% | **PASS** |
| `image_stego_0014` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `manchester_enc_0009` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `morse_code_0027` | 100.00% | n/a | line | ≥95% | **PASS** |
| `nbit_swizzling_0009` | 100.00% | n/a | line | ≥100% | **PASS** |
| `ping_pong_buffer_0004` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `ring_token_0004` | 94.87% | 100.00% | control | ≥100% | **PASS** |
| `secure_read_write_bus_0005` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `secure_variable_timer_0006` | 100.00% | 100.00% | control | ≥98% | **PASS** |
| `Serial_Line_Converter_0006` | 98.86% | 100.00% | control | ≥95% | **PASS** |
| `signed_adder_0003` | 100.00% | 100.00% | control | ≥99% | **PASS** |
| `simple_spi_0003` | 98.06% | 100.00% | control | ≥94% | **PASS** |
| `single_cycle_arbiter_0004` | 100.00% | n/a | line | ≥96% | **PASS** |
| `skid_register_0004` | 100.00% | 100.00% | control | ≥98% | **PASS** |
| `sram_fd_0024` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `static_branch_predict_0035` | 100.00% | n/a | line | ≥95% | **PASS** |
| `traffic_light_controller_0007` | 100.00% | 100.00% | control | ≥97% | **PASS** |
| `word_change_detector_0012` | 100.00% | 100.00% | control | ≥90% | **PASS** |
| `word_reducer_0012` | 100.00% | 100.00% | control | ≥100% | **PASS** |
| `write_through_data_direct_mapped_cache_0001` | 97.50% | 100.00% | control | ≥85% | **PASS** |
| `wb2ahb_0004` | 97.78% | 100.00% | control | ≥95% | **PASS** |

### Net scoreboard

**46/46 PASS, 0/46 FAIL** under the semantic control metric plus explicit `.vlt`
structural waivers. The false failures caused by toggle-as-BRDA accounting are
removed from the scoreboard:

- `adc_data_rotate_0009`: old LCOV-BRDA branch was 85.29%; semantic control is 100.00%.
- `afi_ptr_0004`: old LCOV-BRDA branch was ~58%; semantic control is 100.00%.
- `apb_dsp_op_0006`: old LCOV-BRDA branch was 66.51%; semantic control is now 100.00% after adding the missing invalid `DSP_WRITE_OP_O` high-address transaction.
- `hamming_code_tx_and_rx_0037`: old LCOV-BRDA branch was 83.93%; semantic control is 100.00%.
- `morse_code_0027`: old LCOV-BRDA branch was 94.44%; no semantic control points, so line coverage scores 100.00%.

Structural waivers are applied only for points that are unreachable under fixed
problem parameters or by source type width:

- `Synchronous_Muller_C_Element_0003`: default `PIPE_DEPTH=1` makes generated
  pipeline stages with `i != 0` unreachable.
- `apb_dsp_unit_0003`: 10-bit `paddr` cannot exceed `MEM_SIZE=1024`.
- `bcd_adder_0007`: submodule `cin` inputs are tied constant in both DUT instances.
- `binary_to_BCD_0030`: 8-bit input cannot make the hundreds BCD digit ≥5.
- `decode_firstbit_0017`: default `OutputFormat_g=0` leaves the one-hot output branch unreachable.
- `encoder_8b10b_0026`: the two-state `current_disparity` enum is reset and
  assigned only valid states, making its `default` recovery line unreachable.
- `nbit_swizzling_0009`: 2-bit `sel` covers every explicit case item, making `default` unreachable.
- `simple_spi_0003`: outer `!i_enable` and `i_fault` priority branches make
  the corresponding subterms of the nested IDLE-state condition unreachable.

### Latest auto-coverage stress findings

- `simple_spi_0003` uses `[unique within test]` on 16-bit payloads plus natural
  walking endpoints to cover normal transfers, fault, clear, and FSM state bins.
- `csr_using_apb_0005` uses APB transaction randomization, relation inlining for
  invalid-address accesses, and 32-bit data auto coverage. It reaches 100% control.
- `endian_swapper_0004` uses directed 64-bit walking/endpoint patterns and
  randomized 32-bit halves. Direct `uint<64>` solver extraction is now covered
  by the issue #167 regression fix, but this TB keeps the two-half stimulus
  explicit so the byte-swapping intent stays obvious.
- `skid_register_0004` uses `[unique within test]` on 32-bit payloads while
  crossing upstream valid, downstream ready, and data endpoint/walking bins.
- `encoder_8b10b_0026` combines exhaustive data symbols, directed control
  symbols, relation-constrained control-symbol randomization, and single-cycle
  staging to hit the control running-disparity branch.
- `secure_variable_timer_0006` uses `wait until ... timeout`, unique randomized
  4-bit delays, long counting waits, and start-pattern/ack coverage.
- `word_change_detector_0012` uses solver-randomized data/mask/pattern triples
  plus directed latch/enable/masked-change cases to hit both per-bit change
  detection and pattern-match pulse paths.
- `ping_pong_buffer_0004` uses long fill/drain sequences to wrap both pointers
  twice, which is necessary to cover `buffer_select <= !buffer_select` with
  both input polarities.
- `cdc_pulse_synchronizer_0017` manually toggles the source and destination
  clock inputs under the HARC primary clock, giving a small cosim-like stress
  case for non-primary clock stimulus.
- `IIR_filter_0016` uses signed unique samples plus directed impulse, step,
  endpoint, and alternating-signed sequences to exercise reset and normal
  signed arithmetic history updates.
- `word_reducer_0012` uses exhaustive 3-bit operand pairs plus a
  solver-randomized `[unique within test]` XOR-difference mask to cover every
  Hamming-distance bucket and prove the combinational output against a model.
- `traffic_light_controller_0007` uses directed FSM hold/transition stimulus
  and output covergroups to hit every line and semantic control branch,
  including the S3 hold path that requires vehicle present with long timer low.
- `Serial_Line_Converter_0006` uses a `[unique within test]` mode/pattern
  transaction plus directed 8-mode sweeps; holding each mode long enough for
  the divider terminal count exercises RZ and scrambled-output behavior.
- `write_through_data_direct_mapped_cache_0001` uses directed cache protocol
  sequences plus constrained randomized tag/index/data accesses to cover miss
  fill, hit, write allocation, same-index replacement, uncached bypass, delayed
  memory ready, boundary addresses, reset invalidation, and sliced-address
  coverpoints (`dut.cpu_addr[7:0]`). The complex bench exposed a CVDP DUT
  issue: undeclared `cache_dout` becomes a 1-bit implicit Verilator wire, so
  hit-read data cannot be checked meaningfully without fixing the DUT.
- `wb2ahb_0004` declares generated `clk` and `hclk` clocks, then uses
  `wait ... on hclk` to cover Wishbone/AHB select decoding, read/write phases,
  AHB wait states, data-phase stalls, and idle/busy/nonseq transfer states.
  This validates HARC's ARCH-shaped multi-clock model on a CVDP bus bridge.
- `apb_history_shift_register_0003` uses generated `clk` for APB CSR traffic
  and short wall-time pulses on the event-like `history_shift_valid` input to
  cover no-op, normal prediction, misprediction-priority restore, full/empty
  history flags, invalid-address errors, and clock-gated APB no-update cases.
  This validates mixed generated-clock plus event-edge stimulus without adding
  a second artificial clock domain.
- `restoring_division_0006` is currently not an authored scoreboard case because
  the DUT does not converge in Verilator at cycle 0, before testbench stimulus.

### Metric caveats

Semantic control coverage is much closer to source branch/expression coverage than
LCOV `BRDA`, but it is not a full replacement for CVDP's Cadence IMC metric. In
particular, the old thin `cid012_gf_multiplier_thin` sanity case now passes
because it covers the DUT's semantic branch/control points; its weaker data-space
exploration only shows up in toggle coverage, which is intentionally not part of
this control score.

### HARC language ergonomics noted

- **`else if` is two tokens**; HARC uses single-token `elsif`. Easy to hit when
  porting from SV-style sources.
- **`bits` is a reserved identifier** — rename locals (`v`, `data`, etc.).
- **`.trunc<N>()` (PR #117)** earns its keep: image_stego's TB needed to narrow
  a 33-bit intermediate (`sum + offset`) back to the DUT's 32-bit output before
  comparing — `.trunc<32>()` does it cleanly; `as uint<32>` would have been a
  no-op relabel.

## Phase 2b-scale (next, NOT in this PR)

Author HARC TBs for the remaining unscored cid012 problems. Realistic
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
  the 90% threshold is calibrated for IMC. Our semantic control-coverage
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
