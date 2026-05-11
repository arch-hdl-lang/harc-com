`timescale 1ns / 1ps
module csr_apb_interface (
    input                    pclk,              // Clock input for synchronization
    input                    presetn,           // Active-low asynchronous reset input
    input [31:0]             paddr,             // APB address bus, 32-bit for register addressing
    input                    pselx,             // APB peripheral select signal
    input                    penable,           // APB enable signal for transactions
    input                    pwrite,            // APB write enable signal
    input [31:0]             pwdata,            // APB write data bus

    output reg               pready,            // APB ready signal, indicates end of transaction
    output reg [31:0]        prdata,            // APB read data bus
    output reg               pslverr            // APB slave error signal
);

    // Define register addresses
    localparam DATA_REG      = 32'h10; // Data register
    localparam CONTROL_REG   = 32'h14; // Control register
    localparam INTERRUPT_REG = 32'h18; // Interrupt configuration register

    // Define state machine states
    localparam IDLE          = 2'b00;
    localparam SETUP         = 2'b01;
    localparam READ_STATE    = 2'b10;
    localparam WRITE_STATE   = 2'b11;

    // Internal state registers
    reg [1:0]                present_state, next_state;

    // Signals for state transitions and outputs
    reg                      next_pready;
    reg [31:0]               next_prdata;
    reg                      next_pslverr;

    // Internal storage registers and next-state values
    reg [9:0]                data1, next_data1;
    reg [9:0]                data2, next_data2;
    reg [11:0]               data_reserived, data_next_reserived;

    // Control register bits and their next-state values
    reg                      enable;            // Control register: Enable bit
    reg                      mode;              // Control register: Mode selection bit
    reg [29:0]               CONTROL_reserived; // Reserved bits in control register

    // Interrupt flags and next-state values
    reg                      overflow_ie, next_overflow_ie;
    reg                      sign_ie, next_sign_ie;
    reg                      parity_ie, next_parity_ie;
    reg                      zero_ie, next_zero_ie;
    reg [27:0]               INTERRUPT_reserived, next_INTERRUPT_reserived;

    // Combinational logic to determine next state and outputs
    always @ (*) begin
        // Set default values for the next state outputs
        next_pready = pready;
        next_prdata = prdata;
        next_pslverr = pslverr;

        next_data1 = data1;
        next_data2 = data2;
        next_overflow_ie = overflow_ie;
        next_sign_ie = sign_ie;
        next_parity_ie = parity_ie;
        next_zero_ie = zero_ie;
        next_state = present_state;
        next_INTERRUPT_reserived = INTERRUPT_reserived;
        data_next_reserived = data_reserived;

        // State machine handling APB interface operations
        case (present_state)
            IDLE: begin
                if (pselx)
                    next_state = SETUP; // Transition to setup on select
            end
            
            SETUP: begin
                if (penable && pwrite)
                    next_state = WRITE_STATE; // Handle write transactions
                else if (penable)
                    next_state = READ_STATE; // Handle read transactions
            end

            READ_STATE: begin
                if (pready) begin
                    next_state = IDLE; // Return to IDLE after read
                    next_pready = 1'b0;
                end else begin
                    next_pready = 1'b1;
                    case (paddr) // Output data based on address
                        DATA_REG:      next_prdata = {data_reserived, data1, data2};
                        CONTROL_REG:   next_prdata = {CONTROL_reserived, enable, mode};
                        INTERRUPT_REG: next_prdata = {INTERRUPT_reserived, overflow_ie, sign_ie, parity_ie, zero_ie};
                        default:       next_pslverr = 1'b1; // Unknown address error
                    endcase
                end
            end

            WRITE_STATE: begin
                if (pready) begin
                    next_state = IDLE; // Return to IDLE after write
                    next_pready = 1'b0;
                end else begin
                    next_pready = 1'b1;
                    case (paddr) // Handle data based on address
                        DATA_REG: begin
                            next_data1 = pwdata[19:10];
                            next_data2 = pwdata[9:0];
                            data_next_reserived = pwdata[31:20];
                        end
                        CONTROL_REG: begin
                            enable = pwdata[1];
                            mode = pwdata[0];
                            CONTROL_reserived = pwdata[31:2];
                        end
                        INTERRUPT_REG: begin
                            next_overflow_ie = pwdata[3];
                            next_sign_ie = pwdata[2];
                            next_parity_ie = pwdata[1];
                            next_zero_ie = pwdata[0];
                            next_INTERRUPT_reserived = pwdata[31:4];
                        end
                        default: next_pslverr = 1'b1; 
                    endcase
                end
            end
        endcase
    end

    // Sequential logic to update state and outputs at clock edges or reset
    always @ (posedge pclk or negedge presetn) begin
        if (!presetn) begin
            // Initialize registers on reset
            pready          <= 1'b0;
            prdata          <= 32'h0;
            pslverr         <= 1'b0;

            data1           <= 10'd0;
            data2           <= 10'd0;
            overflow_ie     <= 1'b0;
            sign_ie         <= 1'b0;
            parity_ie       <= 1'b0;
            zero_ie         <= 1'b0;
            INTERRUPT_reserived       <= 28'b0;
            data_reserived       <= 12'b0;

            present_state   <= IDLE;
        end else begin
            // Update internal state and outputs based on next state values
            pready          <= next_pready;
            prdata          <= next_prdata;
            pslverr         <= next_pslverr;

            data1           <= next_data1;
            data2           <= next_data2;
            overflow_ie     <= next_overflow_ie;
            sign_ie         <= next_sign_ie;
            parity_ie       <= next_parity_ie;
            zero_ie         <= next_zero_ie;
            data_reserived  <= data_next_reserived;
            INTERRUPT_reserived  <= next_INTERRUPT_reserived;

            present_state   <= next_state;
        end
    end

endmodule