# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this repo is

`harc-com` is the compiler/runtime for **HARC**, the verification language for
ARCH designs. The compiler binary is `harc`:

```bash
harc check Tb.harc                 # type-check a testbench
harc sim --sv dut.sv Tb.harc --top TopModule   # simulate against a DUT
```

The end-to-end fixture suite lives under `tests/` and is driven by
`tests/run_fixtures.sh` (each row pairs a `.harc` fixture with a vendored DUT
and asserts `ALL TESTS PASSED`).

## IP verification jobs — compiler provenance is mandatory

A verification job has two compiler-built halves, and **both must come from a
real compiler — never hand-write either artifact**:

1. **The DUT is an `arch`-compiler artifact.** Every `.sv` in `tests/dut/` is a
   vendored snapshot of `arch build` output from `arch-hdl-lang/arch-com`. Do
   NOT hand-write or hand-port a DUT `.sv`. The flow is:
   - Author the IP's `.arch` in `arch-com` and build it there (`arch build`),
     committing the generated `.sv` next to the source.
   - Vendor it here by adding a `copy` line in `tests/dut/refresh.sh` and
     running that script (it copies arch-com's compiler output into
     `tests/dut/`). Record the source in `tests/dut/README.md`.

   A hand-written DUT means the test verifies your own SV against your own
   testbench — it proves nothing about whether the `.arch` source compiles or
   behaves correctly. If a DUT `.sv` here did not come from `arch build`, the
   job is incomplete.

2. **The testbench is a `harc`-compiler artifact.** Write the `.harc` fixture,
   register it in `tests/run_fixtures.sh`, and actually run it through `harc
   sim` until it reports `ALL TESTS PASSED`. A fixture that has never been run
   through `harc` is not verified — run it and fix what breaks before declaring
   done.

In short: **DUTs are built by `arch` in arch-com; testbenches are built and run
by `harc` here.** Don't substitute hand-written stand-ins for either.

## Commit conventions

**Do not add `Co-Authored-By:` trailers for AI agents** (Claude, Copilot, etc.)
when creating commits. The human author of record takes full ownership of the
change.
