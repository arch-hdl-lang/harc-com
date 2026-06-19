module BusRwBind (
  input logic clk,
  input logic rst,
  input logic s_ar_valid,
  output logic s_ar_ready,
  input logic [31:0] s_ar_addr,
  output logic s_r_valid,
  input logic s_r_ready,
  output logic [31:0] s_r_data,
  output logic busy
);

  assign s_ar_ready = 1;
  assign s_r_valid = s_ar_valid;
  assign s_r_data = s_ar_addr;
  assign busy = s_ar_valid;

endmodule

