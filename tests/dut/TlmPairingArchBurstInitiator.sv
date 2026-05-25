//! ---
//! tags: [tlm_method, harc, conformance, bounded_burst]
//! refs:
//!   - "HARC/ARCH bounded Vec response pairing fixture"
//! ---
//!
//! ARCH initiator used by HARC conformance tests for canonical bounded burst
//! response payloads served by a HARC target transactor.
/// Canonical bounded burst response shared by ARCH and HARC tests.
typedef struct packed {
  logic [3:0] [31:0] data;
  logic [2:0] len;
  logic [1:0] resp;
} BurstResp32x4;

/// Burst-like memory read contract returning a bounded response struct.
/// Bring the pairing TLM bus into scope for module ports.
/// ARCH initiator that captures a bounded burst response from HARC.
module TlmPairingArchBurstInitiator (
  input logic clk,
  input logic rst,
  input logic start,
  output logic mem_read_burst_req_valid,
  output logic [31:0] mem_read_burst_addr,
  output logic [2:0] mem_read_burst_len,
  input logic mem_read_burst_req_ready,
  input logic mem_read_burst_rsp_valid,
  input BurstResp32x4 mem_read_burst_rsp_data,
  output logic mem_read_burst_rsp_ready,
  output logic done,
  output logic [31:0] data0,
  output logic [31:0] data1,
  output logic [31:0] data2,
  output logic [31:0] data3,
  output logic [2:0] len_o,
  output logic [1:0] resp_o
);

  logic done_r;
  BurstResp32x4 rsp_r;
  logic [2:0] _tlm_init_issue_state;
  logic _tlm_init_mem_read_burst_want_0;
  logic _tlm_init_mem_read_burst_grant_0;
  assign done = done_r;
  assign data0 = rsp_r.data[0];
  assign data1 = rsp_r.data[1];
  assign data2 = rsp_r.data[2];
  assign data3 = rsp_r.data[3];
  assign len_o = rsp_r.len;
  assign resp_o = rsp_r.resp;
  always_ff @(posedge clk) begin
    if (rst) begin
      _tlm_init_issue_state <= 0;
      done_r <= 1'b0;
      rsp_r <= 0;
    end else begin
      if (_tlm_init_issue_state == 3'd0) begin
        if (start && !done_r) begin
          _tlm_init_issue_state <= 3'd1;
        end else begin
          _tlm_init_issue_state <= 3'd4;
        end
      end
      if (_tlm_init_issue_state == 3'd2 && mem_read_burst_rsp_valid) begin
        rsp_r <= mem_read_burst_rsp_data;
        _tlm_init_issue_state <= 3'd3;
      end
      if (_tlm_init_issue_state == 3'd3) begin
        done_r <= 1'b1;
        _tlm_init_issue_state <= 3'd4;
      end
      if (_tlm_init_issue_state == 3'd4) begin
        _tlm_init_issue_state <= 3'd0;
      end
      if (_tlm_init_mem_read_burst_grant_0 && mem_read_burst_req_ready) begin
        _tlm_init_issue_state <= 3'd2;
      end
    end
  end
  assign _tlm_init_mem_read_burst_want_0 = _tlm_init_issue_state == 3'd1;
  assign _tlm_init_mem_read_burst_grant_0 = _tlm_init_mem_read_burst_want_0 && !1'b0;
  assign mem_read_burst_req_valid = _tlm_init_mem_read_burst_grant_0;
  assign mem_read_burst_addr = _tlm_init_mem_read_burst_grant_0 ? 32'd64 : 0;
  assign mem_read_burst_len = _tlm_init_mem_read_burst_grant_0 ? 3'd4 : 0;
  assign mem_read_burst_rsp_ready = _tlm_init_issue_state == 3'd2;
  
  // synopsys translate_off
  // Auto-generated TLM method protocol assertions
  _auto_tlm_mem_read_burst_req_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_burst_req_valid && !mem_read_burst_req_ready) |=> (mem_read_burst_req_valid && $stable(mem_read_burst_addr) && $stable(mem_read_burst_len)))
    else $fatal(1, "TLM VIOLATION (request changed while stalled): TlmPairingArchBurstInitiator._auto_tlm_mem_read_burst_req_stable");
  _auto_tlm_mem_read_burst_rsp_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_burst_rsp_valid && !mem_read_burst_rsp_ready) |=> (mem_read_burst_rsp_valid && $stable(mem_read_burst_rsp_data)))
    else $fatal(1, "TLM VIOLATION (response changed while stalled): TlmPairingArchBurstInitiator._auto_tlm_mem_read_burst_rsp_stable");
  // synopsys translate_on

endmodule

