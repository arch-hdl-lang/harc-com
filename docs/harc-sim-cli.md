# `harc sim` CLI reference

This page is the browsable reference for `harc sim` options. The command's
`--help` output remains the source of truth for exact spelling and defaults.

## Basic forms

```sh
harc sim --sv <dut.sv> [--top Top] <test.harc>...
harc sim --dut <dut.arch> [--top Top] <test.harc>...
```

Pass exactly one of `--sv` or `--dut`, except when using
`--check-backends`, which requires both.

Input `.harc` files may include a base test plus sibling files that use
`extend test T`. HARC parses all inputs, merges matching extensions into
their base tests, emits a C++ testbench, then builds and runs it through the
selected backend.

## Common examples

Run a Verilator-backed SV DUT:

```sh
harc sim --sv dut.sv test.harc --top Top
```

Run an ARCH-authored DUT through `arch sim`:

```sh
harc sim --dut dut.arch test.harc
```

Run every test in a multi-test suite (the default when no `--test` is
given; the pass/fail banner covers all of them):

```sh
harc sim --sv dut.sv suite.harc --top Top
```

Select one test at runtime while keeping the build-once-run-many binary:

```sh
harc sim --sv dut.sv suite.harc --top Top --test Smoke
```

Compile only one selected test:

```sh
harc sim --sv dut.sv suite.harc --top Top --test Smoke --compile-scope test
```

Split generated C++ so Verilator/Make can compile HARC test shards in
parallel:

```sh
harc sim --sv dut.sv suite.harc --top Top --cpp-split tests --jobs 0
```

Tune split shard granularity:

```sh
harc sim --sv dut.sv suite.harc --top Top --cpp-split tests --cpp-split-group-size 2 --jobs 0
```

## Inputs and backend selection

| Option | Meaning |
|---|---|
| `<FILES>...` | Input `.harc` file(s). A missing or unreadable input (or `--dut`/`--sv`/`--vlt`/`--ref-src` file) is reported with its path and flag before anything is built. |
| `--dut <file.arch>` | ARCH DUT source file(s). Repeat for packages or shared definitions. Conflicts with `--sv` unless `--check-backends` is set. |
| `--sv <file.sv>` | SystemVerilog DUT source file(s). Drives Verilator directly. Conflicts with `--dut` unless `--check-backends` is set. |
| `--vlt <file.vlt>` | Verilator control file(s), such as waivers or coverage controls. Forwarded before SV DUT files. |
| `--param NAME=VALUE` | DUT parameter override. Repeatable. Lowers to Verilator `-GNAME=VALUE` for `--sv` and `arch sim --param NAME=VALUE` for `--dut`. |
| `--top <Top>` | SV top module name. Defaults to the type of `let dut : <Type>` in the HARC source. |
| `--arch-bin <path>` | Path to the `arch` binary for the `--dut` path. Defaults to searching `$PATH`, then falling back to the sibling `../arch-com` checkout. |

## Test selection and build shape

| Option | Meaning |
|---|---|
| `--test <name>` | Run only the named test. Without `--test` (and without `HARC_TEST`), every test in the suite runs in alphabetical test-name order, each against a fresh DUT, and the final banner covers all of them: `ALL TESTS PASSED` only when every test passed, otherwise `<N> TESTS FAILED` with `N` the number of failing tests (each test also prints a `[suite] test PASSED/FAILED` line). A single-test suite behaves exactly as before. (With the deprecated `--codegen v1`, only `--cpp-split-layout common` runs the whole suite; the default and `self-contained` v1 layouts keep the old first-test default.) |
| `--compile-scope suite` | Emit all tests into the generated build artifact. This is the default build-once-run-many mode. |
| `--compile-scope test` | Require `--test` and emit only the selected test. Useful when one test's generated C++ dominates compile time. |
| `--cpp-split off` | Preserve the current single generated C++ translation unit. This is the default. |
| `--cpp-split tests` | Emit a dispatcher plus grouped generated C++ translation units for tests, so Verilator/Make can compile HARC test objects independently. Currently supported only with `--sv`. |
| `--cpp-split-group-size <N>` | Number of tests per split C++ shard. Default: `4`. Smaller values improve per-test incremental granularity; larger values reduce compiler startup and header-parse overhead. Rejected under `--cpp-split-layout common` (which always emits one capsule per test). |
| `--cpp-split-layout self-contained\|common` | Layout for `--cpp-split tests`. `self-contained` (default) keeps the historical per-shard self-contained translation units. `common` compiles the shared runtime/testbench/component infrastructure once, emits one small stable capsule per test plus an explicit registry/dispatcher, and links an interface-ABI anchor so stale objects fail at link time (issue #643). Requires `--sv` and `--cpp-split tests`; works with both codegens. Under the default `--codegen tbir` it additionally requires `--compile-scope suite` and no `--vlt` / `--verilator-arg`. |
| `--emit-jobs <N>` | Parallel workers for HARC's own split C++ emission. Default: `0` (automatic: `min(available CPUs, shard count, 4)`). `1` is the deterministic serial path. Applies to `--codegen tbir --cpp-split tests`; ignored elsewhere. Distinct from `--jobs`. |
| `--rebuild` | Force a clean rebuild by wiping `<outdir>/obj_dir/` before invoking Verilator. Useful after Verilator version changes, flag changes, or suspected stale object files. |
| `--jobs <N>` | Verilator/native **build** parallelism. Forwarded as `-j N`; use `0` to let Verilator choose based on available CPUs. Does not affect HARC's own C++ emission — see `--emit-jobs`. |

## Output and run control

| Option | Meaning |
|---|---|
| `--outdir <dir>` | Output directory for generated C++, runtime headers, build outputs, logs, coverage, and waves. Default: `harc_sim_build/`. |
| `--emit-only` | Emit generated artifacts but do not compile or run. Under `--check-backends` both backends' testbenches are emitted and nothing is compiled, run or diffed. |
| `--seed <N>` | PRNG seed for `randomize()` calls. Default: `$HARC_SEED`, else `1`. `harc sim` parses `HARC_SEED` itself (the same parser for plain runs and `--check-backends`) and hands the simulation the resolved value in decimal: accepted forms are an unsigned 64-bit decimal (`42`) or `0x`/`0X` hex (`0x2a`); a single `_` may separate two digits (`1_000`, `0xdead_beef`), but not lead, trail or repeat; no sign; an empty value means unset; anything else is an error. A leading `0` is decimal (`010` is ten), unlike a generated binary run directly, which still reads `HARC_SEED` with C `strtoull` base 0 (`010` is eight there). |
| `--verbosity <LEVEL>` | Log verbosity threshold (spec §7.7): `LOW`, `MEDIUM`, `HIGH`, `DEBUG` or `FULL`, case-insensitive. A message prints when its verbosity is at or below the threshold; `warn`/`info` are `MEDIUM`, `debug` is `HIGH`, `error`/`fatal` always print. Default `MEDIUM`, so `log(debug, ...)` is hidden unless `--verbosity HIGH` or above. Hidden messages are still recorded in the semantic trace (`--record-trace`, `--check-backends`). A call's `verbosity=LEVEL` replaces its severity's default. `logf(...)` file sinks obey the same filter (a hidden message reaches neither stdout nor the file). The `seed=` line is always written. Forwarded as `HARC_VERBOSITY`. |
| `--verbosity-of <ID=LEVEL>` | Per-ID threshold, repeatable. Applies to log calls carrying exactly `id="ID"` and replaces `--verbosity` for them (raising or lowering). IDs are explicit only; there are no implicit component IDs. Forwarded as `HARC_VERBOSITY_OF` (`id=LEVEL,...`); a malformed entry is a CLI error. |
| `--sim-arg <arg>` | Extra argument for the generated simulation binary, for example a plusarg. Repeatable. Verilator (`--sv`) path only: on the ARCH backend (`--dut`, including `--check-backends`) `arch sim` owns the binary's command line, so `--sim-arg` is rejected with an error rather than dropped; use `--test <name>` to select a test. |
| `--mt` | Run bound-driver/bound-monitor coroutine actors on dedicated OS threads with dual-barrier sync. Default is cooperative single-thread execution, which is typically faster on current fixtures. |

### Environment variables read by the simulation

| Variable | Meaning |
|---|---|
| `HARC_TEST` | Test selection when `--test` is absent. `harc sim` does not clear it, so an ambient `HARC_TEST` in your shell selects that test exactly as `--test` would (and an unknown name fails with the list of available tests). Unset it to run the whole suite. |
| `HARC_SEED` | Seed when `--seed` is absent; see `--seed` for the accepted forms. |
| `HARC_VERBOSITY` | Verbosity threshold when `--verbosity` is absent (same level names). An unknown value warns and falls back to `MEDIUM`. |
| `HARC_VERBOSITY_OF` | Per-ID thresholds when `--verbosity-of` is absent: `id=LEVEL,id=LEVEL`. A malformed entry warns and is ignored. |

### Whole-suite runs and output files

When every test runs (no `--test`), the tests share one process and one
`<outdir>`. `sim.log`, the `--record-trace` / `--check-backends` semantic
trace, `logf(...)` files and the `--coverage-json` sidecar are truncated by the
first test that opens them in that process and appended to by later tests (a
`logf` file only a later test writes still starts empty); `sim.log` marks each
test with a `[suite] test <name>` line. Waveforms (`--waves`) and `coverage.dat` are
rewritten per test, so they hold only the last test; pass `--test` to capture
one test's waves or coverage. `--cosim dpi` runs one test per simulation, so a
multi-test suite there requires `--test` (or `HARC_TEST`).

## Coverage, reference models, and Z3

| Option | Meaning |
|---|---|
| `--coverage` | Enable DUT coverage collection. With `--sv`, passes `--coverage` to Verilator and writes `coverage.dat`. With `--dut`, forwards `--coverage` and `--coverage-dat=<outdir>/coverage.dat` to `arch sim`. |
| `--ref-src <file>` | C/C++ source file implementing `extern function` reference models. Repeatable. |
| `--z3-root <dir>` | Z3 installation prefix. Looks for `include/z3++.h` and `lib*/libz3`. |
| `--z3-include-dir <dir>` | Explicit include directory containing `z3++.h`. |
| `--z3-lib-dir <dir>` | Explicit library directory containing `libz3`. |

Z3 is required for constraint-randomized tests, such as
`randomize(t) with ...` and transaction `keep` constraints. The resolver
checks CLI flags first, then `HARC_Z3_INCLUDE_DIR` / `HARC_Z3_LIB_DIR`, then
`--z3-root`, `HARC_Z3_ROOT`, `third_party/z3`, and common system paths.

## Semantic traces and waveforms

| Option | Meaning |
|---|---|
| `--record-trace <file.jsonl>` | Record semantic runtime events as JSONL, including logs, failures, randomization results, and TLM method activity. |
| `--waves` | Enable Verilator VCD/FST waveform dumping. Default format is FST. |
| `--wave-format vcd\|fst` | Select waveform format. Default: `fst`. |
| `--wave-file <path>` | Output path for the waveform. Defaults to `<outdir>/<TestName>.<ext>` or `<outdir>/waves.<ext>` when no test is selected (a whole-suite run rewrites it per test, leaving the last test's waves). |
| `--trace-depth <N>` | Hierarchy depth passed to `dut->trace(tfp, N)`. Default: `99`. |
| `--no-trace-structs` | Disable expansion of packed structs in the waveform. |
| `--trace-max-width <N>` | Maximum traced signal width in bits. Default: `8192`. |
| `--trace-max-array <N>` | Maximum traced array size. Forwarded only when set explicitly. |
| `--verilator-arg <arg>` | Additional Verilator build flag. Repeatable. Appended after HARC defaults but before SV inputs. |

When changing waveform settings after a previous non-wave build, pass
`--rebuild` so Verilator does not reuse objects compiled without trace flags.

## Simulator-owned-time co-simulation (`--cosim dpi`)

`harc sim --sv <dut.sv> <test.harc> --cosim dpi` builds the spec §10
DPI-C co-sim pilot instead of the direct drive loop: a generated
`HarcCosimTop.sv` harness instantiates the DUT, owns the clock in a
timed master process (Verilator `--binary --timing`), and calls the TB
through `harc_cosim_init` / `harc_cosim_step` DPI imports; DUT port
access crosses the boundary through generated typed accessors. The
direct backend remains the fast path — co-sim exists to validate the
simulator-owned-time contract expected from commercial-simulator
backends (and it schedules DUT-internal `#delay`s, which the direct
path's `--no-timing` elides).

v0 limitations: no `--mt`, no `--waves`/`--coverage` (these belong to
the simulator on this path), no `--cpp-split tests`, no `--param`. Probes (including
`probe force`) work through the same bound SV stub as the direct
backend; unpacked-array ports are supported for a single unpacked
dimension with ≤ 64-bit elements; ports and probes are limited to
≤ 64-bit scalars except packed ports, which may be arbitrarily wide.
Run the fixture suite through it with
`tests/run_cosim_fixtures.sh`. Design notes and support matrix:
`docs/2026-07-24-dpi-cosim-exploration.md`.

## Backend comparison

| Option | Meaning |
|---|---|
| `--check-backends` | Run the same test under both Verilator (`--sv`) and ARCH native sim (`--dut`) with the same seed, then diff their semantic traces. Requires both backends. `--test`, `--compile-scope`, `--emit-only`, `--seed`/`HARC_SEED` and `--verbosity` apply to both legs. `--record-trace` is rejected (the two traces are always `<outdir>/trace_arch.jsonl` and `<outdir>/trace_sv.jsonl`), as are `--cpp-split tests` and `--cpp-split-layout common` (the ARCH backend has no split build), `--coverage-json` and `--cosim`. |

The trace diff is line-by-line, so backend trace event order must be
deterministic and stable.

## Split compile notes

`--cpp-split tests` is intended for larger Verilator-backed suites where one
large generated C++ file becomes the build bottleneck. It emits:

- a dispatcher `main.cpp`
- one or more grouped HARC test shard `.cpp` files

The default group size is `4` tests per shard. Smaller groups make individual
test edits more isolated; larger groups reduce duplicated C++ compiler startup
and header parsing.

For a single selected test, `--compile-scope test --test <name>` can be even
simpler than split mode because it emits only that test's code.

### Emission parallelism and progress (`--codegen tbir`)

Under TB-IR the suite is lowered and verified once, the dispatcher is
written before shard emission starts, and each shard is written as soon as
it finishes rather than at the end of the run. `--emit-jobs N` bounds how
many shards are emitted concurrently.

`--emit-jobs` and `--jobs` are deliberately separate knobs: the first is
HARC's frontend, the second is the downstream Verilator/native build.
Raising `--jobs` does nothing for a slow emit phase.

The worker count is a memory knob as well as a speed one — each in-flight
shard holds a whole generated translation unit in memory — which is why
automatic mode caps at 4 regardless of core count. Generated bytes are
identical at every `--emit-jobs` value; only the order of the per-shard
progress lines varies:

```
TBIR parse: 6.1s | merge: 0.1s
TBIR lower: 10.9s | verify: 0.4s
TBIR split plan: 352 tests, 11 shards, group size 32, emit jobs 4, planned in 10.8s
TBIR shard 3/11: 32 tests, 31.7 MB, 0.6s, emitted
TBIR shard 1/11: 32 tests, 31.7 MB, 0.7s, emitted
TBIR split emit: 11/11 shards, 348.6 MB, 0.8s
```

Note shard 3 is reported before shard 1 — under `--emit-jobs > 1` the
progress lines come in completion order, not index order. Each shard is
written by the worker that produced it, so writes overlap with emission
rather than queueing behind a single thread; the file each worker touches
comes from the split plan, so two workers never write the same path.

If a shard fails, the command exits non-zero and Verilator is never
invoked. The reported diagnostic is always the lowest-indexed failure, so
it does not depend on thread scheduling. Shards that already completed
stay on disk — and because a shard above the failing one may have finished
before the failure was observed, exactly which files remain can vary
between two identical failing runs. That is harmless: the next run
recomputes every shard and `write_if_changed` reuses whatever matches.
