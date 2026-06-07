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
| `<FILES>...` | Input `.harc` file(s). |
| `--dut <file.arch>` | ARCH DUT source file(s). Repeat for packages or shared definitions. Conflicts with `--sv` unless `--check-backends` is set. |
| `--sv <file.sv>` | SystemVerilog DUT source file(s). Drives Verilator directly. Conflicts with `--dut` unless `--check-backends` is set. |
| `--vlt <file.vlt>` | Verilator control file(s), such as waivers or coverage controls. Forwarded before SV DUT files. |
| `--top <Top>` | SV top module name. Defaults to the type of `let dut : <Type>` in the HARC source. |
| `--arch-bin <path>` | Path to the `arch` binary for the `--dut` path. Defaults to searching `$PATH`, then falling back to the sibling `../arch-com` checkout. |

## Test selection and build shape

| Option | Meaning |
|---|---|
| `--test <name>` | Pick a specific test by name when the input contains more than one test. |
| `--compile-scope suite` | Emit all tests into the generated build artifact. This is the default build-once-run-many mode. |
| `--compile-scope test` | Require `--test` and emit only the selected test. Useful when one test's generated C++ dominates compile time. |
| `--cpp-split off` | Preserve the current single generated C++ translation unit. This is the default. |
| `--cpp-split tests` | Emit a dispatcher plus grouped generated C++ translation units for tests, so Verilator/Make can compile HARC test objects independently. Currently supported only with `--sv`. |
| `--cpp-split-group-size <N>` | Number of tests per split C++ shard. Default: `4`. Smaller values improve per-test incremental granularity; larger values reduce compiler startup and header-parse overhead. |
| `--rebuild` | Force a clean rebuild by wiping `<outdir>/obj_dir/` before invoking Verilator. Useful after Verilator version changes, flag changes, or suspected stale object files. |
| `--jobs <N>` | Verilator build parallelism. Forwarded as `-j N`; use `0` to let Verilator choose based on available CPUs. |

## Output and run control

| Option | Meaning |
|---|---|
| `--outdir <dir>` | Output directory for generated C++, runtime headers, build outputs, logs, coverage, and waves. Default: `harc_sim_build/`. |
| `--emit-only` | Emit generated artifacts but do not compile or run. |
| `--seed <N>` | PRNG seed for `randomize()` calls. Default: `$HARC_SEED`, else `1`. |
| `--sim-arg <arg>` | Extra argument for the generated simulation binary, for example a plusarg. Repeatable. |
| `--mt` | Run bound-driver/bound-monitor coroutine actors on dedicated OS threads with dual-barrier sync. Default is cooperative single-thread execution, which is typically faster on current fixtures. |

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
| `--wave-file <path>` | Output path for the waveform. Defaults to `<outdir>/<TestName>.<ext>` or `<outdir>/waves.<ext>` when no test is selected. |
| `--trace-depth <N>` | Hierarchy depth passed to `dut->trace(tfp, N)`. Default: `99`. |
| `--no-trace-structs` | Disable expansion of packed structs in the waveform. |
| `--trace-max-width <N>` | Maximum traced signal width in bits. Default: `8192`. |
| `--trace-max-array <N>` | Maximum traced array size. Forwarded only when set explicitly. |
| `--verilator-arg <arg>` | Additional Verilator build flag. Repeatable. Appended after HARC defaults but before SV inputs. |

When changing waveform settings after a previous non-wave build, pass
`--rebuild` so Verilator does not reuse objects compiled without trace flags.

## Backend comparison

| Option | Meaning |
|---|---|
| `--check-backends` | Run the same test under both Verilator (`--sv`) and ARCH native sim (`--dut`) with the same seed, then diff their semantic traces. Requires both backends. |

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
