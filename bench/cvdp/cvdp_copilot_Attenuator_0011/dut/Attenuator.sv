`timescale 1ns / 1ps

module Attenuator (
    input       clk,
    input       reset,
    input [4:0] data,
    output reg  ATTN_CLK,
    output reg  ATTN_DATA,
    output reg  ATTN_LE
);

reg         clk_div2;
reg  [1:0]  current_state, next_state;
reg  [4:0]  shift_reg;
reg  [2:0]  bit_count;
reg  [4:0]  old_data;

parameter IDLE  = 2'b00,
          LOAD  = 2'b01,
          SHIFT = 2'b10,
          LATCH = 2'b11;

initial begin
    ATTN_CLK  = 1'b0;
    ATTN_DATA = 1'b0;
    ATTN_LE   = 1'b0;
end

always @(posedge clk or posedge reset) begin
    if (reset) begin
        clk_div2 <= 1'b0;
    end else begin
        clk_div2 <= ~clk_div2;
    end
end

always @(posedge clk_div2 or posedge reset) begin
    if (reset) begin
        current_state <= IDLE;
        old_data      <= 5'b00000;
        shift_reg     <= 5'b00000;
        bit_count     <= 3'd0;

        ATTN_CLK      <= 1'b0;
        ATTN_DATA     <= 1'b0;
        ATTN_LE       <= 1'b0;
    end else begin
        current_state <= next_state;
        old_data      <= data;

        ATTN_CLK <= 1'b0;
        ATTN_LE  <= 1'b0;

        if (current_state == IDLE) begin
            ATTN_DATA <= 1'b0;
        end

        case (current_state)
            LOAD: begin
                shift_reg <= data;
                bit_count <= 3'd5;

                ATTN_DATA <= data[4];
            end

            SHIFT: begin
                ATTN_CLK <= 1'b1;

                ATTN_DATA = shift_reg[4]; 
                shift_reg = shift_reg << 1; 
                bit_count <= bit_count - 1;
            end

            LATCH: begin
                ATTN_LE <= 1'b1;
            end
        endcase
    end
end

always @(*) begin
    next_state = current_state;
    case (current_state)
        IDLE:   if (data != old_data) next_state = LOAD;
        LOAD:   next_state = SHIFT;
        SHIFT:  if (bit_count == 0)   next_state = LATCH;
        LATCH:  next_state = IDLE;
    endcase
end

endmodule