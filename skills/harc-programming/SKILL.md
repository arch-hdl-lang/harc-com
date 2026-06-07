---
name: harc-programming
description: Write, modify, debug, and verify HARC verification code. Use when working with `.harc` files, HARC tests, transactions, constraints, covergroups, scoreboards, transactors, sequencers, properties, reference models, `harc check`, `harc sim`, or HARC-to-DUT integration with ARCH or SystemVerilog.
---

# HARC Programming

Use this skill to produce HARC verification code and iterate it through the current HARC compiler and simulator. Use README/status tables, fixtures, parser behavior, and compiler results for shipped feature status; use `spec.md` for syntax intent and semantics, then verify advanced features before relying on them.

## Workflow

1. If HARC MCP tools are available, use them before hand-writing unfamiliar syntax:
   - Call `harc_feature_status()` before relying on advanced or pre-1.0 features.
   - Call `harc_examples()` to retrieve shipped fixture examples for the requested pattern.
   - Call `get_harc_syntax()` for targeted syntax snippets.
   - Validate edits with `harc_check()`.
   - Use `harc_sim_emit_only()` for backend/codegen validation without full simulation.
   - On compiler errors, call `harc_advise()` with the error keywords before attempting a fix.
2. If `tool_search` is available but HARC MCP tools are not loaded, search for `harc`. If MCP tools are still unavailable, use the CLI workflow below.
3. Read `references/README.md` first for current status, examples, CLI commands, and shipped features.
4. Read `references/spec.md` for language syntax and intended semantics. Verify roadmap-looking features through README status, fixtures, parser behavior, or compiler checks.
5. For simulator options, backend selection, waveforms, semantic traces, coverage, split compilation, or Z3 configuration, read `references/harc-sim-cli.md`.
6. For RAL/register-model work, read `references/ral-support.md`; trust its implementation-status table over aspirational prose.
7. For generated testbench architecture or IR/lowering behavior, read `references/tb-ir-design.md` only as design/RFC context because it is marked proposed, not shipped behavior.
8. For style and practical test authoring guidance, read `references/test-ergonomics.md`; trust its implementation-status table for shipped/partial/proposed boundaries.
9. Validate every non-trivial edit:

```sh
harc check <files.harc>
harc sim --sv <dut.sv> <test.harc> --top <Top>
harc sim --dut <dut.arch> <test.harc> --top <Top>
```

Use explicit Cargo commands from a `harc-com` checkout when `harc` is not on `PATH`:

```sh
cargo run --bin harc -- check <files.harc>
cargo run --bin harc -- sim --sv <dut.sv> <test.harc> --top <Top>
cargo run --bin harc -- sim --dut <dut.arch> <test.harc> --top <Top>
```

## Search Map

Use targeted search before loading large references:

```sh
rg -n "Status|Shipped|Partial|Proposed|roadmap|v0|fixtures|harc check|harc sim" references/README.md references/test-ergonomics.md references/ral-support.md
rg -n "^##|^###|transaction|covergroup|scoreboard|transactor|sequencer|testbench|impl|wait until|watchdog|randomize|keep|regblock|addrmap" references/spec.md
rg -n "emit-only|--sv|--dut|--top|--test|--seed|--waves|record-trace|coverage|Z3|check-backends" references/harc-sim-cli.md
```

Runnable fixture examples live in a local `harc-com` checkout under `tests/fixtures/` with DUTs under `tests/dut/`; those repo-local files are not bundled in this installed skill. Prefer fixture examples over prose when both are available.

Bundled reference files are snapshots copied from the same `harc-com` commit that contains this skill. Some links inside those snapshots remain repo-relative; use `references/SNAPSHOT.md` for provenance and packaging notes.

## Coding Rules

Use HARC for verification intent: tests, testbenches, transactions, constrained random stimulus, covergroups, scoreboards, reference-model comparisons, watchdogs, properties, and DUT binding. Keep design RTL in ARCH or SystemVerilog.

Choose the DUT backend explicitly. Use `harc sim --sv` for SystemVerilog DUTs through Verilator. Use `harc sim --dut` for ARCH-authored DUTs through `arch sim`. Keep the HARC test source backend-neutral unless backend-specific raw signal access is unavoidable.

Use `test` blocks for concrete tests. Put shared DUT setup, common checks, and teardown in a `testbench`, then bind tests with `impl <Test> for <Tb>` when multiple tests share infrastructure.

Prefer typed verification constructs over ad hoc loops: model packets or bus operations as `transaction`s, drive with transactors/sequencers when reusable, check final behavior with scoreboards, and record intent with covergroups and properties.

Use `wait until` with watchdogs or bounded waits for termination diagnostics. Avoid unbounded passive waits unless the surrounding test has a clear timeout path.

For constrained-random code, ensure Z3 is available and keep constraints relational and debuggable. Use explicit seeds (`--seed N` or `HARC_SEED`) when reproducing failures.

## Debugging

Start with `harc check` for parse and lint issues. On simulation failures, rerun with a fixed seed and add semantic traces or waves before changing test behavior:

```sh
harc sim --sv dut.sv test.harc --top Top --seed 1 --record-trace trace.jsonl --waves
harc sim --dut dut.arch test.harc --top Top --seed 1 --record-trace trace.jsonl --waves
```

Use `--emit-only` to inspect generated C++ without compiling or running. Use `--rebuild` after changing waveform or Verilator settings that may leave stale objects.

When comparing an ARCH DUT path and an SV DUT path, use `--check-backends` with both `--dut` and `--sv` so HARC runs the same test under both backends and diffs semantic traces.

## References

- `references/SNAPSHOT.md`: provenance, freshness, license, and repo-relative link notes for bundled references.
- `references/README.md`: project overview, install, examples, and top-level CLI.
- `references/spec.md`: full HARC language specification.
- `references/harc-sim-cli.md`: complete `harc sim` option reference.
- `references/semantic-trace.md`: semantic trace status and VCD merge notes.
- `references/ral-support.md`: register abstraction layer support.
- `references/tb-ir-design.md`: proposed/RFC generated testbench IR and lowering design; do not treat as shipped syntax.
- `references/test-ergonomics.md`: practical test authoring guidance.
- `references/LICENSE`: license text for the copied HARC reference material.
