module TlmDualReadInitiator (
  input  logic        clk,
  input  logic        rst,

  output logic        left_read_req_valid,
  output logic [7:0]  left_read_addr,
  input  logic        left_read_req_ready,
  input  logic        left_read_rsp_valid,
  input  logic [31:0] left_read_rsp_data,
  output logic        left_read_rsp_ready,

  output logic        right_read_req_valid,
  output logic [7:0]  right_read_addr,
  input  logic        right_read_req_ready,
  input  logic        right_read_rsp_valid,
  input  logic [31:0] right_read_rsp_data,
  output logic        right_read_rsp_ready,

  output logic        done,
  output logic [31:0] left_result,
  output logic [31:0] right_result
);
  logic left_done;
  logic right_done;

  always_ff @(posedge clk) begin
    if (rst) begin
      left_read_req_valid <= 1'b0;
      left_read_addr <= 8'd5;
      left_read_rsp_ready <= 1'b0;
      left_done <= 1'b0;
      left_result <= 32'd0;
    end else if (!left_done) begin
      if (!left_read_rsp_ready) begin
        left_read_req_valid <= 1'b1;
        if (left_read_req_valid && left_read_req_ready) begin
          left_read_req_valid <= 1'b0;
          left_read_rsp_ready <= 1'b1;
        end
      end else if (left_read_rsp_valid) begin
        left_result <= left_read_rsp_data;
        left_read_rsp_ready <= 1'b0;
        left_done <= 1'b1;
      end
    end
  end

  always_ff @(posedge clk) begin
    if (rst) begin
      right_read_req_valid <= 1'b0;
      right_read_addr <= 8'd9;
      right_read_rsp_ready <= 1'b0;
      right_done <= 1'b0;
      right_result <= 32'd0;
    end else if (!right_done) begin
      if (!right_read_rsp_ready) begin
        right_read_req_valid <= 1'b1;
        if (right_read_req_valid && right_read_req_ready) begin
          right_read_req_valid <= 1'b0;
          right_read_rsp_ready <= 1'b1;
        end
      end else if (right_read_rsp_valid) begin
        right_result <= right_read_rsp_data;
        right_read_rsp_ready <= 1'b0;
        right_done <= 1'b1;
      end
    end
  end

  assign done = left_done && right_done;
endmodule
