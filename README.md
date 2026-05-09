# HARC

**HARC** (*Harness of ARCh*) is a verification language compiler — sister to **ARCH**, the hardware description language at [`arch-hdl-lang/arch-com`](https://github.com/arch-hdl-lang/arch-com). HARC produces a Verilator-driven C++ testbench from a high-level test description, with first-class support for transactions, constraint-randomized stimulus, transactors (synthesizable BFMs), scoreboards, covergroups, and concurrent assertions.

The full language reference is in [`spec.md`](spec.md).

## Status

Pre-1.0. Phases 1a + 1b + 2 + 3 + 4 of the spec roadmap are usable end-to-end (stimulus → observation → scoreboard → properties/coverage). 12 example fixtures from `arch-com/examples/` pass against the real Verilator-compiled SystemVerilog.

## Install

HARC needs:

- **Rust** (stable; uses Cargo)
- **Verilator** ≥ 5.0 — `brew install verilator` on macOS, `apt install verilator` on Debian/Ubuntu
- **Z3** ≥ 4.15 — `brew install z3` on macOS, `apt install libz3-dev` on Debian/Ubuntu

Build:

```sh
git clone git@github.com:arch-hdl-lang/harc-com.git
cd harc-com
cargo build --release
```

The binary lands at `target/release/harc`. Add it to your `PATH` or invoke via `cargo run --bin harc -- ...`.

## A first test

Given a SystemVerilog DUT — e.g. `arch-com/examples/sync_fifo.sv` — write a HARC test:

```harc
test SyncFifoTest
    let dut : TxQueue

    scope sim
        run
            dut.rst = 1
            dut.push_valid = 0
            dut.pop_ready = 0
            wait 2 cycles
            dut.rst = 0

            for i in 0 .. 16
                dut.push_valid = 1
                dut.push_data = i + 1
                wait 1 cycle
            end for
            dut.push_valid = 0

            for i in 0 .. 16
                dut.pop_ready = 1
                assert dut.pop_data == i + 1
                    else fail("pop ${i}: got ${dut.pop_data}")
                wait 1 cycle
            end for
            log(info, "PASS")
        end run
    end scope sim
end test SyncFifoTest
```

Run it:

```sh
harc sim --sv path/to/sync_fifo.sv path/to/sync_fifo_test.harc --top TxQueue
```

HARC compiles the test to C++, links Verilator's compiled DUT, runs the binary, and prints cycle-stamped log lines plus a coverage report.

## CLI

| Command | What it does |
|---|---|
| `harc check <files…>` | Type-check, lint, and report problems. No output. |
| `harc fmt <file>` | Pretty-print to stdout (round-trip target — output re-parses to a structurally equivalent AST). |
| `harc sim --dut <arch-source> [--top T] [--test N] <test-files…>` | Build the DUT through `arch sim` (uses ARCH's cpp simulation model), then run. |
| `harc sim --sv <verilog-files…> [--top T] [--test N] <test-files…>` | Run Verilator on the SV directly, then run. |

Common flags:

- `--seed N` — PRNG seed (env: `HARC_SEED`)
- `--outdir <dir>` — build artifact directory (default `harc_sim_build/`)
- `--emit-only` — emit C++ but don't compile/run

## Examples

[`tests/fixtures/`](tests/fixtures/) holds runnable HARC TBs targeting DUTs in [`arch-com/examples/`](https://github.com/arch-hdl-lang/arch-com/tree/main/examples). Quick reference:

| Fixture | DUT | Demonstrates |
|---|---|---|
| `rom_lut_test.harc` | `rom_lut.sv` | covergroup + helper functions |
| `bus_arbiter_test.harc` | `bus_arbiter.sv` | round-robin arbiter, named bins |
| `int_regs_test.harc` | `int_regs.sv` | regfile with hardwired addr 0 |
| `traffic_light_test.harc` | `traffic_light.sv` | FSM, `for _ in 0..N` |
| `sync_fifo_test.harc` | `sync_fifo.sv` | single-clock FIFO, full/empty asserts |
| `pipe_reg_test.harc` | `pipe_reg_test.sv` | concurrent property with `past(...)` |
| `single_port_ram_test.harc` | `single_port_ram.sv` | helper functions returning values |
| `pkt_queue_test.harc` | `pkt_queue.sv` | 2-cycle req/resp protocol |
| `synchronizer_basic_test.harc` | `synchronizer_basic.sv` | 2-clock CDC |
| `async_fifo_test*.harc` | `async_fifo.sv` | dual-clock FIFO, multi-file scope split |
| `axilite_*_test*.harc` | `axilite_regs.sv` | transactors (active+passive), scoreboards, randomize-with, events |
| `counter_test*.harc` | `wrap_counter.sv` | properties, cover properties, multi-file split |

See [`spec.md`](spec.md) for the language reference and [`HANDOFF.md`](HANDOFF.md) (locally generated) for the latest session-level state.

## Layout

```
src/
  ast.rs           AST types
  lexer.rs         Logos-based tokenizer
  parser.rs        Recursive-descent LL(1) parser
  pretty.rs        Pretty-printer (round-trip target)
  diagnostics.rs   miette-based error reporting
  codegen/
    cpp_tb.rs      Verilator C++ TB emitter (single file)
    merge.rs       Multi-file `extend test T` merging
  main.rs          CLI

tests/
  fixtures/        Runnable HARC tests targeting arch-com DUTs
  dut/             Vendored .sv DUT snapshots (refreshable from arch-com)
  snapshots/       insta snapshots for round-trip parser tests
  round_trip.rs    Snapshot-test driver
  run_fixtures.sh  End-to-end fixture runner (used in CI)

spec.md            Language reference
```

## Running the test suite locally

```sh
cargo test --release          # 32 cargo tests
./tests/run_fixtures.sh       # 23 fixtures end-to-end via Verilator
```

The fixture runner builds harc, then for each entry in its manifest:
runs Verilator on the vendored `.sv` DUT, links it against the HARC-
generated C++ testbench, and asserts the binary prints `ALL TESTS
PASSED`. CI runs the same script on every push and PR.

## Relationship to ARCH

HARC and ARCH share a lexer/parser style and several constructs (`domain`, `wait N cycles`, `=` blocking assignment, soft-keyword conventions). A test references the DUT through either:

1. **`harc sim --dut <arch-source>`** — pipes through `arch sim` against ARCH's built-in cpp simulation model. Fastest iteration for ARCH-authored DUTs.
2. **`harc sim --sv <verilog>`** — invokes Verilator on hand-written or arch-built SystemVerilog. The canonical path; matches what synthesis would produce.

Bug coverage rule of thumb: if the same test passes under `--dut` but fails under `--sv`, the divergence is an ARCH backend bug — file it against `arch-com`.

## License

TBD.
