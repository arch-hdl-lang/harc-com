module TlmReadInitiatorPair (
  input  logic        clk,
  input  logic        rst,

  output logic        mem_read_req_valid,
  output logic [7:0]  mem_read_addr,
  input  logic        mem_read_req_ready,
  input  logic        mem_read_rsp_valid,
  input  logic [31:0] mem_read_rsp_data,
  output logic        mem_read_rsp_ready,

  output logic        done,
  output logic [31:0] result0,
  output logic [31:0] result1
);
  typedef enum logic [2:0] {S_REQ0, S_RSP0, S_REQ1, S_RSP1, S_DONE} state_e;
  state_e state;

  always_ff @(posedge clk) begin
    if (rst) begin
      state <= S_REQ0;
      mem_read_req_valid <= 1'b0;
      mem_read_addr <= 8'h05;
      mem_read_rsp_ready <= 1'b0;
      done <= 1'b0;
      result0 <= 32'h0;
      result1 <= 32'h0;
    end else begin
      unique case (state)
        S_REQ0: begin
          mem_read_addr <= 8'h05;
          if (mem_read_req_valid && mem_read_req_ready) begin
            mem_read_req_valid <= 1'b0;
            mem_read_rsp_ready <= 1'b1;
            state <= S_RSP0;
          end else begin
            mem_read_req_valid <= 1'b1;
          end
        end
        S_RSP0: begin
          if (mem_read_rsp_valid) begin
            result0 <= mem_read_rsp_data;
            mem_read_rsp_ready <= 1'b0;
            state <= S_REQ1;
          end
        end
        S_REQ1: begin
          mem_read_addr <= 8'h0c;
          if (mem_read_req_valid && mem_read_req_ready) begin
            mem_read_req_valid <= 1'b0;
            mem_read_rsp_ready <= 1'b1;
            state <= S_RSP1;
          end else begin
            mem_read_req_valid <= 1'b1;
          end
        end
        S_RSP1: begin
          if (mem_read_rsp_valid) begin
            result1 <= mem_read_rsp_data;
            mem_read_rsp_ready <= 1'b0;
            done <= 1'b1;
            state <= S_DONE;
          end
        end
        default: begin
          done <= 1'b1;
        end
      endcase
    end
  end
endmodule
