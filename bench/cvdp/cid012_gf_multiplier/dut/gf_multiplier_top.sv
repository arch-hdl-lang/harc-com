`timescale 1ns/1ps

// Thin Verilator-driving wrapper around `gf_multiplier`.
//
// The CVDP cid012 problem ships `gf_multiplier` as a purely
// combinational module (inputs A, B; output result; no clock).
// HARC's `harc sim` codegen drives a primary `dut->clk` posedge
// per cycle, so to keep the DUT untouched we expose a phantom
// `clk` input on this wrapper. Combinational outputs of the inner
// `gf_multiplier` re-settle on every `eval()` regardless of the
// clock value, so the wrapper is semantics-preserving.
//
// Phase 2 (clockless-DUT detection in HARC codegen) will let
// `gf_multiplier` be the top directly. For Phase 1 plumbing we
// just wrap.
module gf_multiplier_top (
    input  wire        clk,        // unused; drives HARC's posedge loop
    input  wire [3:0]  A,
    input  wire [3:0]  B,
    output wire [3:0]  result
);
    gf_multiplier u_dut (
        .A      (A),
        .B      (B),
        .result (result)
    );
endmodule
