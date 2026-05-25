//! ---
//! tags: [tlm_method, harc, conformance]
//! refs:
//!   - "HARC TLM pairing conformance fixture"
//! ---
//!
//! ARCH target responder used by HARC conformance tests. The module exposes
//! canonical TLM req/rsp pins after lowering so HARC can bind the same method
//! contract through either arch sim or Verilator-built SystemVerilog.
/// Small memory-like TLM bus shared by ARCH and HARC pairing tests.
/// Bring the pairing TLM bus into scope for module ports.
/// ARCH target responder for HARC initiator-side TLM calls.
module TlmPairingArchTarget (
  input logic clk,
  input logic rst,
  input logic mem_read_req_valid,
  input logic [7:0] mem_read_addr,
  output logic mem_read_req_ready,
  output logic mem_read_rsp_valid,
  output logic [31:0] mem_read_rsp_data,
  input logic mem_read_rsp_ready,
  input logic mem_write_req_valid,
  input logic [7:0] mem_write_addr,
  input logic [31:0] mem_write_data,
  output logic mem_write_req_ready,
  output logic mem_write_rsp_valid,
  output logic mem_write_rsp_data,
  input logic mem_write_rsp_ready,
  output logic [7:0] read_count,
  output logic [7:0] write_count,
  output logic [31:0] write_checksum
);

  logic [7:0] read_count_r;
  logic [7:0] write_count_r;
  logic [31:0] write_checksum_r;
  assign read_count = read_count_r;
  assign write_count = write_count_r;
  assign write_checksum = write_checksum_r;
  logic [1:0] _tlm_mem_read_state;
  logic [7:0] _tlm_mem_read_addr_latched;
  logic [1:0] _tlm_mem_write_state;
  logic [7:0] _tlm_mem_write_addr_latched;
  logic [31:0] _tlm_mem_write_data_latched;
  always_ff @(posedge clk) begin
    if (rst) begin
      _tlm_mem_read_addr_latched <= 0;
      _tlm_mem_read_state <= 0;
      read_count_r <= 0;
    end else begin
      if (_tlm_mem_read_state == 2'd0 && mem_read_req_valid) begin
        _tlm_mem_read_addr_latched <= mem_read_addr;
        _tlm_mem_read_state <= 2'd1;
      end
      if (_tlm_mem_read_state == 2'd1 && 1'b1) begin
        read_count_r <= 8'(read_count_r + 8'd1);
        _tlm_mem_read_state <= 2'd2;
      end
      if (_tlm_mem_read_state == 2'd2 && mem_read_rsp_ready) begin
        _tlm_mem_read_state <= 2'd0;
      end
    end
  end
  always_comb begin
    mem_read_req_ready = _tlm_mem_read_state == 2'd0;
    mem_read_rsp_valid = _tlm_mem_read_state == 2'd2;
    mem_read_rsp_data = 32'(32'd268435456 + 32'($unsigned(_tlm_mem_read_addr_latched)));
    if (_tlm_mem_read_state == 2'd2) begin
      mem_read_rsp_data = 32'(32'd268435456 + 32'($unsigned(_tlm_mem_read_addr_latched)));
    end
  end
  always_ff @(posedge clk) begin
    if (rst) begin
      _tlm_mem_write_addr_latched <= 0;
      _tlm_mem_write_data_latched <= 0;
      _tlm_mem_write_state <= 0;
      write_checksum_r <= 0;
      write_count_r <= 0;
    end else begin
      if (_tlm_mem_write_state == 2'd0 && mem_write_req_valid) begin
        _tlm_mem_write_addr_latched <= mem_write_addr;
        _tlm_mem_write_data_latched <= mem_write_data;
        _tlm_mem_write_state <= 2'd1;
      end
      if (_tlm_mem_write_state == 2'd1 && 1'b1) begin
        write_count_r <= 8'(write_count_r + 8'd1);
        write_checksum_r <= (32 > $bits(_tlm_mem_write_data_latched ^ 32'($unsigned(_tlm_mem_write_addr_latched))) ? 32 : $bits(_tlm_mem_write_data_latched ^ 32'($unsigned(_tlm_mem_write_addr_latched))))'(write_checksum_r + (_tlm_mem_write_data_latched ^ 32'($unsigned(_tlm_mem_write_addr_latched))));
        _tlm_mem_write_state <= 2'd2;
      end
      if (_tlm_mem_write_state == 2'd2 && mem_write_rsp_ready) begin
        _tlm_mem_write_state <= 2'd0;
      end
    end
  end
  always_comb begin
    mem_write_req_ready = _tlm_mem_write_state == 2'd0;
    mem_write_rsp_valid = _tlm_mem_write_state == 2'd2;
    mem_write_rsp_data = _tlm_mem_write_data_latched[7:0] == _tlm_mem_write_addr_latched;
    if (_tlm_mem_write_state == 2'd2) begin
      mem_write_rsp_data = _tlm_mem_write_data_latched[7:0] == _tlm_mem_write_addr_latched;
    end
  end

  // synopsys translate_off
  // Auto-generated TLM method protocol assertions
  _auto_tlm_mem_read_req_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_req_valid && !mem_read_req_ready) |=> (mem_read_req_valid && $stable(mem_read_addr)))
    else $fatal(1, "TLM VIOLATION (request changed while stalled): TlmPairingArchTarget._auto_tlm_mem_read_req_stable");
  _auto_tlm_mem_read_rsp_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_rsp_valid && !mem_read_rsp_ready) |=> (mem_read_rsp_valid && $stable(mem_read_rsp_data)))
    else $fatal(1, "TLM VIOLATION (response changed while stalled): TlmPairingArchTarget._auto_tlm_mem_read_rsp_stable");
  _auto_tlm_mem_write_req_stable: assert property (@(posedge clk) disable iff (rst) (mem_write_req_valid && !mem_write_req_ready) |=> (mem_write_req_valid && $stable(mem_write_addr) && $stable(mem_write_data)))
    else $fatal(1, "TLM VIOLATION (request changed while stalled): TlmPairingArchTarget._auto_tlm_mem_write_req_stable");
  _auto_tlm_mem_write_rsp_stable: assert property (@(posedge clk) disable iff (rst) (mem_write_rsp_valid && !mem_write_rsp_ready) |=> (mem_write_rsp_valid && $stable(mem_write_rsp_data)))
    else $fatal(1, "TLM VIOLATION (response changed while stalled): TlmPairingArchTarget._auto_tlm_mem_write_rsp_stable");
  // synopsys translate_on

endmodule
