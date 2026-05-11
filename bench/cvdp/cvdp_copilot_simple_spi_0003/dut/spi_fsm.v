module spi_fsm (
    input  wire         i_clk,       // System clock
    input  wire         i_rst_b,     // Active-low async reset
    input  wire [15:0]  i_data_in,   // Parallel 16-bit data to transmit
    input  wire         i_enable,    // Enable block
    input  wire         i_fault,     // Fault indicator
    input  wire         i_clear,     // Forces FSM to clear/idle
    
    output reg          o_spi_cs_b,  // SPI chip select (active-low)
    output reg          o_spi_clk,   // SPI clock
    output reg          o_spi_data,  // Serialized SPI data out
    output reg [4:0]    o_bits_left, // Bits left to transmit
    output reg          o_done,      // Single-cycle pulse when done or error
    output reg [1:0]    o_fsm_state  // FSM state for external monitoring
);
    
    //--------------------------------------------------------------------------
    // Parameter Definitions
    //--------------------------------------------------------------------------
    localparam [1:0] ST_IDLE         = 2'b00,  // Idle state
                     ST_TRANSMIT     = 2'b01,  // Load/shift bits out
                     ST_CLOCK_TOGGLE = 2'b10,  // Toggle clock & shift data
                     ST_ERROR        = 2'b11;  // Fault state
    
    //--------------------------------------------------------------------------
    // Internal Signals
    //--------------------------------------------------------------------------
    reg [1:0]  curr_state, next_state;
    reg [15:0] shift_reg;        // Holds data to be shifted out
    reg [4:0]  next_bits_left;
    reg        next_done;
    
    //--------------------------------------------------------------------------
    // Synchronous State & Data Registers
    //--------------------------------------------------------------------------
    always @(posedge i_clk or negedge i_rst_b) begin
        if (!i_rst_b) begin
            curr_state   <= ST_IDLE;
            shift_reg    <= 16'd0;
            o_bits_left  <= 5'h10;
            o_done       <= 1'b0;
        end
        else begin
            // Update FSM state
            curr_state   <= next_state;
            
            // Update shift register (only in states that shift data)
            if (curr_state == ST_TRANSMIT ) begin
                shift_reg <= { shift_reg[14:0], 1'b0 }; // shift left
                o_bits_left <= next_bits_left;
            end
            else if ((curr_state==ST_IDLE) && i_enable) begin
                shift_reg <= i_data_in;
            end
            // else if(curr_state == ST_CLOCK_TOGGLE) begin
                
            // end
                        
            
            // Update 'done' (one-cycle pulse)
            o_done      <= next_done;
        end
    end
    
    //--------------------------------------------------------------------------
    // Next-State and output Logic (Combinational)
    //--------------------------------------------------------------------------
    always @(*) begin
        
        if(!i_rst_b) begin
            o_spi_cs_b     = 1'b1;       // Default de-asserted (active-low)
            o_spi_clk      = 1'b0;       // Default clock low when idle
            o_spi_data     = 1'b0;       // Default data low when idle
            o_fsm_state    = ST_IDLE;
            next_state     = ST_IDLE;
            next_bits_left  = 5'h10;
            next_done       = 1'b0;
        end
        else begin
            // If either clear or !enable is active, force IDLE
            if (!i_enable) begin
                o_spi_cs_b     = 1'b1;       // Default de-asserted (active-low)
                o_spi_clk      = 1'b0;       // Default clock low when idle
                o_spi_data     = 1'b0;       // Default data low when idle
                o_fsm_state    = ST_IDLE;
                next_state      = ST_IDLE;
                next_bits_left  = 5'h10;     // Reset bits_left to full (16 bits => 0x10; 0x10 can be a placeholder)
                next_done       = 1'b0;
            end
            // If fault is asserted, go to ERROR
            else if (i_fault) begin
                o_spi_cs_b     = 1'b1;       // Default de-asserted (active-low)
                o_spi_clk      = 1'b0;       // Default clock low when idle
                o_spi_data     = 1'b0;       // Default data low when idle
                next_state      = ST_ERROR;
                next_done       = 1'b0;
                o_fsm_state = ST_ERROR;
                next_bits_left  = 5'h10;     // Reset bits_left to full (16 bits => 0x10; 0x10 can be a placeholder)
                // Remain in ERROR until i_clear
                if (i_clear) begin
                    next_state     = ST_IDLE;
                    o_fsm_state    = ST_IDLE; // Reflect the current FSM state
                        
                end
                else begin
                    next_state     = ST_ERROR;
                    o_fsm_state = ST_ERROR;
                end
            end
            else begin
                case (curr_state)
                    //--------------------------------------------------------------
                    // IDLE State
                    //--------------------------------------------------------------
                    ST_IDLE: begin
                        o_spi_cs_b     = 1'b1;       // Default de-asserted (active-low)
                        o_spi_clk      = 1'b0;       // Default clock low when idle
                        o_spi_data     = 1'b0;       // Default data low when idle
                        o_fsm_state    = ST_IDLE; // Reflect the current FSM state
                        if (i_enable && !i_fault && !i_clear) begin
                            // Move to TRANSMIT to load the shift register
                            next_state     = ST_TRANSMIT;
                            next_done      = 1'b0;
                            next_bits_left = 5'd16;  // 16 bits to send
                        end
                        else begin
                            next_state     = ST_IDLE;
                            next_bits_left = 5'h10;
                            next_done      = 1'b0;
                        end
                    end
                    
                    //--------------------------------------------------------------
                    // TRANSMIT State
                    //--------------------------------------------------------------
                    ST_TRANSMIT: begin
                        o_spi_cs_b  = 1'b0;
                        o_spi_clk   = 1'b0;
                        o_spi_data  = shift_reg[15];
                        o_fsm_state = ST_TRANSMIT;
                        // Immediately move to CLOCK_TOGGLE to begin shifting
                        next_done       = 1'b0;
                        next_bits_left  = o_bits_left;
                        next_state      = ST_CLOCK_TOGGLE;
                        // Decrement bits_left after shifting out
                        next_bits_left = (o_bits_left == 5'd0)
                                        ? 5'd0
                                        : o_bits_left - 5'b1;
                    end
                    
                    //--------------------------------------------------------------
                    // CLOCK TOGGLE State
                    //--------------------------------------------------------------
                    ST_CLOCK_TOGGLE: begin
                        o_spi_cs_b  = 1'b0;
                        o_spi_clk   = 1'b1;
                        o_spi_data  = shift_reg[15];
                        o_fsm_state = ST_CLOCK_TOGGLE;
                        
                        
                        // If all bits have been shifted out, return to IDLE
                        // and pulse `o_done` for one clock.
                        if (o_bits_left == 5'd1) begin
                            // This was the last bit
                            next_state = ST_IDLE;
                            next_done  = 1'b1;
                        end
                        else begin
                            // Go back to TRANSMIT to shift out next bit
                            next_done       = 1'b0;
                            next_state      = ST_TRANSMIT;
                        end
                    end
                    
                    //--------------------------------------------------------------
                    // ERROR State
                    //--------------------------------------------------------------
                    ST_ERROR: begin
                        // Remain in ERROR until i_clear
                        o_spi_cs_b  = 1'b1;
                        o_spi_clk   = 1'b0;
                        o_spi_data  = 1'b0;
                        o_fsm_state = ST_ERROR;
                        next_done       = 1'b0; 
                        next_bits_left  = 5'h10;     // Reset bits_left to full (16 bits => 0x10; 0x10 can be a placeholder)
                        if (i_clear) begin
                            next_state     = ST_IDLE;
                        end
                        else begin
                            next_state     = ST_ERROR;
                        end
                    end
                    
                    default: begin
                        // Should never happen; default to IDLE
                        next_state = ST_IDLE;
                    end
                endcase
            end
        end
    end
    
endmodule
