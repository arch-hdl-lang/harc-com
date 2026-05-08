# Vendored DUTs for the integration test suite

These SystemVerilog files are snapshots of the corresponding modules in [`arch-hdl-lang/arch-com`](https://github.com/arch-hdl-lang/arch-com). They're vendored here so HARC's CI can run end-to-end Verilator simulations without depending on a sibling clone of arch-com.

| DUT | Source |
|---|---|
| `rom_lut.sv` | `arch-com/examples/rom_lut.sv` |
| `bus_arbiter.sv` | `arch-com/examples/bus_arbiter.sv` |
| `traffic_light.sv` | `arch-com/examples/traffic_light.sv` |
| `sync_fifo.sv` | `arch-com/examples/sync_fifo.sv` |
| `pipe_reg_test.sv` | `arch-com/examples/pipe_reg_test.sv` |
| `single_port_ram.sv` | `arch-com/examples/single_port_ram.sv` |
| `synchronizer_basic.sv` | `arch-com/examples/synchronizer_basic.sv` |
| `synchronizer_pulse.sv` | `arch-com/examples/synchronizer_pulse.sv` |
| `synchronizer_gray.sv` | `arch-com/examples/synchronizer_gray.sv` |
| `synchronizer_handshake.sv` | `arch-com/examples/synchronizer_handshake.sv` |
| `synchronizer_reset.sv` | `arch-com/examples/synchronizer_reset.sv` |
| `synchronizer_wide.sv` | `arch-com/examples/synchronizer_wide.sv` |
| `async_fifo.sv` | `arch-com/examples/async_fifo.sv` |
| `multi_clock.sv` | `arch-com/examples/multi_clock.sv` |
| `clk_div_counter.sv` + `clk_divider.sv` | `arch-com/examples/clk_div_counter.sv` + `clk_divider.sv` |
| `top_counter.sv` | `arch-com/examples/top_counter.sv` |
| `fsm_counter.sv` | `arch-com/examples/fsm_counter.sv` |
| `int_regs.sv` | `arch-com/examples/int_regs.sv` |
| `pkt_queue.sv` | `arch-com/examples/pkt_queue.sv` |
| `linklist_basic.sv` | `arch-com/examples/linklist_basic.sv` |
| `cam_basic.sv` | `arch-com/tests/cam_basic.sv` |
| `AxiLiteRegs.sv` | `arch-com/tests/axi_dma/AxiLiteRegs.sv` |

## Refreshing

These are point-in-time snapshots. If the upstream `.arch` source changes and the SV emitter regenerates them, the vendored copies need to be re-synced. Run:

```sh
tests/dut/refresh.sh
```

(Run this from the harc-com repo root with arch-com checked out at `../arch-com`.)
