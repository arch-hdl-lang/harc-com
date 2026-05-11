`timescale 1ns/1ps
module divider #
(
    parameter WIDTH = 32
)
(
    input  wire                  clk,
    input  wire                  rst_n,      // Active-low asynchronous reset
    input  wire                  start,      // Start signal for new operation
    input  wire [WIDTH-1 : 0]    dividend,   // Dividend (numerator)
    input  wire [WIDTH-1 : 0]    divisor,    // Divisor (denominator)
    output wire [WIDTH-1 : 0]    quotient,   // Result of the division
    output wire [WIDTH-1 : 0]    remainder,  // Remainder after division
    output wire                  valid       // Indicates output is valid
);

    // Simple 3-state FSM
    localparam IDLE = 2'b00;
    localparam BUSY = 2'b01;
    localparam DONE = 2'b10;

    reg [1:0] state_reg, state_next;

    // A+Q combined into one 2*WIDTH register:
    reg [2*WIDTH-1:0] aq_reg, aq_next;

    // Divisor register
    reg [WIDTH-1:0] m_reg, m_next;

    // Iterate exactly WIDTH times
    reg [$clog2(WIDTH)-1:0] n_reg, n_next;

    // Final outputs
    reg [WIDTH-1:0] quotient_reg, quotient_next;
    reg [WIDTH-1:0] remainder_reg, remainder_next;
    reg valid_reg, valid_next;

    // Assign the top-level outputs
    assign quotient  = quotient_reg;
    assign remainder = remainder_reg;
    assign valid     = valid_reg;

    //------------------------------------------------
    // SEQUENTIAL: State & register updates
    //------------------------------------------------
    always @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            state_reg     <= IDLE;
            aq_reg        <= 0;
            m_reg         <= 0;
            n_reg         <= 0;
            quotient_reg  <= 0;
            remainder_reg <= 0;
            valid_reg     <= 0;
        end
        else begin
            state_reg     <= state_next;
            aq_reg        <= aq_next;
            m_reg         <= m_next;
            n_reg         <= n_next;
            quotient_reg  <= quotient_next;
            remainder_reg <= remainder_next;
            valid_reg     <= valid_next;
        end
    end

    //------------------------------------------------
    // COMBINATIONAL: Next-state logic
    //------------------------------------------------
    always @* begin
        // Default "hold" behavior
        state_next     = state_reg;
        aq_next        = aq_reg;
        m_next         = m_reg;
        n_next         = n_reg;
        quotient_next  = quotient_reg;
        remainder_next = remainder_reg;
        valid_next     = valid_reg;

        case (state_reg)
        //---------------------------------------------
        // IDLE: Wait for start
        //---------------------------------------------
        IDLE: begin
            // Outputs not valid yet
            valid_next = 1'b0;

            if (start) begin
                // Step-1: Initialize
                // A=0 => top WIDTH bits of aq_reg = 0
                // Q=dividend => lower WIDTH bits
                aq_next  = { {WIDTH{1'b0}}, dividend };
                m_next   = divisor;
                n_next   = WIDTH;
                // We do not set the final quotient/remainder yet
                // Move to BUSY
                state_next = BUSY;
            end
        end

        //---------------------------------------------
        // BUSY: Perform the N iterations
        //---------------------------------------------
        BUSY: begin
            // 1) SHIFT LEFT
            aq_next = aq_reg << 1;

            // 2) If sign bit of old A == 1 => add M; else => subtract M.
            if (aq_reg[2*WIDTH-1] == 1'b1) begin
                aq_next[2*WIDTH-1 : WIDTH] = aq_next[2*WIDTH-1 : WIDTH] + m_reg;
            end
            else begin
                aq_next[2*WIDTH-1 : WIDTH] = aq_next[2*WIDTH-1 : WIDTH] - m_reg;
            end

            // 3) Check the new sign bit of A (after the add/sub).
            //    If sign(A) = 1 => Q[0] = 0; else => Q[0] = 1.
            if (aq_next[2*WIDTH-1] == 1'b1) begin
                aq_next[0] = 1'b0;
            end
            else begin
                aq_next[0] = 1'b1;
            end

            // Decrement iteration count
            n_next = n_reg - 1;

            // Step-7: if n != 0 => repeat, else => next step
            if (n_next == 0) begin
                state_next = DONE;
            end
        end

        //---------------------------------------------
        // DONE: Final correction + output
        //---------------------------------------------
        DONE: begin
            // Step-8: If sign bit of A == 1 => A = A + M
            if (aq_reg[2*WIDTH-1] == 1'b1) begin
                aq_next = aq_reg;  // default
                aq_next[2*WIDTH-1 : WIDTH] = aq_reg[2*WIDTH-1 : WIDTH] + m_reg;
            end

            // Step-9: 
            //   - The final quotient is the lower WIDTH bits of aq_reg
            //   - The final remainder is the upper WIDTH bits of aq_reg
            quotient_next  = aq_next[WIDTH-1 : 0];
            remainder_next = aq_next[2*WIDTH-1 : WIDTH];

            // Output becomes valid
            valid_next = 1'b1;

            // Return to IDLE when 'start' goes low
            if (!start) begin
                state_next = IDLE;
            end else begin
                state_next = DONE;
            end
        end

        default: begin
            state_next = IDLE;
        end
        endcase
    end

endmodule