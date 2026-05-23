module DmaEngineTlmMem #(
  parameter int DATA_WIDTH = 32
) (
  input logic clk,
  input logic rst,

  input logic apb_sel,
  input logic apb_enable,
  input logic apb_write,
  input logic [2:0] apb_addr,
  input logic [31:0] apb_wdata,
  output logic [31:0] apb_rdata,
  output logic apb_ready,

  output logic mem_read_req_valid,
  output logic [31:0] mem_read_addr,
  input logic mem_read_req_ready,
  input logic mem_read_rsp_valid,
  input logic [31:0] mem_read_rsp_data,
  output logic mem_read_rsp_ready,

  output logic mem_write_req_valid,
  output logic [31:0] mem_write_addr,
  output logic [31:0] mem_write_data,
  input logic mem_write_req_ready,
  input logic mem_write_rsp_valid,
  output logic mem_write_rsp_ready,

  output logic irq,
  output logic busy
);

  logic core_mem_rd_valid;
  logic core_mem_rd_ready;
  logic [31:0] core_mem_rd_addr;
  logic [31:0] core_mem_rd_data;
  logic core_mem_wr_valid;
  logic core_mem_wr_ready;
  logic [31:0] core_mem_wr_addr;
  logic [31:0] core_mem_wr_data;

  logic rd_wait_rsp;
  logic rd_have_rsp;
  logic rd_ack_rsp;
  logic [31:0] rd_addr_q;
  logic [31:0] rd_rsp_data_q;
  logic wr_wait_rsp;
  logic [31:0] wr_addr_q;
  logic [31:0] wr_data_q;

  DmaEngine #(.DATA_WIDTH(DATA_WIDTH)) core (
    .clk(clk),
    .rst(rst),
    .apb_sel(apb_sel),
    .apb_enable(apb_enable),
    .apb_write(apb_write),
    .apb_addr(apb_addr),
    .apb_wdata(apb_wdata),
    .apb_rdata(apb_rdata),
    .apb_ready(apb_ready),
    .mem_rd_valid(core_mem_rd_valid),
    .mem_rd_ready(core_mem_rd_ready),
    .mem_rd_addr(core_mem_rd_addr),
    .mem_rd_data(core_mem_rd_data),
    .mem_wr_valid(core_mem_wr_valid),
    .mem_wr_ready(core_mem_wr_ready),
    .mem_wr_addr(core_mem_wr_addr),
    .mem_wr_data(core_mem_wr_data),
    .irq(irq),
    .busy(busy)
  );

  assign mem_read_req_valid = core_mem_rd_valid && !rd_wait_rsp && !rd_have_rsp;
  assign mem_read_addr = (rd_wait_rsp || rd_have_rsp) ? rd_addr_q : core_mem_rd_addr;
  assign mem_read_rsp_ready = rd_ack_rsp;
  assign core_mem_rd_ready = rd_have_rsp;
  assign core_mem_rd_data = rd_rsp_data_q;

  assign mem_write_req_valid = core_mem_wr_valid && !wr_wait_rsp;
  assign mem_write_addr = wr_wait_rsp ? wr_addr_q : core_mem_wr_addr;
  assign mem_write_data = wr_wait_rsp ? wr_data_q : core_mem_wr_data;
  assign mem_write_rsp_ready = wr_wait_rsp;
  assign core_mem_wr_ready = !wr_wait_rsp && mem_write_req_ready;

  always_ff @(posedge clk) begin
    if (rst) begin
      rd_wait_rsp <= 1'b0;
      rd_have_rsp <= 1'b0;
      rd_ack_rsp <= 1'b0;
      rd_addr_q <= '0;
      rd_rsp_data_q <= '0;
      wr_wait_rsp <= 1'b0;
      wr_addr_q <= '0;
      wr_data_q <= '0;
    end else begin
      rd_ack_rsp <= 1'b0;
      if (!rd_wait_rsp && !rd_have_rsp && core_mem_rd_valid && mem_read_req_ready) begin
        rd_wait_rsp <= 1'b1;
        rd_addr_q <= core_mem_rd_addr;
      end else if (rd_wait_rsp && mem_read_rsp_valid) begin
        rd_wait_rsp <= 1'b0;
        rd_have_rsp <= 1'b1;
        rd_ack_rsp <= 1'b1;
        rd_rsp_data_q <= mem_read_rsp_data;
      end
      if (rd_have_rsp && core_mem_rd_valid) begin
        rd_have_rsp <= 1'b0;
      end

      if (!wr_wait_rsp && core_mem_wr_valid && mem_write_req_ready) begin
        wr_wait_rsp <= 1'b1;
        wr_addr_q <= core_mem_wr_addr;
        wr_data_q <= core_mem_wr_data;
      end else if (wr_wait_rsp && mem_write_rsp_valid && mem_write_rsp_ready) begin
        wr_wait_rsp <= 1'b0;
      end
    end
  end

endmodule
