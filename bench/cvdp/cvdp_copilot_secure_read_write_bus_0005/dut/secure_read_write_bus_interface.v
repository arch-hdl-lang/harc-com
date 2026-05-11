module secure_read_write_bus_interface #(
    parameter p_configurable_key = 8'hAA,      // Default key
    parameter p_data_width       = 8,         // Data width
    parameter p_addr_width       = 8          // Address width
)(
    input  wire [p_addr_width-1:0] i_addr,            // Address input
    input  wire [p_data_width-1:0] i_data_in,         // Data to write
    input  wire [7:0]              i_key_in,         // Input key for authentication
    input  wire                    i_read_write_enable, // 1: Read, 0: Write
    input  wire                    i_capture_pulse,  // Qualifies input capture
    input  wire                    i_reset_bar,      // Active-low reset
    output reg  [p_data_width-1:0] o_data_out,       // Data output during read
    output reg                     o_error           // Error flag for unauthorized access
);

    // Internal memory declaration
    reg [p_data_width-1:0] memory [(2**p_addr_width)-1:0]; // Memory array

    // Internal key comparison logic
    wire key_match = (i_key_in == p_configurable_key); // Check if keys match

    // Sequential logic: Operates on i_capture_pulse edge or asynchronous reset
    always @(posedge i_capture_pulse or negedge i_reset_bar) begin
        if (!i_reset_bar) begin
            // Reset all outputs and states
            o_data_out <= {p_data_width{1'b0}};
            o_error    <= 1'b0;
        end else begin
            // Handle Read/Write operations based on enable signal
            if (key_match) begin
                o_error <= 1'b0; // Clear error flag if key matches
                if (i_read_write_enable) begin
                    // Read operation
                    o_data_out <= memory[i_addr];
                end else begin
                    // Write operation
                    memory[i_addr] <= i_data_in;
                    o_data_out     <= {p_data_width{1'b0}}; // Clear output during write
                end
            end else begin
                // Key mismatch: Raise error and clear outputs
                o_error    <= 1'b1;
                o_data_out <= {p_data_width{1'b0}};
            end
        end
    end
endmodule
