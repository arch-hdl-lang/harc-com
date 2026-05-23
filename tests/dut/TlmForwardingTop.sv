module TlmForwardingTop (
  input  logic        clk,
  input  logic        rst,

  input  logic        front_read_req_ready,
  input  logic        front_read_rsp_valid,
  input  logic [31:0] front_read_rsp_data,
  output logic        front_read_req_valid,
  output logic [7:0]  front_read_addr,
  output logic        front_read_rsp_ready,

  input  logic        back_read_req_valid,
  input  logic [7:0]  back_read_addr,
  input  logic        back_read_rsp_ready,
  output logic        back_read_req_ready,
  output logic        back_read_rsp_valid,
  output logic [31:0] back_read_rsp_data,

  output logic        done,
  output logic [31:0] result0,
  output logic [31:0] result1
);
  logic unused_read_ooo_req_valid;
  logic [7:0] unused_read_ooo_addr;
  logic [0:0] unused_read_ooo_req_tag;
  logic unused_read_ooo_req_ready;
  logic unused_read_ooo_rsp_valid;
  logic [31:0] unused_read_ooo_rsp_data;
  logic [0:0] unused_read_ooo_rsp_tag;
  logic unused_read_ooo_rsp_ready;
  logic unused_poke_req_valid;
  logic [7:0] unused_poke_addr;
  logic [31:0] unused_poke_data;
  logic unused_poke_req_ready;
  logic unused_poke_rsp_valid;
  logic unused_poke_rsp_ready;

  assign unused_read_ooo_req_valid = 1'b0;
  assign unused_read_ooo_addr = 8'h00;
  assign unused_read_ooo_req_tag = 1'b0;
  assign unused_read_ooo_rsp_ready = 1'b0;
  assign unused_poke_req_valid = 1'b0;
  assign unused_poke_addr = 8'h00;
  assign unused_poke_data = 32'h0;
  assign unused_poke_rsp_ready = 1'b0;

  TlmReadInitiatorPair initiator (
    .clk(clk),
    .rst(rst),
    .mem_read_req_valid(front_read_req_valid),
    .mem_read_addr(front_read_addr),
    .mem_read_req_ready(front_read_req_ready),
    .mem_read_rsp_valid(front_read_rsp_valid),
    .mem_read_rsp_data(front_read_rsp_data),
    .mem_read_rsp_ready(front_read_rsp_ready),
    .done(done),
    .result0(result0),
    .result1(result1)
  );

  TlmMemory memory (
    .clk(clk),
    .rst(rst),
    .mem_read_req_valid(back_read_req_valid),
    .mem_read_addr(back_read_addr),
    .mem_read_req_ready(back_read_req_ready),
    .mem_read_rsp_valid(back_read_rsp_valid),
    .mem_read_rsp_data(back_read_rsp_data),
    .mem_read_rsp_ready(back_read_rsp_ready),
    .mem_read_ooo_req_valid(unused_read_ooo_req_valid),
    .mem_read_ooo_addr(unused_read_ooo_addr),
    .mem_read_ooo_req_tag(unused_read_ooo_req_tag),
    .mem_read_ooo_req_ready(unused_read_ooo_req_ready),
    .mem_read_ooo_rsp_valid(unused_read_ooo_rsp_valid),
    .mem_read_ooo_rsp_data(unused_read_ooo_rsp_data),
    .mem_read_ooo_rsp_tag(unused_read_ooo_rsp_tag),
    .mem_read_ooo_rsp_ready(unused_read_ooo_rsp_ready),
    .mem_poke_req_valid(unused_poke_req_valid),
    .mem_poke_addr(unused_poke_addr),
    .mem_poke_data(unused_poke_data),
    .mem_poke_req_ready(unused_poke_req_ready),
    .mem_poke_rsp_valid(unused_poke_rsp_valid),
    .mem_poke_rsp_ready(unused_poke_rsp_ready)
  );
endmodule
