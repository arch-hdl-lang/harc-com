# DPI-C co-sim with HDL simulators: exploration + working spike (Verilator, Icarus)

**Date:** 2026-07-24
**Status:** ✅ Implemented (same day) — `harc sim --sv ... --cosim dpi` ships the
backend; see *Implementation* at the end. The hand-written spike in
`spikes/dpi-cosim/` remains as the contract's minimal reproduction (and the
only Icarus/VPI demonstration).
**Related:** spec §10 "Verilator DPI-C co-sim pilot" / "Commercial-simulator co-sim" (spec.md:2527–2542)

---

## Summary

Spec §10 sketches a v1.1+ backend where the HDL simulator owns time and HARC
becomes a passive DPI-C runtime entered from generated SV hook points —
the rehearsal path for VCS/Xcelium/Questa co-sim. This exploration builds
that contract end-to-end as a hand-written spike and runs it against two
simulators:

- **Verilator 5.020** (`--binary --timing`), through real DPI-C
  imports/exports — the pilot the spec names.
- **Icarus Verilog 12.0**, which turns out to have **no DPI-C support at
  all** (`import "DPI-C"` is a syntax error). The same HARC-side C ABI is
  reached instead through a thin IEEE 1800 **VPI** bridge; the TB core
  object file is byte-identical between the two builds.

Result: the ported `sync_fifo_test` run block — scheduled by the
**unmodified production scheduler** (`runtime/harc_thread_rt.h`, the same
header the direct backend emits against) — passes on both simulators with
identical cycle counts (36) and, in a 1M-cycle randomized-free soak,
identical observable behavior (`popped=999999` on all three of: direct
backend, Verilator-DPI, Icarus-VPI).

Throughput (1M-cycle saturating push+pop soak, 6 boundary crossings/cycle,
this container, one core):

| Path | cycles/s | vs direct |
|---|---|---|
| Direct Verilator backend (today's `harc sim --sv`) | ~3.4M | 1.0× |
| Verilator DPI-C co-sim (spike) | ~1.8M | ~0.53× |
| Icarus VPI co-sim (spike) | ~0.38M | ~0.11× |

The DPI tax on Verilator is ~2× on a boundary-heavy microbenchmark — an
upper bound on the penalty, since real TBs do more in-HARC work per signal
touch. That is comfortably inside "vendor-flow compatibility path" budget;
the direct backend stays the fast path, as the spec intends.

## What the spike is

```
spikes/dpi-cosim/
  harc_cosim_core.cpp    # simulator-neutral "generated TB" stand-in:
                         #   sync_fifo_test's run block as a C++20 coroutine
                         #   on harc_rt::ThreadScheduler, plus the three
                         #   C entrypoints harc_init / harc_on_posedge /
                         #   harc_finish. Compiled unchanged into both builds.
  harc_cosim_sig_ids.h   # DUT port <-> integer id table (generated, in the
                         #   real integration, from the same port list the
                         #   `let dut : T` binding uses)
  harness.sv             # generated-harness stand-in; DPI flavor by default,
                         #   `+define+HARC_COSIM_VPI` selects the Icarus flavor
  dpi_adapter.cpp        # harc_dut_get/set -> DPI-exported harc_sv_get/set
  vpi_adapter.c          # harc_dut_get/set -> vpi_get_value/vpi_put_value,
                         #   $harc_init/$harc_tick system tasks
  run_verilator.sh       # build + run + assert "ALL TESTS PASSED"
  run_icarus.sh          # same, via iverilog/vvp with the .vpi module
```

The layering is the finding, not an accident:

```
           harc_cosim_core.cpp  (HARC runtime + coroutines; no simulator types)
                    │  C ABI: harc_init / harc_on_posedge / harc_finish
                    │         harc_dut_get(id) / harc_dut_set(id, val)
        ┌───────────┴───────────┐
  dpi_adapter.cpp         vpi_adapter.c
  (2 forwarding calls)    (VPI handles resolved once at $harc_init)
        │                       │
  harness.sv DPI flavor   harness.sv VPI flavor
  (Verilator, VCS,        (Icarus, or any sim where
   Xcelium, Questa)        only VPI is available)
```

Everything above the adapter line satisfies the spec §10 contract:
simulator owns time (1), HARC owns TB intent (2), signal access through
typed generated accessors keyed by id — no hierarchical strings in TB code
(3), coroutines resume only inside the entrypoints (4), same TB source
semantics as the direct backend (5).

## Findings

### 1. The spec contract works as written — bootstrap ordering included

`harc_init()` is called from the harness's `initial` block at time zero and
runs `ThreadScheduler::bootstrap()`, so reset/default drives land before
the first posedge — the same ordering guarantee the direct backend gets by
calling `bootstrap()` before its drive loop. `harc_on_posedge()` runs
`sched.tick()` once per cycle at the negedge point of the master process
(post-posedge state settled, drives land half a cycle before the next
posedge — no active-region race with the DUT's `always_ff`). This maps
1:1 onto the direct backend's `posedge eval → tick() → negedge eval`
quantum; the ported fixture needed **zero changes to wait/cycle counts**
to match (36 cycles on both paths).

### 2. Verilator hazard: a context DPI import in an `always` block fires twice per event

The obvious harness shape

```systemverilog
initial begin harc_init(); clk = 0; forever #5 clk = ~clk; end
always @(negedge clk) if (harc_on_posedge() != 0) begin ... end
```

is miscompiled-for-our-purposes by Verilator 5.020: the *call expression*
is evaluated in more than one scheduling region, so `harc_on_posedge()`
executes **twice per negedge event** while the block's own `$display`
executes once (verified empirically; see the comment block in
`harness.sv`). The coroutine then advances two quanta per clock and every
other drive is overwritten before the DUT ever samples it — the FIFO test
fails with 8 of 16 pushes latched.

The robust shape is a **single timed master process** that owns both the
clock and the hook call:

```systemverilog
initial begin
  harc_init();
  clk = 0;
  forever begin
    #5 clk = 1;
    #5 clk = 0;
    if (harc_on_posedge() != 0) begin harc_finish(); $finish; end
  end
end
```

Under `--timing` that process is one coroutine resumed exactly once per
delay, so the import runs exactly once per cycle. Event-driven simulators
execute either shape once; the generated harness should emit the master-
process shape unconditionally. **Takeaway for the real backend: DPI
imports with side effects must never sit in re-evaluable process
positions; the harness generator owns this invariant, not the user.**

### 3. Icarus has no DPI at all — but the contract survives via a standard VPI bridge

`iverilog -g2012` rejects `import "DPI-C"` with a plain syntax error
(no partial support to speak of). However, the spec's actual portable
surface — the three C entrypoints + typed id-keyed accessors — does not
care what transports it. A 120-line VPI bridge (`vpi_adapter.c`) provides
it: `$harc_init` resolves the signal handles once and calls `harc_init()`;
`$harc_tick` calls `harc_on_posedge()`; gets/sets go through
`vpi_get_value`/`vpi_put_value(vpiNoDelay)`. Nothing in the bridge is
Icarus-specific — it is IEEE 1800 VPI that every commercial simulator also
ships.

This is consistent with spec.md:2542's guardrail read carefully: it warns
against an *Icarus-only VPI abstraction* in the backend. The spike's HARC
side has **no VPI abstraction at all** — the bridge lives entirely below
the neutral C ABI, in one adapter file, and the TB core object is shared
byte-for-byte with the DPI build. If the project ever wants an Icarus (or
"VPI-only simulator") target, it is one small generated adapter away, with
no new surface in the compiler or runtime. That said, Icarus's ~9× deficit
vs the direct backend means it earns its keep only as a portability
smoke-target, exactly as the spec predicted ("useful for ad-hoc DUT
diagnosis").

Two Icarus quirks worth recording:
- At time zero, `clk = 0` transitions X→0, which Icarus reports as a
  **negedge** — a separate `always @(negedge clk)` hook would fire a
  spurious tick at t=0. The master-process shape sidesteps this too.
- The `.vpi` module must contain both the bridge and the TB core; building
  it is a plain `g++ -shared` (C++20 for the coroutines), no
  `iverilog-vpi` dependency needed beyond its `--cflags` include path.

### 4. The production scheduler needed zero changes

`runtime/harc_thread_rt.h` was included as-is: it has no Verilator or
`main()` coupling — `ThreadScheduler::bootstrap()/tick()/all_done()` are
exactly the passive, call-in-driven API the co-sim contract needs
(spec §10 point 4). This de-risks the biggest piece of the eventual
backend: the emitted per-test code (`run_<Test>` bodies, covergroups,
checkers) can be reused; only the *driver* around it (drive_loop + main
dispatcher vs. entrypoints + harness) changes.

### 5. Cost profile

Per boundary crossing, Verilator DPI export calls are cheap (~10⁻⁷ s
including the id-switch accessor); VPI handle-based access is ~5× more.
The soak (6 crossings + one scheduler tick per cycle) puts the whole-TB
overhead at ~2× vs direct on Verilator. Two implications for the real
backend:

- **Batching matters at the margins, not the median.** A per-cycle
  "read all sampled signals into a struct" exported task (one crossing
  per cycle instead of N) is the obvious lever if the tax ever matters,
  and it fits the existing TB-IR sampling model (covergroup/checker
  sampling is already cycle-batched).
- **Don't route hot paths through co-sim.** The direct backend remains
  the default; co-sim is the compatibility/vendor path, per spec.

## Proposed compiler integration (not implemented)

Smallest-diff shape, reusing the existing emission pipeline:

1. **CLI:** `harc sim --sv ... --cosim dpi` (mutually exclusive with the
   default direct path; later `--cosim vpi` for VPI-only simulators).
2. **Codegen:** a driver-emission variant in `src/codegen/tbir/runtime.rs`
   — emit `harc_init/harc_on_posedge/harc_finish` entrypoints wrapping the
   existing scheduler + per-test bodies instead of `main()` + drive_loop.
   Signal access lowers `dut.<port>` to `harc_dut_get/set(id)` instead of
   Verilated member access — a small alternative in the same place TB-IR
   already abstracts DUT access for the `--dut` (ARCH) backend.
3. **Harness generation:** emit `HarcCosimTop.sv` (id table, accessor
   functions, master process) from the same DUT port list used today for
   probe stubs (`src/codegen/sv_stub.rs` is the precedent).
4. **Build:** `run_verilator` grows a co-sim mode: `--binary --timing`
   (drop `--no-timing`), pass the generated harness as top. Note
   `--timing` also lifts the direct backend's documented `#delay`
   limitation (spec §10 v1 limitations) for DUTs that need it — co-sim
   incidentally becomes the answer for delay-dependent DUTs.
5. **Test selection / multi-test:** the entrypoints take the test name at
   `harc_init` time (env var, same as `HARC_TEST` today); one process per
   test run, as now.
6. **CI:** the spike runs in seconds; a `run_cosim_spike.sh` job gated on
   `verilator`+`iverilog` presence would keep the contract from rotting
   until the real backend lands. (Not wired up in this exploration —
   Icarus isn't in CI's toolchain yet.)

Open questions for the real backend (deliberately not solved in the spike):

- **Wide signals** (>64 bit): the accessor ABI needs a word-array variant
  (`svBitVecVal*` on DPI, `vpiVectorVal` on VPI); the id table gains a
  width column. The direct backend's `_harc_u128`/word-array handling in
  `c_type_for` is the pattern to follow.
- **Multi-clock:** the harness must generate one timed process per
  declared clock plus a shared "which edges fired" bitmask into a single
  `harc_on_edge(mask)` entrypoint, mirroring `eval_clocks_until`.
- **Waves/coverage:** on co-sim these belong to the simulator
  (`$dumpvars`, vendor coverage), not to HARC's `--waves` plumbing —
  needs a CLI story so users aren't surprised.
- **`$finish` from the DUT:** a DUT-initiated finish must flush HARC
  (watchdog diagnostics, coverage report) — needs a `final`-block hook
  calling `harc_finish()` idempotently.

## Reproducing

```sh
./spikes/dpi-cosim/run_verilator.sh                      # DPI-C, Verilator ≥ 5.x
./spikes/dpi-cosim/run_icarus.sh                         # VPI, Icarus 11/12
HARC_COSIM_SOAK=1000000 ./spikes/dpi-cosim/run_verilator.sh   # throughput soak
HARC_COSIM_DEBUG=1 ./spikes/dpi-cosim/run_verilator.sh        # per-cycle signal dump
```

Both scripts exit nonzero unless the sim prints `ALL TESTS PASSED`
(same marker `tests/run_fixtures.sh` asserts).

---

## Implementation (landed 2026-07-24): `harc sim --sv ... --cosim dpi`

The proposed integration above was implemented the same day, with one
significant architecture change discovered during regression bring-up.

### What shipped

- **CLI:** `--cosim dpi` on `harc sim` (requires `--sv`; rejects `--waves`,
  `--coverage`, `--mt`, `--cpp-split tests`, `--check-backends`, and probe
  tests with targeted diagnostics).
- **Port discovery** (`cosim_ports_from_sv`, `src/codegen/cpp_tb.rs`): a
  tolerant scan of the `--top` module's ANSI header — direction + total
  packed width per port. Folds parameter/localparam chains with a small
  const-expression evaluator (`+ - * / ( )`, `$clog2`), resolves
  `typedef logic [..] T` / `typedef struct packed {…} T` /
  `parameter type T = …` widths, and is bounded to the header so
  `function` arguments in the body are never mistaken for ports.
- **Generated SV harness** (`HarcCosimTop.sv`, emitted per build):
  instantiates the DUT, exports id-keyed accessors (`harc_sv_get/set`
  for ≤64-bit ports, `harc_sv_get_word/set_word` 32-bit-word accessors
  for wider ones), and runs a single timed master process implementing
  the step protocol (`advance N ps` / `settle 1 ps` / done). `timescale
  1ps/1ps` with integer delays — a coarser unit rounded the 1 ps settle
  to a ZERO delay, freezing sim time and collapsing every clock edge
  into one timestep.
- **DUT shim** (emitted in the TB preamble): a struct named `V<Top>` whose
  members are `SigProxy<ID>` / `WideSigProxy<ID, NWORDS>`
  (`runtime/harc_cosim_rt.h`), so every `dut-><port>` access site in the
  existing TB-IR emission compiles unchanged. Proxy writes land as SV
  variable assignments inside the exported accessor — the simulator
  schedules re-evaluation itself, which is what raw `--public-flat-rw`
  pokes get wrong. `dut->eval()` maps to a 1 ps settle.
- **Build/run:** `run_verilator` swaps `--cc --exe --build` +
  `--no-timing` for `--binary --timing` with the harness as top;
  test selection flows through `HARC_TEST` (Verilator owns argv).
- **Runner:** `tests/run_cosim_fixtures.sh` — the full fixture table
  through the co-sim backend via `HARC_SIM_EXTRA_ARGS="--cosim dpi"`.

### The architecture change: thread bridge, not driver coroutine

The exploration proposed emitting the drive loop as a C++20 coroutine
that `co_yield`s time requests. That worked for simple fixtures but
cannot cover HARC's synchronous cycle-advance paths: helper functions
containing `wait`, and blocking TLM/bus calls, lower to plain functions
that call `tick()` — a position a coroutine cannot yield from. (First
seen as `rom_lut_test` reading all-zero data: its `read_addr` helper
advanced HARC cycles while simulator time stood still.)

The shipped design runs `run_<Test>` unchanged on a dedicated OS thread
under a **strict handshake** (`Bridge` in `runtime/harc_cosim_rt.h`):
exactly one of {simulator thread, TB thread} is ever runnable, and the
simulator thread is parked inside the `harc_cosim_step` DPI import the
entire time the TB runs — every TB-side access to simulator state still
happens inside a DPI entrypoint by delegation, honoring spec §10's
no-concurrent-access intent. "Yield to simulator" is now a plain
blocking call, legal from any emission context, so the direct backend's
entire synchronous machinery (helpers, TLM targets, transactor methods,
RAL) works under co-sim without emission changes. Verilator-specific
detail: the export trampolines resolve context/scope through
thread-locals, so the bridge captures `Verilated::threadContextp()` +
`svGetScope()` in `harc_cosim_init` and installs them on the TB thread.

Declared clocks needed no dedicated lowering: the clocked scheduler's
`dut-><clk> = level` writes are real SV edges through the DPI setter and
its `dut->eval()` calls are settles, so the simulator sees the correct
edge *sequence* — with time compressed (1 ps per edge instead of the
declared period). Fine for cycle-based tests; delay-dependent DUTs need
a future timing-faithful clocked lowering.

### Regression results (Verilator 5.034, clang, this container)

| Suite | Result |
|---|---|
| Direct backend (`tests/run_fixtures.sh`) | **130 / 130** — unchanged by the integration |
| Co-sim backend (`tests/run_cosim_fixtures.sh`) | **127 / 130** |
| `cargo test --release` | green (default-mode emission byte-identical) |

The 3 co-sim failures are the one remaining documented v0 gap:

- `probe_basic_test`, `probe_force_test`, `testbench_probe_dut_test` —
  probes need hierarchical access into the Verilated model, which lives
  inside the simulator on this path (rejected with a diagnostic).

Unpacked-array ports (`input logic [7:0] p [N]`, single dimension,
elements ≤ 64 bits) are supported through a third accessor pair
(`harc_sv_get_elem` / `harc_sv_set_elem`) and an element-indexed
`UnpackedSigProxy` in the shim — the TB-IR emission for these ports is a
raw subscript on both the read and write side, so a temporary element
ref with a conversion operator and an assignment operator covers every
access site.

Everything else — TLM targets/initiators, blocking bus calls, RAL
regblocks, transactors, scoreboards, covergroups, watchdogs, multi-clock
tests, randomize/Z3, 128-bit+ wide ports, extern-fn reference models —
passes through the co-sim backend with observable results matching the
direct backend.

### Cost

The sync_fifo 1M-cycle soak (spike numbers, same contract): co-sim is
~0.5× the direct backend on a boundary-heavy workload — the direct
backend remains the default fast path, exactly as spec §10 intends.
