module secure_variable_timer (
    input  wire        i_clk,         // Clock signal (rising-edge triggered)
    input  wire        i_rst_n,       // Active-low synchronous reset signal
    input  wire        i_data_in,     // Serial data input
    output reg  [3:0]  o_time_left,   // 4-bit output showing remaining time (during counting phase)
    output reg         o_processing,  // Asserted high when the timer is actively counting
    output reg         o_completed,   // Asserted high when the timer completes its delay
    input  wire        i_ack          // Acknowledgment signal to reset after completion
);

    //--------------------------------------------------------------------
    // 1) State Definitions
    //--------------------------------------------------------------------
    localparam  STATE_IDLE       = 2'b00;  // Waiting for 1101 pattern
    localparam  STATE_DELAY_CFG  = 2'b01;  // Reading 4 bits of delay
    localparam  STATE_COUNT      = 2'b10;  // Counting phase
    localparam  STATE_DONE       = 2'b11;   // Timer done, waiting for i_ack

    reg [1:0] current_state;
    reg [1:0] next_state;

    //--------------------------------------------------------------------
    // 2) Internal Registers
    //--------------------------------------------------------------------
    // Shift register for detecting start pattern (1101)
    reg [3:0] pattern_shift;
    // Counter to know how many bits of delay have been read
    reg [1:0] delay_bit_count;
    // Temporary shift register for the 4-bit delay value
    reg [3:0] delay_shift;


    // Sub-counter for 1000-cycle chunks
    // Counts from 0 to 999, then resets
    reg [9:0] sub_count;

    //--------------------------------------------------------------------
    // 3) State Machine - Sequential Logic
    //--------------------------------------------------------------------
    always @(posedge i_clk or negedge i_rst_n) begin
        if (!i_rst_n) begin
            // Synchronous reset (active low)
            current_state   <= STATE_IDLE;
            pattern_shift   <= 4'b0;
            delay_bit_count <= 2'd0;
            delay_shift     <= 4'b0;
            sub_count       <= 10'd0;
            o_time_left     <= 4'b0;
        end
        else begin
            current_state <= next_state;

            case (current_state)

            //--------------------------------------------------------------
            // STATE_IDLE: Search for the 1101 pattern in i_data_in
            //--------------------------------------------------------------
            STATE_IDLE: begin
                // Shift in the new bit each clock
                // pattern_shift[3] is the oldest bit, pattern_shift[0] is the newest
                pattern_shift <= {pattern_shift[2:0], i_data_in};

                // Once we detect the pattern 1101, we’ll move to STATE_DELAY_CFG
                // (We can check the pattern right after we shift in the new bit.)
                if (pattern_shift == 4'b1101) begin
                    // Prepare for delay configuration
                    delay_bit_count <= 2'd0;
                    delay_shift     <= 4'b0;
                end
            end

            //--------------------------------------------------------------
            // STATE_DELAY_CFG: Read next 4 bits as the delay
            //--------------------------------------------------------------
            STATE_DELAY_CFG: begin
                // Shift in the next bit from i_data_in
                delay_shift <= {delay_shift[2:0], i_data_in};
                delay_bit_count <= delay_bit_count + 1'b1;
                // Once we have collected 4 bits, latch them as the final delay
                if (delay_bit_count == 2'd2) begin
                    o_time_left <= {delay_shift[2:0], i_data_in}; 
                    
                end
            end

            //--------------------------------------------------------------
            // STATE_COUNT: Counting phase
            //--------------------------------------------------------------
            STATE_COUNT: begin
                // Sub-counter increments every clock
                sub_count <= sub_count + 1'b1;
                // After 1000 cycles, sub_count resets and o_time_left decrements
                if (sub_count == 10'd999) begin
                    sub_count   <= 10'd0;
                    // Decrement time_left if not already zero
                    if (o_time_left != 4'd0)
                        o_time_left <= o_time_left - 1'b1;
                end
            end

            //--------------------------------------------------------------
            // STATE_DONE: Timer complete, o_completed=1, waiting for i_ack
            //--------------------------------------------------------------
            STATE_DONE: begin
                pattern_shift   <= 4'b0;
                delay_bit_count <= 2'd0;
                delay_shift     <= 4'b0;
                // Remain here until i_ack is seen
                // (No special sequential logic needed here.)
            end

            endcase
        end
    end

    //--------------------------------------------------------------------
    // 4) State Machine - Combinational Next-State Logic
    //--------------------------------------------------------------------
    always @* begin

        case (current_state)

        //--------------------------------------------------------------
        // STATE_IDLE
        //--------------------------------------------------------------
        STATE_IDLE: begin
            // Remain in IDLE unless we detect '1101' in pattern_shift
            if (pattern_shift == 4'b1101) begin
                next_state = STATE_DELAY_CFG;
                o_completed = 1'b0; 
                o_processing = 1'b0;
            end
            else begin
                next_state = STATE_IDLE;
                o_completed = 1'b0; 
                o_processing = 1'b0;
            end
        end

        //--------------------------------------------------------------
        // STATE_DELAY_CFG
        //--------------------------------------------------------------
        STATE_DELAY_CFG: begin
            // Wait until we've read 4 bits total (delay_bit_count = 4)
            if (delay_bit_count == 2'd3) begin
                // Move to COUNT state
                next_state = STATE_COUNT;
                o_processing = 1'b1; // During counting phase, we set o_processing=1
                o_completed = 1'b0; 
            end
            else begin
                next_state = STATE_DELAY_CFG;
                o_completed = 1'b0; 
                o_processing = 1'b0;
            end
        end

        //--------------------------------------------------------------
        // STATE_COUNT
        //--------------------------------------------------------------
        STATE_COUNT: begin
            

            // We count for (delay_reg + 1) * 1000 cycles
            // The easiest check: if o_time_left == 0 AND sub_count == 999,
            // then we have just finished the final chunk of 1000 cycles.
            if ((o_time_left == 4'd0) && (sub_count == 10'd999)) begin
                // Counting complete
                next_state = STATE_DONE;
                o_completed = 1'b1; // Signal completion
                o_processing = 1'b0;
            end
            else begin
                next_state = STATE_COUNT;
                o_processing = 1'b1;
                o_completed = 1'b0; 
            end
        end

        //--------------------------------------------------------------
        // STATE_DONE
        //--------------------------------------------------------------
        STATE_DONE: begin

            // Once i_ack is asserted, go back to STATE_IDLE
            // to look for next 1101 pattern
            if (i_ack == 1'b1) begin
                next_state = STATE_IDLE;
                o_processing = 1'b0;
                o_completed = 1'b0;
            end
            else begin
                next_state = STATE_DONE;
                o_completed = 1'b1;
                o_processing = 1'b0;
            end
        end

        endcase
    end



endmodule