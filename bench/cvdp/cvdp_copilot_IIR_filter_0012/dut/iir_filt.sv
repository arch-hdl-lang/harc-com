module iir_filt #(parameter WIDTH = 16)(
    input clk,
    input reset,
    input signed [WIDTH-1:0] in_1,
    input signed [WIDTH-1:0] in_2,
    input signed [WIDTH-1:0] in_3,
    input [1:0] filter_select, // 00: Butterworth, 01: Chebyshev, 10: Elliptic
    output reg signed [WIDTH-1:0] out_1,
    output reg signed [WIDTH-1:0] out_2,
    output reg signed [WIDTH-1:0] out_3
);

    reg signed [WIDTH-1:0] butterworth_a_0, butterworth_a_1;
    reg signed [WIDTH-1:0] butterworth_b_0, butterworth_b_1;

    reg signed [WIDTH-1:0] chebyshev_a_0, chebyshev_a_1;
    reg signed [WIDTH-1:0] chebyshev_b_0, chebyshev_b_1;

    reg signed [WIDTH-1:0] elliptic_a_0, elliptic_a_1;
    reg signed [WIDTH-1:0] elliptic_b_0, elliptic_b_1;

    reg signed [WIDTH-1:0] x_1_0, x_1_1, y_1_0, y_1_1;
    reg signed [WIDTH-1:0] x_2_0, x_2_1, y_2_0, y_2_1;
    reg signed [WIDTH-1:0] x_3_0, x_3_1, y_3_0, y_3_1;

    integer i;

    always @(posedge clk or posedge reset) begin
        if (reset) begin
            // Initialize all delay lines and outputs
            x_1_0 <= 0; x_1_1 <= 0; y_1_0 <= 0; y_1_1 <= 0;
            x_2_0 <= 0; x_2_1 <= 0; y_2_0 <= 0; y_2_1 <= 0;
            x_3_0 <= 0; x_3_1 <= 0; y_3_0 <= 0; y_3_1 <= 0;

            out_1 <= 0;
            out_2 <= 0;
            out_3 <= 0;
        end else begin
            // Shift delay lines
            x_1_1 <= x_1_0; x_1_0 <= in_1;
            y_1_1 <= y_1_0;

            x_2_1 <= x_2_0; x_2_0 <= in_2;
            y_2_1 <= y_2_0;

            x_3_1 <= x_3_0; x_3_0 <= in_3;
            y_3_1 <= y_3_0;

            case (filter_select)
                2'b00: begin 
                    y_1_0 <= butterworth_b_0 * x_1_0 + butterworth_b_1 * x_1_1 - butterworth_a_1 * y_1_1;
                    y_2_0 <= butterworth_b_0 * x_2_0 + butterworth_b_1 * x_2_1 - butterworth_a_1 * y_2_1;
                    y_3_0 <= butterworth_b_0 * x_3_0 + butterworth_b_1 * x_3_1 - butterworth_a_1 * y_3_1;
                end
                2'b01: begin 
                    y_1_0 <= chebyshev_b_0 * x_1_0 + chebyshev_b_1 * x_1_1 - chebyshev_a_1 * y_1_1;
                    y_2_0 <= chebyshev_b_0 * x_2_0 + chebyshev_b_1 * x_2_1 - chebyshev_a_1 * y_2_1;
                    y_3_0 <= chebyshev_b_0 * x_3_0 + chebyshev_b_1 * x_3_1 - chebyshev_a_1 * y_3_1;
                end
                2'b10: begin 
                    y_1_0 <= elliptic_b_0 * x_1_0 + elliptic_b_1 * x_1_1 - elliptic_a_1 * y_1_1;
                    y_2_0 <= elliptic_b_0 * x_2_0 + elliptic_b_1 * x_2_1 - elliptic_a_1 * y_2_1;
                    y_3_0 <= elliptic_b_0 * x_3_0 + elliptic_b_1 * x_3_1 - elliptic_a_1 * y_3_1;
                end
                default: begin
                    y_1_0 <= 0;
                    y_2_0 <= 0;
                    y_3_0 <= 0;
                end
            endcase

            out_1 <= y_1_0;
            out_2 <= y_2_0;
            out_3 <= y_3_0;
        end
    end

    always @(*) begin
        case (filter_select)
            2'b00: begin 
                butterworth_a_0 = 16'h0000; butterworth_a_1 = 16'h0001;
                butterworth_b_0 = 16'h0001; butterworth_b_1 = 16'h0002;
            end
            2'b01: begin 
                chebyshev_a_0 = 16'h0000; chebyshev_a_1 = 16'h0003;
                chebyshev_b_0 = 16'h0004; chebyshev_b_1 = 16'h0005;
            end
            2'b10: begin
                elliptic_a_0 = 16'h0000; elliptic_a_1 = 16'h0006;
                elliptic_b_0 = 16'h0007; elliptic_b_1 = 16'h0008;
            end
            default: begin
                butterworth_a_0 = 0; butterworth_a_1 = 0;
                butterworth_b_0 = 0; butterworth_b_1 = 0;

                chebyshev_a_0 = 0; chebyshev_a_1 = 0;
                chebyshev_b_0 = 0; chebyshev_b_1 = 0;

                elliptic_a_0 = 0; elliptic_a_1 = 0;
                elliptic_b_0 = 0; elliptic_b_1 = 0;
            end
        endcase
    end

endmodule
