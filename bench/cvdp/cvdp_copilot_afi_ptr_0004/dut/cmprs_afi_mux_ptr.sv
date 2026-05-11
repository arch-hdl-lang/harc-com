`timescale 1ns / 1ps

module cmprs_afi_mux_ptr(
    input                         hclk,                // Clock signal
    input                         reset,               // Synchronous reset input
    input                  [26:0] sa_len_di,           // Data input for sa_len_ram
    input                  [ 2:0] sa_len_wa,           // Write address for sa_len_ram
    input                         sa_len_we,           // Write enable for sa_len_ram
    input                         en,                  // Enable signal for FSM
    input                  [ 3:0] reset_pointers,      // Reset pointers for specific channels
    input                         pre_busy_w,          // Arbitration start signal
    input                  [ 1:0] pre_winner_channel,  // Active channel for arbitration
    input                         need_to_bother,      // Indicates whether an update is required
    input                   [1:0] chunk_inc_want_m1,   // Increment value for the pointer
    input                         last_burst_in_frame, // Indicates the last burst in frame
    input                  [ 3:0] busy,                // Busy signals for channels
    output                        ptr_resetting,       // Indicates the FSM is in RESETTING state
    output reg             [26:0] chunk_addr,          // Current pointer address
    output                 [ 2:0] max_wlen             // Maximum word length
);

    // Internal signals
    reg [1:0] state, next_state;         // Current and next states of FSM
    localparam IDLE        = 2'b00,      // FSM state: IDLE
               RESETTING   = 2'b01,      // FSM state: RESETTING
               ARBITRATION = 2'b10,      // FSM state: ARBITRATION
               UPDATE      = 2'b11;      // FSM state: UPDATE

    reg [26:0] ptr_ram[0:7];             // Pointer values for 8 channels
    reg [26:0] sa_len_ram[0:7];          // Buffer sizes for 8 channels
    reg [ 2:0] max_inc_ram[0:3];         // Maximum increment values for 4 channels
    reg [3:0] reset_rq;                  // Tracks reset requests for pointers
    reg rollover_flag;                   // Indicates rollover condition for pointer

    integer i;                           // Loop variable for initialization

    assign ptr_resetting = (state == RESETTING); 
    assign max_wlen = max_inc_ram[pre_winner_channel]; 

    always @(posedge hclk) begin
        if (reset) begin
            for (i = 0; i < 8; i = i + 1) begin
                ptr_ram[i] <= 27'd0;       
                sa_len_ram[i] <= 27'd64;  
            end
            for (i = 0; i < 4; i = i + 1) begin
                max_inc_ram[i] <= 3'd0;   
            end
            reset_rq <= 4'b0000;          
            rollover_flag <= 1'b0;        
            chunk_addr <= 27'd0;          
            state <= IDLE;                
        end else begin
            state <= next_state;          
        end
    end

    always @(*) begin
        case (state)
            IDLE: begin
                if (|reset_pointers)
                    next_state = RESETTING;
                else if (pre_busy_w)
                    next_state = ARBITRATION;
                else
                    next_state = IDLE;    
            end
            RESETTING: begin
                if (!reset_rq)
                    next_state = IDLE;
                else
                    next_state = RESETTING; 
            end
            ARBITRATION: begin
                if (need_to_bother)
                    next_state = UPDATE;
                else
                    next_state = IDLE;    
            end
            UPDATE: begin
                if (!busy[pre_winner_channel])
                    next_state = IDLE;
                else
                    next_state = UPDATE;
            end
            default: next_state = IDLE;    
        endcase
    end

    always @(posedge hclk) begin
        if (!reset) begin 
            case (state)
                IDLE: begin
                    if (sa_len_we) begin
                        sa_len_ram[sa_len_wa] <= sa_len_di;
                    end
                    reset_rq <= reset_pointers;
                end
                RESETTING: begin
                    if (reset_rq != 4'b0000) begin
                        if (reset_rq[0]) ptr_ram[0] <= 27'd0;
                        if (reset_rq[1]) ptr_ram[1] <= 27'd0;
                        if (reset_rq[2]) ptr_ram[2] <= 27'd0;
                        if (reset_rq[3]) ptr_ram[3] <= 27'd0;
                        reset_rq <= 4'b0000; 
                    end
                end
                ARBITRATION: begin
                    rollover_flag <= (ptr_ram[pre_winner_channel] + chunk_inc_want_m1 >= sa_len_ram[pre_winner_channel]);
                end
                UPDATE: begin
                    if (busy[pre_winner_channel]) begin
                        if (rollover_flag) begin
                            ptr_ram[pre_winner_channel] <= ptr_ram[pre_winner_channel] + chunk_inc_want_m1 - sa_len_ram[pre_winner_channel];
                            chunk_addr <= ptr_ram[pre_winner_channel] + chunk_inc_want_m1 - sa_len_ram[pre_winner_channel];
                        end else begin
                            ptr_ram[pre_winner_channel] <= ptr_ram[pre_winner_channel] + chunk_inc_want_m1;
                            chunk_addr <= ptr_ram[pre_winner_channel] + chunk_inc_want_m1;
                        end
                    end
                end
            endcase
        end
    end

endmodule
