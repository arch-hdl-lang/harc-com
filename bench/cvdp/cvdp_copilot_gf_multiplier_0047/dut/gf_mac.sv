`timescale 1ns/1ps
module gf_mac#(parameter WIDTH=32)(
    input reset,
    input enable,
    input pass_through,
    input [WIDTH-1:0] a,
    input [WIDTH-1:0] b,
    output [7:0] result,
    output valid_result,
    output ready,
    output error_flag
);
    localparam WIDTH_VALID = (WIDTH % 8 == 0);

    reg [7:0] result_reg;
    reg [7:0] mux_result;

    assign error_flag = (reset) ? 1'b0 : (!WIDTH_VALID);
    assign valid_result = (reset) ? 1'b0 : ((enable && WIDTH_VALID) ? 1'b1 : 1'b0);
    assign ready = (reset) ? 1'b0 : ((enable && WIDTH_VALID) ? 1'b1 : 1'b0);
    assign result = (reset) ? 8'b0 : mux_result;

    generate
        if (WIDTH_VALID) begin : gen_valid
            integer i;
            reg [7:0] temp_result;
            wire [7:0] partial_results[(WIDTH/8)-1:0];
            genvar j;
            for (j = 0; j < WIDTH/8; j = j + 1) begin: gen_segments
                gf_multiplier u_gf_multiplier(
                    .A(a[(j+1)*8-1 : j*8]),
                    .B(b[(j+1)*8-1 : j*8]),
                    .result(partial_results[j])
                );
            end
            always @(*) begin
                temp_result = 8'b0;
                if (!reset && enable && !pass_through) begin
                    for (i = 0; i < WIDTH/8; i = i + 1) begin
                        temp_result = temp_result ^ partial_results[i];
                    end
                end
                result_reg = temp_result;
            end
        end
    endgenerate

    always @(*) begin
        if (!WIDTH_VALID) begin
            result_reg = 8'b0;
        end
        if (!reset && enable && pass_through && WIDTH_VALID) begin
            mux_result = a[7:0];
        end else if (!reset && enable && WIDTH_VALID) begin
            mux_result = result_reg;
        end else begin
            mux_result = 8'b0;
        end
    end
endmodule


module gf_multiplier(
    input [7:0] A,
    input [7:0] B,
    output reg [7:0] result
);
    reg [7:0] temp_result;
    reg [8:0] multiplicand;
    localparam [8:0] IRREDUCIBLE_POLY = 9'b100011011;
    integer i;
    always @(*) begin
        temp_result = 8'b0;
        multiplicand = {1'b0, A};
        for (i = 0; i < 8; i = i + 1) begin
            if (B[i]) begin
                temp_result = temp_result ^ multiplicand[7:0];
            end
            multiplicand = multiplicand << 1;
            if (multiplicand[8]) begin
                multiplicand = multiplicand ^ IRREDUCIBLE_POLY;
            end
        end
        result = temp_result;
    end
endmodule
