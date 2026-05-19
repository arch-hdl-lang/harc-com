module TlmMemory (
  input  logic        clk,
  input  logic        rst,

  input  logic        mem_read_req_valid,
  input  logic [7:0]  mem_read_addr,
  output logic        mem_read_req_ready,
  output logic        mem_read_rsp_valid,
  output logic [31:0] mem_read_rsp_data,
  input  logic        mem_read_rsp_ready,

  input  logic        mem_read_ooo_req_valid,
  input  logic [7:0]  mem_read_ooo_addr,
  input  logic [0:0]  mem_read_ooo_req_tag,
  output logic        mem_read_ooo_req_ready,
  output logic        mem_read_ooo_rsp_valid,
  output logic [31:0] mem_read_ooo_rsp_data,
  output logic [0:0]  mem_read_ooo_rsp_tag,
  input  logic        mem_read_ooo_rsp_ready,

  input  logic        mem_poke_req_valid,
  input  logic [7:0]  mem_poke_addr,
  input  logic [31:0] mem_poke_data,
  output logic        mem_poke_req_ready,
  output logic        mem_poke_rsp_valid,
  input  logic        mem_poke_rsp_ready
);
  logic [31:0] mem [0:255];
  logic        ooo_valid [0:1];
  logic [31:0] ooo_data [0:1];
  logic [0:0]  ooo_tag [0:1];

  assign mem_read_req_ready = !mem_read_rsp_valid || mem_read_rsp_ready;
  assign mem_read_ooo_req_ready = !ooo_valid[0] || !ooo_valid[1];
  assign mem_read_ooo_rsp_valid = ooo_valid[1] || ooo_valid[0];
  assign mem_read_ooo_rsp_data = ooo_valid[1] ? ooo_data[1] : ooo_data[0];
  assign mem_read_ooo_rsp_tag = ooo_valid[1] ? ooo_tag[1] : ooo_tag[0];
  assign mem_poke_req_ready = !mem_poke_rsp_valid || mem_poke_rsp_ready;

  always_ff @(posedge clk) begin
    if (rst) begin
      mem_read_rsp_valid <= 1'b0;
      mem_read_rsp_data <= 32'h0;
      ooo_valid[0] <= 1'b0;
      ooo_valid[1] <= 1'b0;
      ooo_data[0] <= 32'h0;
      ooo_data[1] <= 32'h0;
      ooo_tag[0] <= 1'b0;
      ooo_tag[1] <= 1'b0;
      mem_poke_rsp_valid <= 1'b0;
      for (int i = 0; i < 256; i++) begin
        mem[i] <= 32'h100 + i[31:0];
      end
    end else begin
      if (mem_read_rsp_valid && mem_read_rsp_ready) begin
        mem_read_rsp_valid <= 1'b0;
      end
      if (mem_read_ooo_rsp_valid && mem_read_ooo_rsp_ready) begin
        if (ooo_valid[1]) begin
          ooo_valid[1] <= 1'b0;
        end else begin
          ooo_valid[0] <= 1'b0;
        end
      end
      if (mem_poke_rsp_valid && mem_poke_rsp_ready) begin
        mem_poke_rsp_valid <= 1'b0;
      end

      if (mem_read_req_valid && mem_read_req_ready) begin
        mem_read_rsp_valid <= 1'b1;
        mem_read_rsp_data <= mem[mem_read_addr];
      end
      if (mem_read_ooo_req_valid && mem_read_ooo_req_ready) begin
        if (!ooo_valid[0]) begin
          ooo_valid[0] <= 1'b1;
          ooo_data[0] <= mem[mem_read_ooo_addr];
          ooo_tag[0] <= mem_read_ooo_req_tag;
        end else begin
          ooo_valid[1] <= 1'b1;
          ooo_data[1] <= mem[mem_read_ooo_addr];
          ooo_tag[1] <= mem_read_ooo_req_tag;
        end
      end
      if (mem_poke_req_valid && mem_poke_req_ready) begin
        mem[mem_poke_addr] <= mem_poke_data;
        mem_poke_rsp_valid <= 1'b1;
      end
    end
  end
endmodule
