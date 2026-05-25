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
module _arb_TlmPairingArchTarget__tlm_mem_read_ooo_rsp_ch #(
  parameter int NUM_REQ = 2
) (
  input logic clk,
  input logic rst,
  output logic grant_valid,
  output logic [0:0] grant_requester,
  input logic [NUM_REQ-1:0] request_valid,
  output logic [NUM_REQ-1:0] request_ready
);

  always_comb begin
    grant_valid = 1'b0;
    request_ready = '0;
    grant_requester = '0;
    for (int pri_i = 0; pri_i < 2; pri_i++) begin
      if (!grant_valid && request_valid[pri_i]) begin
        grant_valid = 1'b1;
        grant_requester = 1'(pri_i);
        request_ready[pri_i] = 1'b1;
      end
    end
  end

endmodule

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
  input logic mem_read_ooo_req_valid,
  input logic [1:0] mem_read_ooo_req_tag,
  input logic [7:0] mem_read_ooo_addr,
  output logic mem_read_ooo_req_ready,
  output logic mem_read_ooo_rsp_valid,
  output logic [1:0] mem_read_ooo_rsp_tag,
  output logic [31:0] mem_read_ooo_rsp_data,
  input logic mem_read_ooo_rsp_ready,
  output logic [7:0] read_count,
  output logic [7:0] write_count,
  output logic [31:0] write_checksum,
  output logic [7:0] ooo0_count,
  output logic [7:0] ooo1_count
);

  logic [7:0] read_count_r;
  logic [7:0] write_count_r;
  logic [31:0] write_checksum_r;
  logic [7:0] ooo0_count_r;
  logic [7:0] ooo1_count_r;
  assign read_count = read_count_r;
  assign write_count = write_count_r;
  assign write_checksum = write_checksum_r;
  assign ooo0_count = ooo0_count_r;
  assign ooo1_count = ooo1_count_r;
  logic [1:0] _tlm_mem_read_state;
  logic [7:0] _tlm_mem_read_addr_latched;
  logic [1:0] _tlm_mem_write_state;
  logic [7:0] _tlm_mem_write_addr_latched;
  logic [31:0] _tlm_mem_write_data_latched;
  logic _tlm_mem_read_ooo_tag0_req_ready;
  logic _tlm_mem_read_ooo_tag0_rsp_valid;
  logic _tlm_mem_read_ooo_tag0_rsp_ready;
  logic [1:0] _tlm_mem_read_ooo_tag0_rsp_tag;
  logic [31:0] _tlm_mem_read_ooo_tag0_rsp_data;
  logic [1:0] _tlm_mem_read_ooo_tag0_state;
  logic [31:0] _tlm_mem_read_ooo_tag0_wait_cnt;
  logic [1:0] _tlm_mem_read_ooo_tag0_tag_latched;
  logic [7:0] _tlm_mem_read_ooo_tag0_addr_latched;
  logic _tlm_mem_read_ooo_tag1_req_ready;
  logic _tlm_mem_read_ooo_tag1_rsp_valid;
  logic _tlm_mem_read_ooo_tag1_rsp_ready;
  logic [1:0] _tlm_mem_read_ooo_tag1_rsp_tag;
  logic [31:0] _tlm_mem_read_ooo_tag1_rsp_data;
  logic [1:0] _tlm_mem_read_ooo_tag1_state;
  logic [1:0] _tlm_mem_read_ooo_tag1_tag_latched;
  logic [7:0] _tlm_mem_read_ooo_tag1_addr_latched;
  logic [1:0] _tlm_mem_read_ooo_rsp_arb_req_packed;
  logic [1:0] _tlm_mem_read_ooo_rsp_arb_grant_packed;
  logic _tlm_mem_read_ooo_rsp_arb_grant_valid;
  logic [0:0] _tlm_mem_read_ooo_rsp_arb_grant_requester;
  logic _tlm_mem_read_ooo_rsp_arb_hold_valid_r;
  logic [0:0] _tlm_mem_read_ooo_rsp_arb_hold_idx_r;
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
  always_ff @(posedge clk) begin
    if (rst) begin
      _tlm_mem_read_ooo_tag0_addr_latched <= 0;
      _tlm_mem_read_ooo_tag0_state <= 0;
      _tlm_mem_read_ooo_tag0_tag_latched <= 0;
      _tlm_mem_read_ooo_tag0_wait_cnt <= 0;
      ooo0_count_r <= 0;
    end else begin
      if (_tlm_mem_read_ooo_tag0_state == 2'd0 && mem_read_ooo_req_valid && mem_read_ooo_req_tag == 2'd0) begin
        _tlm_mem_read_ooo_tag0_addr_latched <= mem_read_ooo_addr;
        _tlm_mem_read_ooo_tag0_tag_latched <= mem_read_ooo_req_tag;
        _tlm_mem_read_ooo_tag0_state <= 2'd1;
      end
      if (_tlm_mem_read_ooo_tag0_state == 2'd1 && 1'b1) begin
        ooo0_count_r <= 8'(ooo0_count_r + 8'd1);
        _tlm_mem_read_ooo_tag0_wait_cnt <= 32'd3;
        _tlm_mem_read_ooo_tag0_state <= 2'd2;
      end
      if (_tlm_mem_read_ooo_tag0_state == 2'd2 && !(_tlm_mem_read_ooo_tag0_wait_cnt == 32'd0)) begin
        _tlm_mem_read_ooo_tag0_wait_cnt <= 32'(_tlm_mem_read_ooo_tag0_wait_cnt - 32'd1);
      end
      if (_tlm_mem_read_ooo_tag0_state == 2'd2 && 1'b1 && _tlm_mem_read_ooo_tag0_wait_cnt == 32'd0) begin
        _tlm_mem_read_ooo_tag0_state <= 2'd3;
      end
      if (_tlm_mem_read_ooo_tag0_state == 2'd3 && _tlm_mem_read_ooo_tag0_rsp_ready) begin
        _tlm_mem_read_ooo_tag0_state <= 2'd0;
      end
    end
  end
  always_comb begin
    _tlm_mem_read_ooo_tag0_req_ready = _tlm_mem_read_ooo_tag0_state == 2'd0;
    _tlm_mem_read_ooo_tag0_rsp_valid = _tlm_mem_read_ooo_tag0_state == 2'd3;
    _tlm_mem_read_ooo_tag0_rsp_data = 32'(32'd805306368 + 32'($unsigned(_tlm_mem_read_ooo_tag0_addr_latched)));
    if (_tlm_mem_read_ooo_tag0_state == 2'd3) begin
      _tlm_mem_read_ooo_tag0_rsp_data = 32'(32'd805306368 + 32'($unsigned(_tlm_mem_read_ooo_tag0_addr_latched)));
    end
    _tlm_mem_read_ooo_tag0_rsp_tag = _tlm_mem_read_ooo_tag0_tag_latched;
  end
  always_ff @(posedge clk) begin
    if (rst) begin
      _tlm_mem_read_ooo_tag1_addr_latched <= 0;
      _tlm_mem_read_ooo_tag1_state <= 0;
      _tlm_mem_read_ooo_tag1_tag_latched <= 0;
      ooo1_count_r <= 0;
    end else begin
      if (_tlm_mem_read_ooo_tag1_state == 2'd0 && mem_read_ooo_req_valid && mem_read_ooo_req_tag == 2'd1) begin
        _tlm_mem_read_ooo_tag1_addr_latched <= mem_read_ooo_addr;
        _tlm_mem_read_ooo_tag1_tag_latched <= mem_read_ooo_req_tag;
        _tlm_mem_read_ooo_tag1_state <= 2'd1;
      end
      if (_tlm_mem_read_ooo_tag1_state == 2'd1 && 1'b1) begin
        ooo1_count_r <= 8'(ooo1_count_r + 8'd1);
        _tlm_mem_read_ooo_tag1_state <= 2'd2;
      end
      if (_tlm_mem_read_ooo_tag1_state == 2'd2 && _tlm_mem_read_ooo_tag1_rsp_ready) begin
        _tlm_mem_read_ooo_tag1_state <= 2'd0;
      end
    end
  end
  always_comb begin
    _tlm_mem_read_ooo_tag1_req_ready = _tlm_mem_read_ooo_tag1_state == 2'd0;
    _tlm_mem_read_ooo_tag1_rsp_valid = _tlm_mem_read_ooo_tag1_state == 2'd2;
    _tlm_mem_read_ooo_tag1_rsp_data = 32'(32'd1073741824 + 32'($unsigned(_tlm_mem_read_ooo_tag1_addr_latched)));
    if (_tlm_mem_read_ooo_tag1_state == 2'd2) begin
      _tlm_mem_read_ooo_tag1_rsp_data = 32'(32'd1073741824 + 32'($unsigned(_tlm_mem_read_ooo_tag1_addr_latched)));
    end
    _tlm_mem_read_ooo_tag1_rsp_tag = _tlm_mem_read_ooo_tag1_tag_latched;
  end
  _arb_TlmPairingArchTarget__tlm_mem_read_ooo_rsp_ch _tlm_mem_read_ooo_rsp_arb_inst (
    .clk(clk),
    .rst(rst),
    .request_valid(_tlm_mem_read_ooo_rsp_arb_req_packed),
    .request_ready(_tlm_mem_read_ooo_rsp_arb_grant_packed),
    .grant_valid(_tlm_mem_read_ooo_rsp_arb_grant_valid),
    .grant_requester(_tlm_mem_read_ooo_rsp_arb_grant_requester)
  );
  always_comb begin
    _tlm_mem_read_ooo_rsp_arb_req_packed[0] = !_tlm_mem_read_ooo_rsp_arb_hold_valid_r && _tlm_mem_read_ooo_tag0_rsp_valid;
    _tlm_mem_read_ooo_rsp_arb_req_packed[1] = !_tlm_mem_read_ooo_rsp_arb_hold_valid_r && _tlm_mem_read_ooo_tag1_rsp_valid;
    mem_read_ooo_req_ready = 0;
    mem_read_ooo_rsp_valid = 0;
    mem_read_ooo_rsp_data = _tlm_mem_read_ooo_tag0_rsp_data;
    mem_read_ooo_rsp_tag = 0;
    _tlm_mem_read_ooo_tag0_rsp_ready = 0;
    _tlm_mem_read_ooo_tag1_rsp_ready = 0;
    if (mem_read_ooo_req_tag == 2'd0) begin
      mem_read_ooo_req_ready = _tlm_mem_read_ooo_tag0_req_ready;
    end
    if (mem_read_ooo_req_tag == 2'd1) begin
      mem_read_ooo_req_ready = _tlm_mem_read_ooo_tag1_req_ready;
    end
    if ((_tlm_mem_read_ooo_rsp_arb_hold_valid_r && _tlm_mem_read_ooo_rsp_arb_hold_idx_r == 1'd0 || _tlm_mem_read_ooo_rsp_arb_grant_packed[0]) && _tlm_mem_read_ooo_tag0_rsp_valid) begin
      mem_read_ooo_rsp_valid = 1'd1;
      mem_read_ooo_rsp_tag = _tlm_mem_read_ooo_tag0_rsp_tag;
      _tlm_mem_read_ooo_tag0_rsp_ready = mem_read_ooo_rsp_ready;
      mem_read_ooo_rsp_data = _tlm_mem_read_ooo_tag0_rsp_data;
    end
    if ((_tlm_mem_read_ooo_rsp_arb_hold_valid_r && _tlm_mem_read_ooo_rsp_arb_hold_idx_r == 1'd1 || _tlm_mem_read_ooo_rsp_arb_grant_packed[1]) && _tlm_mem_read_ooo_tag1_rsp_valid) begin
      mem_read_ooo_rsp_valid = 1'd1;
      mem_read_ooo_rsp_tag = _tlm_mem_read_ooo_tag1_rsp_tag;
      _tlm_mem_read_ooo_tag1_rsp_ready = mem_read_ooo_rsp_ready;
      mem_read_ooo_rsp_data = _tlm_mem_read_ooo_tag1_rsp_data;
    end
  end
  always_ff @(posedge clk) begin
    if (rst) begin
      _tlm_mem_read_ooo_rsp_arb_hold_idx_r <= 0;
      _tlm_mem_read_ooo_rsp_arb_hold_valid_r <= 0;
    end else begin
      if (_tlm_mem_read_ooo_rsp_arb_hold_valid_r && mem_read_ooo_rsp_ready) begin
        _tlm_mem_read_ooo_rsp_arb_hold_valid_r <= 0;
      end
      if (!_tlm_mem_read_ooo_rsp_arb_hold_valid_r && _tlm_mem_read_ooo_rsp_arb_grant_valid && !mem_read_ooo_rsp_ready) begin
        _tlm_mem_read_ooo_rsp_arb_hold_valid_r <= 1'd1;
        _tlm_mem_read_ooo_rsp_arb_hold_idx_r <= _tlm_mem_read_ooo_rsp_arb_grant_requester;
      end
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
  _auto_tlm_mem_read_ooo_req_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_ooo_req_valid && !mem_read_ooo_req_ready) |=> (mem_read_ooo_req_valid && $stable(mem_read_ooo_req_tag) && $stable(mem_read_ooo_addr)))
    else $fatal(1, "TLM VIOLATION (request changed while stalled): TlmPairingArchTarget._auto_tlm_mem_read_ooo_req_stable");
  _auto_tlm_mem_read_ooo_rsp_stable: assert property (@(posedge clk) disable iff (rst) (mem_read_ooo_rsp_valid && !mem_read_ooo_rsp_ready) |=> (mem_read_ooo_rsp_valid && $stable(mem_read_ooo_rsp_tag) && $stable(mem_read_ooo_rsp_data)))
    else $fatal(1, "TLM VIOLATION (response changed while stalled): TlmPairingArchTarget._auto_tlm_mem_read_ooo_rsp_stable");
  // synopsys translate_on

endmodule
