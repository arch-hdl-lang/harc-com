// SystemVerilog reference implementation of DotProductEngine.
// Matches arch-com/examples/dot_product_engine.arch.
//
// 8-element signed INT8 dot product engine.
// result = a0*b0 + a1*b1 + ... + a7*b7, one-cycle registered output.
// valid_out rises one cycle after valid_in; result holds the product.

module DotProductEngine (
  input  wire               clk,
  input  wire               rst,
  input  wire signed [7:0]  a0, a1, a2, a3, a4, a5, a6, a7,
  input  wire signed [7:0]  b0, b1, b2, b3, b4, b5, b6, b7,
  input  wire               valid_in,
  output logic signed [23:0] result,
  output logic               valid_out
);

  // Signed products: SInt<8> × SInt<8> → SInt<16> (IEEE 1800-2012 §11.6)
  wire signed [15:0] p0 = a0 * b0;
  wire signed [15:0] p1 = a1 * b1;
  wire signed [15:0] p2 = a2 * b2;
  wire signed [15:0] p3 = a3 * b3;
  wire signed [15:0] p4 = a4 * b4;
  wire signed [15:0] p5 = a5 * b5;
  wire signed [15:0] p6 = a6 * b6;
  wire signed [15:0] p7 = a7 * b7;

  // Adder tree — sign-extend each 16-bit product to 24 bits then sum pairwise.
  // Max total magnitude = 8 × 127 × 127 = 129,032 — no overflow in SInt<24>.
  wire signed [23:0] sum01   = $signed({{8{p0[15]}}, p0}) + $signed({{8{p1[15]}}, p1});
  wire signed [23:0] sum23   = $signed({{8{p2[15]}}, p2}) + $signed({{8{p3[15]}}, p3});
  wire signed [23:0] sum45   = $signed({{8{p4[15]}}, p4}) + $signed({{8{p5[15]}}, p5});
  wire signed [23:0] sum67   = $signed({{8{p6[15]}}, p6}) + $signed({{8{p7[15]}}, p7});
  wire signed [23:0] sum0123 = sum01 + sum23;
  wire signed [23:0] sum4567 = sum45 + sum67;
  wire signed [23:0] dot     = sum0123 + sum4567;

  // Pipeline registers — 1-cycle latency.
  logic signed [23:0] result_r;
  logic               valid_r;

  always_ff @(posedge clk) begin
    if (rst) begin
      result_r <= '0;
      valid_r  <= 1'b0;
    end else begin
      result_r <= dot;
      valid_r  <= valid_in;
    end
  end

  assign result    = result_r;
  assign valid_out = valid_r;

endmodule
