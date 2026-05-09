// HARC test wrapper around AesCipherTop.
//
// AesCipherTop has 128-bit `key`, `text_in`, and `text_out` ports.
// HARC's v0 testbench codegen lowers DUT signals as scalar
// `uint64_t` accesses, which doesn't compose with Verilator's
// `VlWide<4>` representation for >64-bit ports. To keep the AES
// fixture inside HARC's current scalar-only envelope, this wrapper
// exposes the same module behavior through 4 x 32-bit ports per
// 128-bit value (MSB-first: word3 = bits[127:96], word0 = bits[31:0]).
//
// The wrapper is purely structural — no extra registers, no extra
// latency. AesCipherTop's internal pipeline is unchanged.

module AesCipherTopWrap (
  input logic clk,
  input logic rst,
  input logic ld,
  output logic done,
  // Key, MSB-first 32-bit words.
  input logic [31:0] key3,
  input logic [31:0] key2,
  input logic [31:0] key1,
  input logic [31:0] key0,
  // Plaintext, MSB-first 32-bit words.
  input logic [31:0] text_in3,
  input logic [31:0] text_in2,
  input logic [31:0] text_in1,
  input logic [31:0] text_in0,
  // Ciphertext, MSB-first 32-bit words.
  output logic [31:0] text_out3,
  output logic [31:0] text_out2,
  output logic [31:0] text_out1,
  output logic [31:0] text_out0
);

  logic [127:0] key_w;
  logic [127:0] text_in_w;
  logic [127:0] text_out_w;

  assign key_w     = {key3,     key2,     key1,     key0};
  assign text_in_w = {text_in3, text_in2, text_in1, text_in0};

  assign text_out3 = text_out_w[127:96];
  assign text_out2 = text_out_w[95:64];
  assign text_out1 = text_out_w[63:32];
  assign text_out0 = text_out_w[31:0];

  AesCipherTop u_aes (
    .clk(clk),
    .rst(rst),
    .ld(ld),
    .done(done),
    .key(key_w),
    .text_in(text_in_w),
    .text_out(text_out_w)
  );

endmodule
