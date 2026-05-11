module traffic_controller_fsm ( 
    input i_clk,                     // System clock input
    input i_rst_b,                   // Active-low reset signal
    input i_vehicle_sensor_input,     // High when a vehicle is present on the side road
    input i_short_timer,              // High when the short timer expires
    input i_long_timer,               // High when the long timer expires
    output reg o_short_trigger,       // Trigger for the short timer (active high)
    output reg o_long_trigger,        // Trigger for the long timer (active high)
    output reg [2:0] o_main,          // Main road traffic lights (3-bit: Red, Yellow, Green)
    output reg [2:0] o_side           // Side road traffic lights (3-bit: Red, Yellow, Green)
);
    
localparam p_state_S1 = 2'd0 ;       
localparam p_state_S2 = 2'd1 ;       
localparam p_state_S3 = 2'd2 ;       
localparam p_state_S4 = 2'd3 ;       

reg [1:0]   r_state;                 
reg [1:0]   r_next_state;            

//-----------------------------------------------------------------------------
// Next State Logic
//-----------------------------------------------------------------------------
always @(*) begin
    if (!i_rst_b) begin              
        r_next_state = p_state_S1;   
    end else begin
        case (r_state)
        p_state_S1: begin
            if (i_vehicle_sensor_input & i_long_timer) begin
                r_next_state = p_state_S2;  
            end else begin
                r_next_state = p_state_S1;  
            end
        end
        p_state_S2: begin
            if (i_short_timer) begin
                r_next_state = p_state_S3;  
            end else begin
                r_next_state = p_state_S2;  
            end
        end
        p_state_S3: begin
            if ((!i_vehicle_sensor_input) | i_long_timer) begin
                r_next_state = p_state_S4;  
            end else begin
                r_next_state = p_state_S3;  
            end
        end
        p_state_S4: begin
            if (i_short_timer) begin
                r_next_state = p_state_S1;  
            end else begin
                r_next_state = p_state_S4;  
            end
        end
        endcase
    end
end

always @(posedge i_clk or negedge i_rst_b) begin
    if (!i_rst_b) begin                  
        r_state <= p_state_S1;           
    end else begin
        r_state <= r_next_state;         
    end
end


always @(posedge i_clk or negedge i_rst_b) begin
    if (!i_rst_b) begin                   
        o_main <= 3'd0;                  
        o_side <= 3'd0;                  
        o_long_trigger <= 1'b0;          
        o_short_trigger <= 1'b0;         
    end else begin
        case (r_state)
        p_state_S1: begin
            o_main <= 3'b001;            
            o_side <= 3'b100;            
            o_long_trigger <= 1'b1;      
            o_short_trigger <= 1'b0;     
        end
        p_state_S2: begin
            o_main <= 3'b010;             
            o_side <= 3'b100;             
            o_long_trigger <= 1'b0;       
            o_short_trigger <= 1'b1;     
        end
        p_state_S3: begin
            o_main <= 3'b100;            
            o_side <= 3'b001;            
            o_long_trigger <= 1'b1;      
            o_short_trigger <= 1'b0;     
        end
        p_state_S4: begin
            o_main <= 3'b100;             
            o_side <= 3'b010;             
            o_long_trigger <= 1'b0;       
            o_short_trigger <= 1'b1;     
        end
        endcase
    end
end

endmodule
