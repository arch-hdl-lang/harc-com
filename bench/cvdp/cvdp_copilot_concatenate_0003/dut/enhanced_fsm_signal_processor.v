// Verilog RTL Design for enhanced_fsm_signal_processor

module enhanced_fsm_signal_processor (
    input wire i_clk,            // Clock signal
    input wire i_rst_n,          // Active-low reset signal
    input wire i_enable,         // Enable signal to start processing
    input wire i_clear,          // Signal to clear fault state and reset outputs
    input wire i_ack,            // Acknowledge signal to transition from READY to IDLE
    input wire i_fault,          // Fault condition signal
    input wire [4:0] i_vector_1, // Input vector 1
    input wire [4:0] i_vector_2, // Input vector 2
    input wire [4:0] i_vector_3, // Input vector 3
    input wire [4:0] i_vector_4, // Input vector 4
    input wire [4:0] i_vector_5, // Input vector 5
    input wire [4:0] i_vector_6, // Input vector 6

    output reg o_ready,          // Indicates when processing is complete
    output reg o_error,          // Indicates fault condition
    output reg [1:0] o_fsm_status, // Current FSM state
    output reg [7:0] o_vector_1, // Processed output vector 1
    output reg [7:0] o_vector_2, // Processed output vector 2
    output reg [7:0] o_vector_3, // Processed output vector 3
    output reg [7:0] o_vector_4  // Processed output vector 4
);

// FSM state definitions
localparam IDLE   = 2'b00;
localparam PROCESS = 2'b01;
localparam READY   = 2'b10;
localparam FAULT   = 2'b11;

// Internal registers
reg [1:0] current_state, next_state;
reg [31:0] concatenated_vector;

// Synchronous state transition and reset handling
always @(posedge i_clk or negedge i_rst_n) begin
    if (!i_rst_n) begin
        current_state <= IDLE;   // Reset state to IDLE
    end else begin
        current_state <= next_state;
    end
end

// Next state logic
always @(*) begin
    case (current_state)
        IDLE: begin
            if (i_fault) begin
                next_state = FAULT;
            end 
            else begin
                if (i_enable) begin
                next_state = PROCESS;
                end
                else next_state = current_state;
            end
        end
        PROCESS: begin
            if (i_fault) begin
                next_state = FAULT;
            end else begin
                next_state = READY;
            end
        end
        READY: begin
            if (i_fault) begin
                next_state = FAULT;
            end 
            else begin
                if (i_ack) begin
                    next_state = IDLE;
                end
                else next_state = current_state;
            end
        end
        FAULT: begin
            if (i_clear && !i_fault) begin
                next_state = IDLE;
            end
            else next_state = current_state;
        end
    endcase
end

// Output logic and processing
always @(posedge i_clk or negedge i_rst_n) begin
    if (!i_rst_n) begin
        o_ready <= 0;
        o_error <= 0;
        o_fsm_status <= IDLE;
        o_vector_1 <= 0;
        o_vector_2 <= 0;
        o_vector_3 <= 0;
        o_vector_4 <= 0;
        concatenated_vector <= 0;
    end else begin
        o_fsm_status <= current_state;  // Update FSM status
        case (current_state)
            IDLE: begin
                o_ready <= 0;
                o_error <= 0;
                o_vector_1 <= 0;
                o_vector_2 <= 0;
                o_vector_3 <= 0;
                o_vector_4 <= 0;
            end
            PROCESS: begin
                concatenated_vector <= {i_vector_1, i_vector_2, i_vector_3, i_vector_4, i_vector_5, i_vector_6, 2'b11};
                end
            READY: begin
                o_vector_1 <= concatenated_vector[31:24]; // MSB
                o_vector_2 <= concatenated_vector[23:16];
                o_vector_3 <= concatenated_vector[15:8];
                o_vector_4 <= concatenated_vector[7:0];  // LSB
                o_ready <= 1;
            end
            FAULT: begin
                o_error <= 1;
                o_ready <= 0;
                o_vector_1 <= 0;
                o_vector_2 <= 0;
                o_vector_3 <= 0;
                o_vector_4 <= 0;
            end
        endcase
    end
end

endmodule