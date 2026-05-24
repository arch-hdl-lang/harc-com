module ScoreboardTypedQueue(
    input  logic        clk,
    input  logic        rst_n,
    output logic [63:0] dut_value,
    output logic [63:0] expected_value
);
    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            dut_value <= 64'd0;
            expected_value <= 64'd0;
        end else begin
            dut_value <= 64'd7;
            expected_value <= 64'd9;
        end
    end
endmodule
