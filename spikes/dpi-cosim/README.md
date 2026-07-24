# DPI-C co-sim spike

Working prototype of the spec §10 "simulator owns time" co-sim contract:
the `sync_fifo_test` run block, hand-lowered onto the production
`harc_rt::ThreadScheduler`, driven by an HDL simulator through the
C entrypoints `harc_init` / `harc_on_posedge` / `harc_finish` and the
id-keyed signal-accessor ABI `harc_dut_get` / `harc_dut_set`.

Two builds of the same TB core:

- **Verilator** (`run_verilator.sh`) — real DPI-C imports/exports,
  `--binary --timing`, SV harness owns the clock.
- **Icarus Verilog** (`run_icarus.sh`) — Icarus has no DPI-C; the same
  C ABI is reached through a standard VPI bridge (`vpi_adapter.c`)
  compiled with the unchanged TB core into a `.vpi` module.

Full findings, throughput numbers, and the proposed compiler
integration: `docs/2026-07-24-dpi-cosim-exploration.md`.

```sh
./run_verilator.sh                        # needs verilator >= 5.x
./run_icarus.sh                           # needs iverilog + vvp (11/12)
HARC_COSIM_SOAK=1000000 ./run_verilator.sh   # throughput soak
HARC_COSIM_DEBUG=1 ./run_icarus.sh           # per-cycle signal dump
```

Both scripts fail unless the sim prints `ALL TESTS PASSED`.
