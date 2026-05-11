module adc_data_rotate #(
    parameter DATA_WIDTH = 8 // Parameterized data width (default = 8)
)(
    // Inputs
    input  logic                     i_clk,             // Clock signal
    input  logic                     i_rst_n,           // Active-low reset
    input  logic [DATA_WIDTH-1:0]    i_adc_data_in,     // Input ADC data
    input  logic [3:0]               i_shift_count,     // Number of bits to rotate
    input  logic                     i_shift_direction, // Shift direction (0: Left, 1: Right)

    // Outputs
    output logic [DATA_WIDTH-1:0]    o_processed_data,  // Rotated output data
    output logic                     o_operation_status // Operation status
);

    // Internal signal to hold the effective shift count
    // Taking modulo with DATA_WIDTH ensures correct behavior for shifts > DATA_WIDTH
    logic [DATA_WIDTH-1:0] shift_count_mod;

    // Calculate effective shift count
    // Since i_shift_count is 4 bits, the maximum shift count is 15.
    // We only need to limit it to DATA_WIDTH-1 if DATA_WIDTH < 16.
    assign shift_count_mod = i_shift_count % DATA_WIDTH;

    // Synchronous logic
    always_ff @(posedge i_clk or negedge i_rst_n) begin
        if (!i_rst_n) begin
            // Reset condition: outputs go to idle states
            o_processed_data   <= '0;
            o_operation_status <= 1'b0;
        end
        else begin
            // Set operation_status high to indicate active rotation
            o_operation_status <= 1'b1;

            // Perform rotation based on i_shift_direction
            if (i_shift_direction == 1'b0) begin
                // Left rotate
                // Bits shifted out of the left re-enter on the right
                o_processed_data <= (i_adc_data_in << shift_count_mod) |
                                    (i_adc_data_in >> (DATA_WIDTH - shift_count_mod));
            end
            else begin
                // Right rotate
                // Bits shifted out of the right re-enter on the left
                o_processed_data <= (i_adc_data_in >> shift_count_mod) |
                                    (i_adc_data_in << (DATA_WIDTH - shift_count_mod));
            end
        end
    end

endmodule