//! ---
//! tags: [tlm_method, harc, conformance, bounded_burst]
//! refs:
//!   - "HARC/ARCH bounded Vec response pairing fixture"
//! ---
//!
//! ARCH target responder used by HARC conformance tests for canonical bounded
//! burst response payloads: fixed Vec data plus runtime length and response
//! code fields.
/// Canonical bounded burst response shared by ARCH and HARC tests.
typedef struct packed {
  logic [3:0] [31:0] data;
  logic [2:0] len;
  logic [1:0] resp;
} BurstResp32x4;

/// Burst-like memory read contract returning a bounded response struct.
/// Bring the pairing TLM bus into scope for module ports.
/// ARCH target that synthesizes a deterministic bounded burst response.
module TlmPairingArchBurstTarget (
  input logic clk,
  input logic rst,
  input logic mem_read_burst_req_valid,
  input logic [31:0] mem_read_burst_addr,
  input logic [2:0] mem_read_burst_len,
  output logic mem_read_burst_req_ready,
  output logic mem_read_burst_rsp_valid,
  output BurstResp32x4 mem_read_burst_rsp_data,
  input logic mem_read_burst_rsp_ready,
  output logic [7:0] read_count,
  output logic [31:0] last_addr,
  output logic [2:0] last_len
);

  logic [7:0] read_count_r;
  logic [31:0] last_addr_r;
  logic [2:0] last_len_r;
  BurstResp32x4 rsp_r;
  assign read_count = read_count_r;
  assign last_addr = last_addr_r;
  assign last_len = last_len_r;
  logic [1:0] _tlm_mem_read_burst_state;
  logic [31:0] _tlm_mem_read_burst_addr_latched;
  logic [2:0] _tlm_mem_read_burst_len_latched;
  always_ff @(posedge clk) begin
    if (rst) begin
      _tlm_mem_read_burst_addr_latched <= 0;
      _tlm_mem_read_burst_len_latched <= 0;
      _tlm_mem_read_burst_state <= 0;
      last_addr_r <= 0;
      last_len_r <= 0;
      read_count_r <= 0;
      rsp_r <= 0;
    end else begin
      if (_tlm_mem_read_burst_state == 2'd0 && mem_read_burst_req_valid) begin
        _tlm_mem_read_burst_addr_latched <= mem_read_burst_addr;
        _tlm_mem_read_burst_len_latched <= mem_read_burst_len;
        _tlm_mem_read_burst_state <= 2'd1;
      end
      if (_tlm_mem_read_burst_state == 2'd1 && 1'b1) begin
        read_count_r <= 8'(read_count_r + 8'd1);
        last_addr_r <= _tlm_mem_read_burst_addr_latched;
        last_len_r <= _tlm_mem_read_burst_len_latched;
        rsp_r.data[0] <= 32'd32;
        rsp_r.data[1] <= 32'd36;
        rsp_r.data[2] <= 32'd40;
        rsp_r.data[3] <= 32'd44;
        rsp_r.len <= 3'd3;
        rsp_r.resp <= 2'd0;
        _tlm_mem_read_burst_state <= 2'd2;
      end
      if (_tlm_mem_read_burst_state == 2'd2 && mem_read_burst_rsp_ready) begin
        _tlm_mem_read_burst_state <= 2'd0;
      end
    end
  end
  always_comb begin
    mem_read_burst_req_ready = _tlm_mem_read_burst_state == 2'd0;
    mem_read_burst_rsp_valid = _tlm_mem_read_burst_state == 2'd2;
    mem_read_burst_rsp_data = rsp_r;
    if (_tlm_mem_read_burst_state == 2'd2) begin
      mem_read_burst_rsp_data = rsp_r;
    end
  end
  
  // synopsys translate_off
  // Auto-generated TLM method protocol assertions
  _auto_tlm_mem_read_burst_req_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_burst_req_valid && !mem_read_burst_req_ready) |=> (mem_read_burst_req_valid && $stable(mem_read_burst_addr) && $stable(mem_read_burst_len)))
    else $fatal(1, "TLM VIOLATION (request changed while stalled): TlmPairingArchBurstTarget._auto_tlm_mem_read_burst_req_stable");
  _auto_tlm_mem_read_burst_rsp_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_burst_rsp_valid && !mem_read_burst_rsp_ready) |=> (mem_read_burst_rsp_valid && $stable(mem_read_burst_rsp_data)))
    else $fatal(1, "TLM VIOLATION (response changed while stalled): TlmPairingArchBurstTarget._auto_tlm_mem_read_burst_rsp_stable");
  // synopsys translate_on

endmodule

