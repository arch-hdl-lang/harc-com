module thermostat (
    input wire [5:0] i_temp_feedback, // Temperature feedback bits
    input wire i_fan_on,             // Manual fan control
    input wire i_enable,             // Enable thermostat
    input wire i_fault,              // Fault signal
    input wire i_clr,                // Clear fault signal
    input wire i_clk,                // Clock input
    input wire i_rst,                // Asynchronous reset (active-low)

    output reg o_heater_full,
    output reg o_heater_medium,
    output reg o_heater_low,
    output reg o_aircon_full,
    output reg o_aircon_medium,
    output reg o_aircon_low,
    output reg o_fan,
    output reg [2:0] o_state         // FSM state output
);

// State encoding
localparam [2:0] HEAT_LOW  = 3'b000,
                 HEAT_MED  = 3'b001,
                 HEAT_FULL = 3'b010,
                 AMBIENT   = 3'b011,
                 COOL_LOW  = 3'b100,
                 COOL_MED  = 3'b101,
                 COOL_FULL = 3'b110;

// Internal signals
reg [2:0] current_state, next_state; // FSM state registers
reg heater_full, heater_medium, heater_low;
reg aircon_full, aircon_medium, aircon_low;
reg fan;
always @(*) begin
    o_state = current_state;
end
// Sequential logic for state transitions and registered outputs
always @(posedge i_clk or negedge i_rst) begin
    if (!i_rst) begin
        // Asynchronous reset
        current_state <= AMBIENT;
        o_heater_full <= 0;
        o_heater_medium <= 0;
        o_heater_low <= 0;
        o_aircon_full <= 0;
        o_aircon_medium <= 0;
        o_aircon_low <= 0;
        o_fan <= 0;
    end else begin
        // Normal state transition
        current_state <= next_state;
        // Update registered outputs
        o_heater_full <= heater_full;
        o_heater_medium <= heater_medium;
        o_heater_low <= heater_low;
        o_aircon_full <= aircon_full;
        o_aircon_medium <= aircon_medium;
        o_aircon_low <= aircon_low;
        o_fan <= fan || i_fan_on;
    end
end

// Combinational logic for next state and intermediate outputs
always @(*) begin
    if (!i_enable || i_fault) begin
        // Handle fault or disable
        next_state = AMBIENT;
        heater_full = 0;
        heater_medium = 0;
        heater_low = 0;
        aircon_full = 0;
        aircon_medium = 0;
        aircon_low = 0;
        fan = 0;
    end else begin
        case (current_state)
            // Heating states
            HEAT_LOW: begin
                heater_full = 0;
                heater_medium = 0;
                heater_low = 1;
                aircon_full = 0;
                aircon_medium = 0;
                aircon_low = 0;
                fan = 1;
                if (i_temp_feedback[5]) begin 
                    next_state = HEAT_FULL; 
                end// Full cold
                else if (i_temp_feedback[0]) begin // Full hot
                    next_state = COOL_FULL;
                end
                else begin
                    if (i_temp_feedback[4]) begin 
                        next_state = HEAT_MED; 
                    end// Medium cold
                    else if (i_temp_feedback[1]) begin // Medium hot
                        next_state = COOL_MED;
                    end
                    else begin
                        if (i_temp_feedback[3]) begin 
                            next_state = HEAT_LOW; 
                        end// Low cold
                        else if (i_temp_feedback[2]) begin // Low hot
                            next_state = COOL_LOW;
                        end
                        else begin
                            next_state = AMBIENT;
                        end
                    end
                end
            end

            HEAT_MED: begin
                heater_full = 0;
                heater_medium = 1;
                heater_low = 0;
                aircon_full = 0;
                aircon_medium = 0;
                aircon_low = 0;
                fan = 1;
                if (i_temp_feedback[5]) begin 
                    next_state = HEAT_FULL; 
                end// Full cold
                else if (i_temp_feedback[0]) begin // Full hot
                    next_state = COOL_FULL;
                end
                else begin
                    if (i_temp_feedback[4]) begin 
                        next_state = HEAT_MED; 
                    end// Medium cold
                    else if (i_temp_feedback[1]) begin // Medium hot
                        next_state = COOL_MED;
                    end
                    else begin
                        if (i_temp_feedback[3]) begin 
                            next_state = HEAT_LOW; 
                        end// Low cold
                        else if (i_temp_feedback[2]) begin // Low hot
                            next_state = COOL_LOW;
                        end
                        else begin
                            next_state = AMBIENT;
                        end
                    end
                end
            end

            HEAT_FULL: begin
                heater_full = 1;
                heater_medium = 0;
                heater_low = 0;
                aircon_full = 0;
                aircon_medium = 0;
                aircon_low = 0;
                fan = 1;
                if (i_temp_feedback[5]) begin 
                    next_state = HEAT_FULL; 
                end// Full cold
                else if (i_temp_feedback[0]) begin // Full hot
                    next_state = COOL_FULL;
                end
                else begin
                    if (i_temp_feedback[4]) begin 
                        next_state = HEAT_MED; 
                    end// Medium cold
                    else if (i_temp_feedback[1]) begin // Medium hot
                        next_state = COOL_MED;
                    end
                    else begin
                        if (i_temp_feedback[3]) begin 
                            next_state = HEAT_LOW; 
                        end// Low cold
                        else if (i_temp_feedback[2]) begin // Low hot
                            next_state = COOL_LOW;
                        end
                        else begin
                            next_state = AMBIENT;
                        end
                    end
                end
            end

            // Cooling states
            COOL_LOW: begin
                heater_full = 0;
                heater_medium = 0;
                heater_low = 0;
                aircon_full = 0;
                aircon_medium = 0;
                aircon_low = 1;
                fan = 1;
                if (i_temp_feedback[5]) begin 
                    next_state = HEAT_FULL; 
                end// Full cold
                else if (i_temp_feedback[0]) begin // Full hot
                    next_state = COOL_FULL;
                end
                else begin
                    if (i_temp_feedback[4]) begin 
                        next_state = HEAT_MED; 
                    end// Medium cold
                    else if (i_temp_feedback[1]) begin // Medium hot
                        next_state = COOL_MED;
                    end
                    else begin
                        if (i_temp_feedback[3]) begin 
                            next_state = HEAT_LOW; 
                        end// Low cold
                        else if (i_temp_feedback[2]) begin // Low hot
                            next_state = COOL_LOW;
                        end
                        else begin
                            next_state = AMBIENT;
                        end
                    end
                end
            end

            COOL_MED: begin
                heater_full = 0;
                heater_medium = 0;
                heater_low = 0;
                aircon_full = 0;
                aircon_medium = 1;
                aircon_low = 0;
                fan = 1;
                aircon_medium = 1;
                if (i_temp_feedback[5]) begin 
                    next_state = HEAT_FULL; 
                end// Full cold
                else if (i_temp_feedback[0]) begin // Full hot
                    next_state = COOL_FULL;
                end
                else begin
                    if (i_temp_feedback[4]) begin 
                        next_state = HEAT_MED; 
                    end// Medium cold
                    else if (i_temp_feedback[1]) begin // Medium hot
                        next_state = COOL_MED;
                    end
                    else begin
                        if (i_temp_feedback[3]) begin 
                            next_state = HEAT_LOW; 
                        end// Low cold
                        else if (i_temp_feedback[2]) begin // Low hot
                            next_state = COOL_LOW;
                        end
                        else begin
                            next_state = AMBIENT;
                        end
                    end
                end
            end

            COOL_FULL: begin
                heater_full = 0;
                heater_medium = 0;
                heater_low = 0;
                aircon_full = 1;
                aircon_medium = 0;
                aircon_low = 0;
                fan = 1;
                if (i_temp_feedback[5]) begin 
                    next_state = HEAT_FULL; 
                end// Full cold
                else if (i_temp_feedback[0]) begin // Full hot
                    next_state = COOL_FULL;
                end
                else begin
                    if (i_temp_feedback[4]) begin 
                        next_state = HEAT_MED; 
                    end// Medium cold
                    else if (i_temp_feedback[1]) begin // Medium hot
                        next_state = COOL_MED;
                    end
                    else begin
                        if (i_temp_feedback[3]) begin 
                            next_state = HEAT_LOW; 
                        end// Low cold
                        else if (i_temp_feedback[2]) begin // Low hot
                            next_state = COOL_LOW;
                        end
                        else begin
                            next_state = AMBIENT;
                        end
                    end
                end
            end

            // Ambient state
            AMBIENT: begin
                heater_full = 0;
                heater_medium = 0;
                heater_low = 0;
                aircon_full = 0;
                aircon_medium = 0;
                aircon_low = 0;
                fan = 0;
                if (i_temp_feedback[5]) begin 
                    next_state = HEAT_FULL; 
                end// Full cold
                else if (i_temp_feedback[0]) begin // Full hot
                    next_state = COOL_FULL;
                end
                else begin
                    if (i_temp_feedback[4]) begin 
                        next_state = HEAT_MED; 
                    end// Medium cold
                    else if (i_temp_feedback[1]) begin // Medium hot
                        next_state = COOL_MED;
                    end
                    else begin
                        if (i_temp_feedback[3]) begin 
                            next_state = HEAT_LOW; 
                        end// Low cold
                        else if (i_temp_feedback[2]) begin // Low hot
                            next_state = COOL_LOW;
                        end
                        else begin
                            next_state = AMBIENT;
                        end
                    end
                end
            end

            default: next_state = AMBIENT; // Safety fallback
        endcase
    end
end

endmodule
