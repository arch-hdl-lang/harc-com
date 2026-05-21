module TlmReadInitiator (
  input  logic        clk,
  input  logic        rst,

  output logic        mem_read_req_valid,
  output logic [7:0]  mem_read_addr,
  input  logic        mem_read_req_ready,
  input  logic        mem_read_rsp_valid,
  input  logic [31:0] mem_read_rsp_data,
  output logic        mem_read_rsp_ready,

  output logic        done,
  output logic [31:0] result
);
  typedef enum logic [1:0] {S_REQ, S_RSP, S_DONE} state_e;
  state_e state;

  always_ff @(posedge clk) begin
    if (rst) begin
      state <= S_REQ;
      mem_read_req_valid <= 1'b0;
      mem_read_addr <= 8'h05;
      mem_read_rsp_ready <= 1'b0;
      done <= 1'b0;
      result <= 32'h0;
    end else begin
      unique case (state)
        S_REQ: begin
          mem_read_addr <= 8'h05;
          if (mem_read_req_valid && mem_read_req_ready) begin
            mem_read_req_valid <= 1'b0;
            mem_read_rsp_ready <= 1'b1;
            state <= S_RSP;
          end else begin
            mem_read_req_valid <= 1'b1;
          end
        end
        S_RSP: begin
          if (mem_read_rsp_valid) begin
            result <= mem_read_rsp_data;
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
