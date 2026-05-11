`timescale 1ns / 1ps
module restoring_division #(parameter WIDTH = 6) (
    input        clk,                       // Clock signal
    input        rst,                       // Reset signal (active low)
    input        start,                     // Start signal to begin the division process
    input  [WIDTH-1:0] dividend,            // Dividend input
    input  [WIDTH-1:0] divisor,             // Divisor input
    output [WIDTH-1:0] quotient,            // Quotient output
    output [WIDTH-1:0] remainder,           // Remainder output
    output reg        valid,                // Valid output indicating completion of division
    output reg        divisor_valid_result  // Divisor divisor_valid_result signal
);
    // Registers width declaration
    localparam COUNT_WIDTH = $clog2(WIDTH);
    localparam ACCUMULATOR_WIDTH = 2*WIDTH;
    // FSM state encoding
    localparam IDLE  = 1'b0;                                    // Idle state
    localparam START = 1'b1;                                    // Start state (active division)
    reg [ACCUMULATOR_WIDTH-1:0] accumulator;                    // Accumulator to store intermediate results
    reg [ACCUMULATOR_WIDTH-1:0] next_accumulator;               // Next state of the accumulator
    reg [ACCUMULATOR_WIDTH-1:0] accumulator_temp;               // Temporary register for accumulator shifting
    reg [ACCUMULATOR_WIDTH-1:0] accumulator_temp1;              // Temporary register for subtraction and concatenation
    reg                        next_state, present_state;       // FSM states for control logic
    reg [COUNT_WIDTH-1:0]      count;                           // Counter for tracking the number of shifts
    reg [COUNT_WIDTH-1:0]      next_count;                      // Next state of the counter
    reg                        next_valid;                      // Next state of the valid signal
    reg                        next_divisor_valid_result;       // Divisor next_divisor_valid_result signal
   
    // Combinational logic block to determine the next state and outputs
    always @ (*) begin                  
        case(present_state)
            IDLE: begin
                next_count = {COUNT_WIDTH{1'b0}};                               // Reset the counter to 0
                next_valid = 1'b0;                                              // Clear the valid signal
                next_divisor_valid_result  = 1'b1;                              // Clear the divisor_valid_result     
                if (start) begin
                    // Special cases
                    if (divisor == 0) begin                                     // Division by zero case
                        next_state = IDLE;
                        next_valid = 1'b1;
                        next_accumulator = {dividend, {WIDTH{1'b0}}};
                        next_divisor_valid_result = 1'b0;
                    end 
                    else if (dividend == 0) begin                               // Dividend is zero case
                        next_state = IDLE;
                        next_valid = 1'b1;
                        next_accumulator = {ACCUMULATOR_WIDTH{1'b0}};           // Both quotient and remainder are zero
                        next_divisor_valid_result = 1'b1;
                    end 
                    else if (dividend < divisor) begin                          // Dividend is less than divisor case
                        next_state = IDLE;
                        next_valid = 1'b1;
                        next_accumulator = {dividend, {WIDTH{1'b0}}};           // Quotient is zero, remainder is the dividend
                        next_divisor_valid_result = 1'b1;
                    end 
                    else begin                                                  // Valid division process
                        next_state = START;
                        next_accumulator = { {WIDTH{1'b0}}, dividend };         // Initialize the accumulator with the dividend
                    end
                end 
                else begin
                    next_state = present_state;                                 // Remain in IDLE state
                    next_accumulator = {ACCUMULATOR_WIDTH{1'b0}};               // Keep the accumulator cleared
                end
            end
            START: begin
                if (next_count == WIDTH) begin                                  // Check if the division process is complete
                    next_count = {COUNT_WIDTH{1'b0}};                           // Reset the counter
                    next_state = present_state;                                 // Transition back to IDLE or present_state state
                    next_valid = 1'b1;                                          // Set the valid signal to indicate completion
                    next_divisor_valid_result = 1'b1;                           // Set the divisor_valid_result signal to indicate completion
                end else begin
                    next_count = count + 1'b1;                                  // Increment the counter
                    accumulator_temp = accumulator << 1;                        // Left shift the accumulator
                    accumulator_temp1 = {accumulator_temp[ACCUMULATOR_WIDTH-1:WIDTH] - divisor, accumulator_temp[WIDTH-1:0]}; // Subtract divisor and concatenate
                    // Update the accumulator based on the subtraction result
                    next_accumulator = accumulator_temp1[ACCUMULATOR_WIDTH-1] ? 
                                       {accumulator_temp[ACCUMULATOR_WIDTH-1:WIDTH], accumulator_temp[WIDTH-1:1], 1'b0} :
                                       {accumulator_temp1[ACCUMULATOR_WIDTH-1:WIDTH], accumulator_temp[WIDTH-1:1], 1'b1}; 
                    next_valid = (&count) ? 1'b1 : 1'b0;                        // Set valid signal if all bits have been processed
                    next_state = (valid) ? IDLE : present_state;                // Transition to IDLE if done, otherwise stay in START
                end
            end
        endcase
    end
    
    // Sequential logic block to update the current state and outputs on the clock edge or reset
    always @ (posedge clk or negedge rst) begin
        if (!rst) begin
            accumulator            <= {ACCUMULATOR_WIDTH{1'b0}};                // Clear the accumulator on reset
            valid                  <= 1'b0;                                     // Clear the valid signal on reset
            present_state          <= IDLE;                                     // Set the FSM to IDLE state on reset
            count                  <= {COUNT_WIDTH{1'b0}};                      // Reset the counter on reset 
            divisor_valid_result   <= 1'b0;                                     // Clear the divisor_valid_result signal on reset
        end else begin
            accumulator            <= next_accumulator;                         // Update the accumulator
            valid                  <= next_valid;                               // Update the valid signal
            present_state          <= next_state;                               // Update the FSM state
            count                  <= next_count;                               // Update the counter
            divisor_valid_result   <= next_divisor_valid_result;                // Update the divisor_valid_result signal
        end
    end
    
    // Assign outputs for quotient and remainder based on the accumulator value
    assign remainder = accumulator[ACCUMULATOR_WIDTH-1:WIDTH];                  // Upper half of the accumulator is the remainder
    assign quotient  = accumulator[WIDTH-1:0];                                  // Lower half of the accumulator is the quotient
endmodule