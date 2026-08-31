typedef struct packed {
  logic signed [7:0] narrow;
  logic signed [64:0] wide;
} SignedRecordReply;

module TlmSignedRecordOooInitiator (
  input logic clk,
  input logic rst,

  output logic widen_req_valid,
  output logic signed [7:0] widen_value,
  input logic widen_req_ready,
  input logic widen_rsp_valid,
  input logic signed [64:0] widen_rsp_data,
  output logic widen_rsp_ready,

  output logic transform_req_valid,
  output SignedRecordReply transform_request,
  output logic transform_req_tag,
  input logic transform_req_ready,

  input logic transform_rsp_valid,
  input SignedRecordReply transform_rsp_data,
  input logic transform_rsp_tag,
  output logic transform_rsp_ready,

  output logic done,
  output logic [2:0] error
);
  typedef enum logic [2:0] {
    WIDEN_REQ,
    WIDEN_RSP,
    REQ0,
    REQ1,
    WAIT_RSP,
    FINISHED
  } state_t;
  state_t state;
  logic got0;
  logic got1;

  always_ff @(posedge clk) begin
    if (rst) begin
      state <= WIDEN_REQ;
      widen_req_valid <= 1'b0;
      widen_value <= -8'sd9;
      widen_rsp_ready <= 1'b0;
      transform_req_valid <= 1'b0;
      transform_request <= '0;
      transform_req_tag <= 1'b0;
      transform_rsp_ready <= 1'b0;
      done <= 1'b0;
      error <= 3'b000;
      got0 <= 1'b0;
      got1 <= 1'b0;
    end else begin
      case (state)
        WIDEN_REQ: begin
          widen_req_valid <= 1'b1;
          widen_value <= -8'sd9;
          if (widen_req_valid && widen_req_ready) begin
            widen_req_valid <= 1'b0;
            widen_rsp_ready <= 1'b1;
            state <= WIDEN_RSP;
          end
        end
        WIDEN_RSP: begin
          if (widen_rsp_valid && widen_rsp_ready) begin
            if (widen_rsp_data !== -65'sd9) begin
              $display("WIDEN_MISMATCH got=%h expected=%h", widen_rsp_data, -65'sd9);
              error <= error | 3'b001;
            end
            widen_rsp_ready <= 1'b0;
            state <= REQ0;
          end
        end
        REQ0: begin
          transform_req_valid <= 1'b1;
          transform_request.narrow <= -8'sd5;
          transform_request.wide <= -65'sd5;
          transform_req_tag <= 1'b0;
          if (transform_req_valid && transform_req_ready) begin
            state <= REQ1;
          end
        end
        REQ1: begin
          transform_req_valid <= 1'b1;
          transform_request.narrow <= -8'sd7;
          transform_request.wide <= -65'sd7;
          transform_req_tag <= 1'b1;
          if (transform_req_valid && transform_req_ready) begin
            transform_req_valid <= 1'b0;
            transform_rsp_ready <= 1'b1;
            state <= WAIT_RSP;
          end
        end
        WAIT_RSP: begin
          if (transform_rsp_valid && transform_rsp_ready) begin
            if (transform_rsp_tag == 1'b0) begin
              if (transform_rsp_data.narrow !== -8'sd6 ||
                  transform_rsp_data.wide !== -65'sd5) begin
                error <= error | 3'b010;
              end
              got0 <= 1'b1;
            end else begin
              if (transform_rsp_data.narrow !== -8'sd8 ||
                  transform_rsp_data.wide !== -65'sd7) begin
                error <= error | 3'b100;
              end
              got1 <= 1'b1;
            end
          end
          if (got0 && got1) begin
            done <= 1'b1;
            transform_rsp_ready <= 1'b0;
            state <= FINISHED;
          end
        end
        default: begin
          widen_req_valid <= 1'b0;
          widen_rsp_ready <= 1'b0;
          transform_req_valid <= 1'b0;
          transform_rsp_ready <= 1'b0;
          done <= 1'b1;
        end
      endcase
    end
  end
endmodule
