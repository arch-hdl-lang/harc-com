//! ---
//! tags: [tlm_method, harc, conformance]
//! refs:
//!   - "HARC TLM pairing conformance fixture"
//! ---
//!
//! ARCH TLM initiator used by HARC conformance tests. HARC binds a target
//! responder to the canonical method pins and serves the ARCH thread-lowered
//! read/write calls through the same synthesizable transaction contract.
/// Small memory-like TLM bus shared by ARCH and HARC pairing tests.
/// Bring the pairing TLM bus into scope for module ports.
/// ARCH initiator that issues serialized read/write TLM calls.
module TlmPairingArchInitiator (
  input logic clk,
  input logic rst,
  input logic start,
  output logic mem_read_req_valid,
  output logic [7:0] mem_read_addr,
  input logic mem_read_req_ready,
  input logic mem_read_rsp_valid,
  input logic [31:0] mem_read_rsp_data,
  output logic mem_read_rsp_ready,
  output logic mem_write_req_valid,
  output logic [7:0] mem_write_addr,
  output logic [31:0] mem_write_data,
  input logic mem_write_req_ready,
  input logic mem_write_rsp_valid,
  input logic mem_write_rsp_data,
  output logic mem_write_rsp_ready,
  output logic done,
  output logic [31:0] read0,
  output logic [31:0] read1,
  output logic write_ack
);

  logic done_r;
  logic [31:0] read0_r;
  logic [31:0] read1_r;
  logic write_ack_r;
  assign done = done_r;
  assign read0 = read0_r;
  assign read1 = read1_r;
  assign write_ack = write_ack_r;
  logic [3:0] _tlm_init_driver_state;
  logic _tlm_init_mem_read_want_0;
  logic _tlm_init_mem_read_want_1;
  logic _tlm_init_mem_read_taken_0;
  logic _tlm_init_mem_read_grant_0;
  logic _tlm_init_mem_read_grant_1;
  logic _tlm_init_mem_write_want_0;
  logic _tlm_init_mem_write_grant_0;
  always_ff @(posedge clk) begin
    if (rst) begin
      _tlm_init_driver_state <= 0;
      done_r <= 1'b0;
      read0_r <= 0;
      read1_r <= 0;
      write_ack_r <= 1'b0;
    end else begin
      if (_tlm_init_driver_state == 4'd0) begin
        if (start && !done_r) begin
          _tlm_init_driver_state <= 4'd1;
        end else begin
          _tlm_init_driver_state <= 4'd8;
        end
      end
      if (_tlm_init_driver_state == 4'd2 && mem_read_rsp_valid) begin
        read0_r <= mem_read_rsp_data;
        _tlm_init_driver_state <= 4'd3;
      end
      if (_tlm_init_driver_state == 4'd4 && mem_write_rsp_valid) begin
        write_ack_r <= mem_write_rsp_data;
        _tlm_init_driver_state <= 4'd5;
      end
      if (_tlm_init_driver_state == 4'd6 && mem_read_rsp_valid) begin
        read1_r <= mem_read_rsp_data;
        _tlm_init_driver_state <= 4'd7;
      end
      if (_tlm_init_driver_state == 4'd7) begin
        done_r <= 1'b1;
        _tlm_init_driver_state <= 4'd8;
      end
      if (_tlm_init_driver_state == 4'd8) begin
        _tlm_init_driver_state <= 4'd0;
      end
      if (_tlm_init_mem_read_grant_0 && mem_read_req_ready) begin
        _tlm_init_driver_state <= 4'd2;
      end
      if (_tlm_init_mem_read_grant_1 && mem_read_req_ready) begin
        _tlm_init_driver_state <= 4'd6;
      end
      if (_tlm_init_mem_write_grant_0 && mem_write_req_ready) begin
        _tlm_init_driver_state <= 4'd4;
      end
    end
  end
  assign _tlm_init_mem_read_want_0 = _tlm_init_driver_state == 4'd1;
  assign _tlm_init_mem_read_want_1 = _tlm_init_driver_state == 4'd5;
  assign _tlm_init_mem_read_taken_0 = 1'b0 || _tlm_init_mem_read_want_0;
  assign _tlm_init_mem_read_grant_0 = _tlm_init_mem_read_want_0 && !1'b0;
  assign _tlm_init_mem_read_grant_1 = _tlm_init_mem_read_want_1 && !_tlm_init_mem_read_taken_0;
  assign mem_read_req_valid = _tlm_init_mem_read_grant_0 || _tlm_init_mem_read_grant_1;
  assign mem_read_addr = _tlm_init_mem_read_grant_0 ? 8'd5 : _tlm_init_mem_read_grant_1 ? 8'd7 : 0;
  assign mem_read_rsp_ready = _tlm_init_driver_state == 4'd2 || _tlm_init_driver_state == 4'd6;
  assign _tlm_init_mem_write_want_0 = _tlm_init_driver_state == 4'd3;
  assign _tlm_init_mem_write_grant_0 = _tlm_init_mem_write_want_0 && !1'b0;
  assign mem_write_req_valid = _tlm_init_mem_write_grant_0;
  assign mem_write_addr = _tlm_init_mem_write_grant_0 ? 8'd9 : 0;
  assign mem_write_data = _tlm_init_mem_write_grant_0 ? 32'd2882400009 : 0;
  assign mem_write_rsp_ready = _tlm_init_driver_state == 4'd4;

  // synopsys translate_off
  // Auto-generated TLM method protocol assertions
  _auto_tlm_mem_read_req_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_req_valid && !mem_read_req_ready) |=> (mem_read_req_valid && $stable(mem_read_addr)))
    else $fatal(1, "TLM VIOLATION (request changed while stalled): TlmPairingArchInitiator._auto_tlm_mem_read_req_stable");
  _auto_tlm_mem_read_rsp_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_rsp_valid && !mem_read_rsp_ready) |=> (mem_read_rsp_valid && $stable(mem_read_rsp_data)))
    else $fatal(1, "TLM VIOLATION (response changed while stalled): TlmPairingArchInitiator._auto_tlm_mem_read_rsp_stable");
  _auto_tlm_mem_write_req_stable: assert property (@(posedge clk) disable iff (rst) (mem_write_req_valid && !mem_write_req_ready) |=> (mem_write_req_valid && $stable(mem_write_addr) && $stable(mem_write_data)))
    else $fatal(1, "TLM VIOLATION (request changed while stalled): TlmPairingArchInitiator._auto_tlm_mem_write_req_stable");
  _auto_tlm_mem_write_rsp_stable: assert property (@(posedge clk) disable iff (rst) (mem_write_rsp_valid && !mem_write_rsp_ready) |=> (mem_write_rsp_valid && $stable(mem_write_rsp_data)))
    else $fatal(1, "TLM VIOLATION (response changed while stalled): TlmPairingArchInitiator._auto_tlm_mem_write_rsp_stable");
  // synopsys translate_on

endmodule
