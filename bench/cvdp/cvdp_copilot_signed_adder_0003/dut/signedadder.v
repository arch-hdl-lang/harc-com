module signedadder #(parameter DATA_WIDTH = 8)(
    input i_clk,
    input i_rst_n,
    input i_start,
    input i_enable,
    input i_mode,
    input i_clear,
    input [DATA_WIDTH-1:0] i_operand_a,
    input [DATA_WIDTH-1:0] i_operand_b,
    output reg [DATA_WIDTH-1:0] o_resultant_sum,
    output reg o_overflow,
    output reg o_ready,
    output [1:0] o_status
);

    // State encoding
    localparam IDLE    = 2'b00;
    localparam LOAD    = 2'b01;
    localparam COMPUTE = 2'b10;
    localparam OUTPUT  = 2'b11;

    // Internal registers
    reg [DATA_WIDTH-1:0] reg_operand_a;
    reg [DATA_WIDTH-1:0] reg_operand_b;
    reg [1:0] state;
    reg [DATA_WIDTH-1:0] result;
    reg signed_overflow;

    assign o_status = state;
    // Synchronous reset and state transitions
    always @(posedge i_clk or negedge i_rst_n) begin
        if (!i_rst_n) begin
            state <= IDLE;
            o_resultant_sum <= 0;
            o_overflow <= 0;
            o_ready <= 0;
            reg_operand_a <= 0;
            reg_operand_b <= 0;
        end else begin
            if (i_clear) begin
                o_resultant_sum <= 0;
                state <= IDLE;
            end else begin
                case (state)
                    IDLE: begin
                        o_ready <= 0;
                        o_overflow <= 0;
                        if (i_enable && i_start) begin
                            state <= LOAD;
                        end
                    end

                    LOAD: begin
                        if (i_enable) begin
                            reg_operand_a <= i_operand_a;
                            reg_operand_b <= i_operand_b;
                            state <= COMPUTE;
                        end else begin
                            state <= IDLE;
                        end
                    end

                    COMPUTE: begin
                        if (i_enable) begin
                            if (i_mode == 0) begin
                                result = reg_operand_a + reg_operand_b;
                                signed_overflow = 
                                    (~reg_operand_a[DATA_WIDTH-1] & ~reg_operand_b[DATA_WIDTH-1] & result[DATA_WIDTH-1]) | 
                                    (reg_operand_a[DATA_WIDTH-1] & reg_operand_b[DATA_WIDTH-1] & ~result[DATA_WIDTH-1]);
                            end else begin
                                result = reg_operand_a - reg_operand_b;
                                signed_overflow = 
                                    (~reg_operand_a[DATA_WIDTH-1] & reg_operand_b[DATA_WIDTH-1] & result[DATA_WIDTH-1]) | 
                                    (reg_operand_a[DATA_WIDTH-1] & ~reg_operand_b[DATA_WIDTH-1] & ~result[DATA_WIDTH-1]);
                            end
                            state <= OUTPUT;
                        end else begin
                            state <= IDLE;
                        end
                    end

                    OUTPUT: begin
                        if (i_enable) begin
                            o_resultant_sum <= result;
                            o_overflow <= signed_overflow;
                            o_ready <= 1;
                            state <= IDLE;
                        end else begin
                            state <= IDLE;
                        end
                    end
                endcase
            end
        end
    end
endmodule
