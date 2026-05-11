module iir_filter (
    input logic clk,
    input logic rst,
    input logic signed [15:0] x,    // Input sample
    output logic signed [15:0] y    // Output sample
);

    // Filter coefficients
    parameter signed [15:0] b0 = 16'h0F00;
    parameter signed [15:0] b1 = 16'h0E00;
    parameter signed [15:0] b2 = 16'h0D00;
    parameter signed [15:0] b3 = 16'h0C00;
    parameter signed [15:0] b4 = 16'h0B00;
    parameter signed [15:0] b5 = 16'h0A00;
    parameter signed [15:0] b6 = 16'h0900;
    parameter signed [15:0] a1 = -16'h0800;
    parameter signed [15:0] a2 = -16'h0700;
    parameter signed [15:0] a3 = -16'h0600;
    parameter signed [15:0] a4 = -16'h0500;
    parameter signed [15:0] a5 = -16'h0400;
    parameter signed [15:0] a6 = -16'h0300;

    // Explicitly declare each state variable
    logic signed [15:0] x_prev1, x_prev2, x_prev3, x_prev4, x_prev5, x_prev6;
    logic signed [15:0] y_prev1, y_prev2, y_prev3, y_prev4, y_prev5, y_prev6;

    always_ff @(posedge clk or posedge rst) begin
        if (rst) begin
            x_prev1 <= 0; x_prev2 <= 0; x_prev3 <= 0;
            x_prev4 <= 0; x_prev5 <= 0; x_prev6 <= 0;
            y_prev1 <= 0; y_prev2 <= 0; y_prev3 <= 0;
            y_prev4 <= 0; y_prev5 <= 0; y_prev6 <= 0;
            y <= 0;
        end else begin
            // Calculate the filter output
            y <= (b0 * x + b1 * x_prev1 + b2 * x_prev2 + b3 * x_prev3 +
                  b4 * x_prev4 + b5 * x_prev5 + b6 * x_prev6 -
                  a1 * y_prev1 - a2 * y_prev2 - a3 * y_prev3 -
                  a4 * y_prev4 - a5 * y_prev5 - a6 * y_prev6) >>> 16;

            // Update the historical data
            x_prev6 <= x_prev5; x_prev5 <= x_prev4; x_prev4 <= x_prev3;
            x_prev3 <= x_prev2; x_prev2 <= x_prev1; x_prev1 <= x;
            y_prev6 <= y_prev5; y_prev5 <= y_prev4; y_prev4 <= y_prev3;
            y_prev3 <= y_prev2; y_prev2 <= y_prev1; y_prev1 <= y;
        end
    end

endmodule