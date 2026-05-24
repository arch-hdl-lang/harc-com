module TlmOooReadInitiatorPair (
  input  logic        clk,
  input  logic        rst,

  output logic        mem_read_ooo_req_valid,
  output logic [7:0]  mem_read_ooo_addr,
  output logic [0:0]  mem_read_ooo_req_tag,
  input  logic        mem_read_ooo_req_ready,

  input  logic        mem_read_ooo_rsp_valid,
  input  logic [31:0] mem_read_ooo_rsp_data,
  input  logic [0:0]  mem_read_ooo_rsp_tag,
  output logic        mem_read_ooo_rsp_ready,

  output logic        done,
  output logic        late_ready_error,
  output logic [31:0] result0,
  output logic [31:0] result1
);
  typedef enum logic [1:0] {S_REQ0, S_REQ1, S_WAIT, S_DONE} state_e;
  state_e state;
  logic got0;
  logic got1;
  logic [1:0] req1_wait;

  always_ff @(posedge clk) begin
    if (rst) begin
      state <= S_REQ0;
      mem_read_ooo_req_valid <= 1'b0;
      mem_read_ooo_addr <= 8'h05;
      mem_read_ooo_req_tag <= 1'b0;
      mem_read_ooo_rsp_ready <= 1'b0;
      done <= 1'b0;
      late_ready_error <= 1'b0;
      result0 <= 32'h0;
      result1 <= 32'h0;
      got0 <= 1'b0;
      got1 <= 1'b0;
      req1_wait <= 2'h0;
    end else begin
      unique case (state)
        S_REQ0: begin
          mem_read_ooo_req_valid <= 1'b1;
          mem_read_ooo_addr <= 8'h05;
          mem_read_ooo_req_tag <= 1'b0;
          if (mem_read_ooo_req_valid && mem_read_ooo_req_ready) begin
            state <= S_REQ1;
            req1_wait <= 2'h0;
          end
        end
        S_REQ1: begin
          mem_read_ooo_req_valid <= 1'b1;
          mem_read_ooo_addr <= 8'h0c;
          mem_read_ooo_req_tag <= 1'b1;
          if (mem_read_ooo_req_valid && mem_read_ooo_req_ready) begin
            mem_read_ooo_req_valid <= 1'b0;
            mem_read_ooo_rsp_ready <= 1'b1;
            state <= S_WAIT;
          end else begin
            req1_wait <= req1_wait + 2'h1;
            if (req1_wait >= 2'h1) begin
              late_ready_error <= 1'b1;
            end
          end
        end
        S_WAIT: begin
          mem_read_ooo_req_valid <= 1'b0;
          mem_read_ooo_rsp_ready <= 1'b1;
          if (mem_read_ooo_rsp_valid && mem_read_ooo_rsp_ready) begin
            if (mem_read_ooo_rsp_tag == 1'b0) begin
              result0 <= mem_read_ooo_rsp_data;
              got0 <= 1'b1;
            end else begin
              result1 <= mem_read_ooo_rsp_data;
              got1 <= 1'b1;
            end
          end
          if (got0 && got1) begin
            done <= 1'b1;
            mem_read_ooo_rsp_ready <= 1'b0;
            state <= S_DONE;
          end
        end
        default: begin
          mem_read_ooo_req_valid <= 1'b0;
          mem_read_ooo_rsp_ready <= 1'b0;
          done <= 1'b1;
        end
      endcase
    end
  end
endmodule
