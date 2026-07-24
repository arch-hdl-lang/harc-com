// Co-sim harness for the spec §10 DPI-C pilot: the simulator owns time,
// HARC is a passive runtime called from generated hook points.
//
// In the eventual compiler integration this file is generated from the
// DUT port list. Two flavors, selected at compile time:
//
//   default          — DPI-C (Verilator, and the VCS/Xcelium/Questa
//                      commercial path). Imports harc_init /
//                      harc_on_posedge / harc_finish, exports the typed
//                      signal accessors harc_sv_get / harc_sv_set.
//   +define+HARC_COSIM_VPI — Icarus Verilog, which has no DPI-C.
//                      The same C entrypoints are reached through
//                      system tasks provided by vpi_adapter.c, and
//                      signal access goes through VPI handles instead
//                      of exported functions. The HARC-side C ABI is
//                      identical.
//
// Hook-point placement: harc_on_posedge represents the direct backend's
// mid-cycle coroutine quantum (posedge eval -> sched.tick() -> negedge
// eval). In simulator-owned time the equivalent stable point is the
// negedge event: FFs latched at the posedge half a cycle ago,
// combinational logic has settled, and TB drives land a half cycle
// before the next posedge — no active-region race with the DUT's
// always_ff blocks.
`timescale 1ns / 1ps

module HarcCosimTop;

  // Keep in sync with harc_cosim_sig_ids.h.
  localparam int SigRst = 0;
  localparam int SigPushValid = 1;
  localparam int SigPushData = 2;
  localparam int SigPopReady = 3;
  localparam int SigPushReady = 4;
  localparam int SigPopValid = 5;
  localparam int SigPopData = 6;
  localparam int SigFull = 7;
  localparam int SigEmpty = 8;

  logic clk;
  logic rst;
  logic push_valid;
  logic [7:0] push_data;
  logic pop_ready;
  logic push_ready;
  logic pop_valid;
  logic [7:0] pop_data;
  logic full;
  logic empty;

  TxQueue #(
      .DEPTH(16),
      .DATA_WIDTH(8)
  ) dut (
      .clk(clk),
      .rst(rst),
      .push_valid(push_valid),
      .push_ready(push_ready),
      .push_data(push_data),
      .pop_valid(pop_valid),
      .pop_ready(pop_ready),
      .pop_data(pop_data),
      .full(full),
      .empty(empty)
  );

`ifndef HARC_COSIM_VPI
  // ---- DPI-C flavor (Verilator / commercial simulators) ----
  import "DPI-C" context function void harc_init();
  import "DPI-C" context function int harc_on_posedge();
  import "DPI-C" context function void harc_finish();

  export "DPI-C" function harc_sv_get;
  export "DPI-C" function harc_sv_set;

  // Typed accessors (spec §10 contract point 3): the HARC runtime never
  // sees hierarchical paths, only these generated functions.
  function longint harc_sv_get(input int sig_id);
    case (sig_id)
      SigRst: return longint'(rst);
      SigPushValid: return longint'(push_valid);
      SigPushData: return longint'(push_data);
      SigPopReady: return longint'(pop_ready);
      SigPushReady: return longint'(push_ready);
      SigPopValid: return longint'(pop_valid);
      SigPopData: return longint'(pop_data);
      SigFull: return longint'(full);
      SigEmpty: return longint'(empty);
      default: return 0;
    endcase
  endfunction

  function void harc_sv_set(input int sig_id, input longint value);
    case (sig_id)
      SigRst: rst = value[0];
      SigPushValid: push_valid = value[0];
      SigPushData: push_data = value[7:0];
      SigPopReady: pop_ready = value[0];
      default: ;
    endcase
  endfunction

`ifdef HARC_COSIM_DUMP
  initial begin
    $dumpfile("cosim.vcd");
    $dumpvars(0, HarcCosimTop);
  end
`endif

  // One timed master process: clock generation AND the per-cycle HARC
  // hook. The obvious alternative — `forever #5 clk = ~clk` plus a
  // separate `always @(negedge clk)` block calling harc_on_posedge() —
  // is broken on Verilator 5.020: the context-import call expression is
  // evaluated in more than one scheduling region, so harc_on_posedge()
  // fires TWICE per negedge event (verified empirically; the block's
  // own $display fires once). The coroutine then advances two quanta
  // per clock and every other drive is overwritten before the DUT
  // samples it. A `--timing` process is a coroutine resumed exactly
  // once per delay, so putting the call inline after the negedge
  // assignment guarantees once-per-cycle semantics. Event-driven
  // simulators execute either shape once; the generated harness should
  // use this one everywhere.
  initial begin
    // Time zero, before the first posedge: HARC constructs its
    // coroutines and runs setup drives (reset, input defaults) via the
    // exported setters.
    harc_init();
    clk = 0;
    forever begin
      #5 clk = 1;
      #5 clk = 0;
      if (harc_on_posedge() != 0) begin
        harc_finish();
        $finish;
      end
    end
  end
`else
  // ---- VPI flavor (Icarus) ----
  // Same contract, same C entrypoints; the bridge from SV events to the
  // HARC runtime is vpi_adapter.c's system tasks, and signal access is
  // VPI handles resolved once at $harc_init time.
  // Same single-master-process shape as the DPI flavor (and it also
  // sidesteps Icarus's X->0 transition at time zero registering as a
  // negedge). $harc_tick calls harc_on_posedge() and, when it reports
  // done, calls harc_finish() and stops the simulation from the C side
  // via vpi_control(vpiFinish).
  initial begin
    $harc_init;
    clk = 0;
    forever begin
      #5 clk = 1;
      #5 clk = 0;
      $harc_tick;
    end
  end
`endif

endmodule
