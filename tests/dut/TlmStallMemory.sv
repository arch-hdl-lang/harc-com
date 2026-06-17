// Stall DUT for the TLM wait-timeout diagnostic e2e test
// (tests/tlm_wait_timeout_e2e.rs, harc-com#416).
//
// The `read` target NEVER asserts `req_ready` or `rsp_valid`, so a blocking
// initiator's bounded handshake waits both expire. Before #416 those loops
// fell through silently; now each emits a structured FAIL diagnostic. This
// DUT exists to make that diagnostic fire under a real Verilator run.
module TlmStallMemory (
  input  logic        clk,
  input  logic        rst,

  input  logic        mem_read_req_valid,
  input  logic [7:0]  mem_read_addr,
  output logic        mem_read_req_ready,
  output logic        mem_read_rsp_valid,
  output logic [31:0] mem_read_rsp_data,
  input  logic        mem_read_rsp_ready
);
  // Stuck target: never accept a request, never produce a response.
  assign mem_read_req_ready = 1'b0;
  assign mem_read_rsp_valid = 1'b0;
  assign mem_read_rsp_data  = 32'h0;

  // Consume the otherwise-unused inputs so Verilator stays quiet.
  wire _unused = &{1'b0, clk, rst, mem_read_req_valid, mem_read_addr,
                   mem_read_rsp_ready};
endmodule
