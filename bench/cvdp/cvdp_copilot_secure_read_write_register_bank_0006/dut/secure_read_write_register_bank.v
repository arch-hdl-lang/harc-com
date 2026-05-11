// -----------------------------------------------------------------------------
// Module Name: secure_read_write_register_bank
// Description: 
// This module implements a secure, parameterized register bank with access 
// control. It uses a state machine to enforce an unlocking mechanism for 
// read/write access. Access to the register bank is allowed only after a 
// two-step unlock sequence is completed by writing specific unlock codes to 
// predefined addresses. The register bank operates in a restricted mode until 
// the correct unlock sequence is performed.
// 
// Key Features:
// - Parameterized address and data widths
// - Configurable unlock codes for security
// - Read and write access controlled by a security FSM
// - Addresses `0` and `1` are restricted to write-only operations
// - Reset mechanism to relock the register bank
// 
// -----------------------------------------------------------------------------

module secure_read_write_register_bank #(
    parameter p_address_width = 8,                // Address width parameter (default 8 bits)
    parameter p_data_width = 8,                   // Data width parameter (default 8 bits)
    parameter p_unlock_code_0 = 8'hAB,            // Unlock code for address 0
    parameter p_unlock_code_1 = 8'hCD             // Unlock code for address 1
)(
    input  wire                         i_rst_n,            // Asynchronous active-low reset
    input  wire [p_address_width-1:0]   i_addr,             // Address input for read/write operations
    input  wire [p_data_width-1:0]      i_data_in,          // Data input for write operations
    input  wire                         i_read_write_enable, // Control signal: 1 = Read, 0 = Write
    input  wire                         i_capture_pulse,    // Capture pulse (acts as clock for the register bank)
    output reg  [p_data_width-1:0]      o_data_out          // Data output for read operations
);

    // -------------------------------------------------------------------------
    // Internal Memory: Register Bank
    // -------------------------------------------------------------------------
    // A 2^p_address_width-sized register bank to hold data. Each register is 
    // p_data_width bits wide. This memory is accessed using the `i_addr` signal.
    reg [p_data_width-1:0] r_register_bank [0:(1<<p_address_width)-1];
    
    // -------------------------------------------------------------------------
    // Unlock State Machine
    // -------------------------------------------------------------------------
    // Tracks the state of the unlock mechanism:
    // - LOCKED: No access to register bank except for write-only access to 
    //   addresses 0 and 1 to attempt unlocking.
    // - UNLOCK_STEP1: First step of the unlock sequence completed.
    // - UNLOCKED: Full access granted.
    reg [1:0] r_unlock_state;  // Unlock FSM state

    // State Encoding
    localparam p_STATE_LOCKED       = 2'b00; // Locked state (initial state)
    localparam p_STATE_UNLOCK_STEP1 = 2'b01; // First unlock step complete
    localparam p_STATE_UNLOCKED     = 2'b11; // Register bank fully unlocked

    // -------------------------------------------------------------------------
    // Unlock FSM Logic
    // -------------------------------------------------------------------------
    // The FSM transitions between LOCKED, UNLOCK_STEP1, and UNLOCKED states 
    // based on the address and data written to the register bank. Incorrect 
    // unlock codes or incorrect sequences reset the FSM back to LOCKED.
    always @(posedge i_capture_pulse or negedge i_rst_n) begin
        if (!i_rst_n) begin
            // Reset the FSM to the locked state on active-low reset
            r_unlock_state <= p_STATE_LOCKED;
        end else begin
            case (r_unlock_state)
                p_STATE_LOCKED: begin
                    // Check if the first unlock code is written to address 0
                    if ((i_addr == 0) && (i_data_in == p_unlock_code_0) && (!i_read_write_enable)) begin
                        r_unlock_state <= p_STATE_UNLOCK_STEP1; // Move to step 1
                    end
                end

                p_STATE_UNLOCK_STEP1: begin
                    // Check if the second unlock code is written to address 1
                    if ((i_addr == 1) && (i_data_in == p_unlock_code_1) && (!i_read_write_enable)) begin
                        r_unlock_state <= p_STATE_UNLOCKED; // Unlock successful
                    end 
                    // If any other address is tried to be written, reset to LOCKED
                    else begin
                        r_unlock_state <= p_STATE_LOCKED;
                    end
                end

                p_STATE_UNLOCKED: begin
                    // If incorrect code is written to address 0 or 1, relock the register bank
                    if (((i_addr == 1) && (i_data_in != p_unlock_code_1) && (!i_read_write_enable)) ||
                        ((i_addr == 0) && (i_data_in != p_unlock_code_0) && (!i_read_write_enable))) begin
                        r_unlock_state <= p_STATE_LOCKED;
                    end
                end

                default: begin
                    r_unlock_state <= p_STATE_LOCKED; // Default to LOCKED
                end
            endcase
        end
    end

    // -------------------------------------------------------------------------
    // Read and Write Operations
    // -------------------------------------------------------------------------
    always @(posedge i_capture_pulse) begin
        if (r_unlock_state == p_STATE_UNLOCKED) begin
            // Unlocked: Full access to the register bank
            if (i_read_write_enable) begin
                // Read operation
                if (i_addr == 0 || i_addr == 1) begin
                    o_data_out <= 0; // Addresses 0 and 1 are write-only
                end else begin
                    o_data_out <= r_register_bank[i_addr]; // Output data at address
                end
            end else begin
                // Write operation
                r_register_bank[i_addr] <= i_data_in; // Write data to register
                o_data_out <= 0; // Default output during write
            end
        end else begin
            // Locked: Restricted access
            if (i_read_write_enable) begin
                o_data_out <= 0; // Locked state: read outputs 0
            end else begin
                // Allow writes only to addresses 0 and 1
                if (i_addr == 0 || i_addr == 1) begin
                    r_register_bank[i_addr] <= i_data_in;
                end
                o_data_out <= 0; // Default output during write
            end
        end
    end

endmodule
