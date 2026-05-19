module matrix_multiplier_tlm_top (
  input  logic         clk,
  input  logic         srst,
  input  logic         valid_in,
  input  logic [127:0] matrix_a,
  input  logic [127:0] matrix_b,
  output logic         valid_out,
  output logic [287:0] matrix_c,

  input  logic         mul_req_valid,
  input  logic [127:0] mul_a,
  input  logic [127:0] mul_b,
  output logic         mul_req_ready,
  output logic         mul_rsp_valid,
  output logic [63:0]  mul_rsp_data,
  input  logic         mul_rsp_ready
);
  logic         inner_valid_in;
  logic [127:0] inner_matrix_a;
  logic [127:0] inner_matrix_b;

  assign mul_req_ready = 1'b1;
  assign mul_rsp_valid = valid_out;
  assign mul_rsp_data = matrix_c[63:0];

  assign inner_valid_in = mul_req_valid ? 1'b1 : valid_in;
  assign inner_matrix_a = mul_req_valid ? mul_a : matrix_a;
  assign inner_matrix_b = mul_req_valid ? mul_b : matrix_b;

  matrix_multiplier dut_i (
    .clk(clk),
    .srst(srst),
    .valid_in(inner_valid_in),
    .matrix_a(inner_matrix_a),
    .matrix_b(inner_matrix_b),
    .valid_out(valid_out),
    .matrix_c(matrix_c)
  );

  // Consume is observed only by the HARC TLM adapter. The wrapped CVDP DUT has
  // a fixed-latency valid_out pulse and no downstream backpressure.
  wire _unused_rsp_ready = mul_rsp_ready;
endmodule
