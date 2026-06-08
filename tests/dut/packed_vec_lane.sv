// Regression DUT for the packed `Vec<Bus, N>` lane-access TB-codegen fix.
//
// Mirrors what `arch build` emits for multi-lane bus ports at a fabric
// top: N-lane ports flatten to PACKED SystemVerilog vectors that the SV
// simulator exposes as single packed C++ scalars (or a wide word array
// once the total width exceeds 64b). A HARC test indexing
// `dut.<port>[i]` must bit-extract / bit-deposit per lane on `--sv`.
//
// Three port shapes are exercised on purpose:
//   * `lane_valid`  — 1-bit lane, single-dimension packed `[N-1:0]`.
//   * `lane_id`     — multi-bit lane, 2-D packed `[N-1:0][W-1:0]` (W=5).
//   * `lane_data`   — wide multi-bit lane, total width 3*32 = 96b → the
//                     SV-sim port is a wide word array (`VlWide<3>`),
//                     exercising the word-array branch of the lane helpers.
//   * `pass_uvec`   — a TRUE SystemVerilog *unpacked* array port. The SV
//                     sim keeps this as a C++ array, so `dut.pass_uvec[i]`
//                     must KEEP plain array indexing (NOT bit-extract).
//
// Comb behaviour: each output lane echoes the matching input lane plus a
// per-port constant, so the TB can prove per-lane addressing end-to-end.
module PackedVecLane #(
    parameter int N = 3,
    parameter int W = 5
) (
    input  logic                 clk,
    input  logic                 rst,
    // 1-bit lane (single packed dimension).
    input  logic [N-1:0]         lane_valid_in,
    output logic [N-1:0]         lane_valid_out,
    // W-bit lane (2-D packed).
    input  logic [N-1:0][W-1:0]  lane_id_in,
    output logic [N-1:0][W-1:0]  lane_id_out,
    // 32-bit lane (2-D packed, > 64b total → VlWide).
    input  logic [N-1:0][31:0]   lane_data_in,
    output logic [N-1:0][31:0]   lane_data_out,
    // Genuine UNPACKED array port — must stay array-indexed on both
    // backends (no bit-extraction).
    input  logic [7:0]           pass_uvec_in  [N],
    output logic [7:0]           pass_uvec_out [N]
);
    always_comb begin
        for (int i = 0; i < N; i++) begin
            lane_valid_out[i] = lane_valid_in[i];
            lane_id_out[i]    = lane_id_in[i] + W'(1);
            lane_data_out[i]  = lane_data_in[i] + 32'(i);
            pass_uvec_out[i]  = pass_uvec_in[i] + 8'(2);
        end
    end
endmodule
