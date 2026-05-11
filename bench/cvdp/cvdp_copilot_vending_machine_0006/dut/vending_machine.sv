module vending_machine(
    input clk,                          // Clock signal
    input rst,                          // Reset signal
    input item_button,                  // Signal for item selection button press (active high, rising edge)
    input [2:0] item_selected,          // 3-bit input to select item (valid: 3'b001 to 3'b100)
    input [3:0] coin_input,             // 4-bit coin input to represent value of coin inserted (1, 2, 5, or 10)
    input cancel,                       // Cancel button signal (active high, rising edge)
    output reg dispense_item,           // Signal to dispense item (active high for one cycle)
    output reg return_change,           // Signal to return change (active high for one cycle)
    output reg [4:0] item_price,        // Price of the selected item
    output reg [4:0] change_amount,     // Amount of change to return
    output reg [2:0] dispense_item_id,  // ID of item being dispensed
    output reg error,                   // Error signal if any invalid operation occurs (active high for one cycle)
    output reg return_money             // Signal to return all money if operation is cancelled or invalid (active high for one cycle)
);

    // Item Prices (Assume we have 4 items with prices: 5, 10, 15, 20 units)
    localparam ITEM_1_PRICE = 5'd5;
    localparam ITEM_2_PRICE = 5'd10;
    localparam ITEM_3_PRICE = 5'd15;
    localparam ITEM_4_PRICE = 5'd20;

    // Item IDs or Names
    localparam NO_ITEM = 3'b000;
    localparam ITEM_1_ID = 3'b001; // Item 1
    localparam ITEM_2_ID = 3'b010; // Item 2
    localparam ITEM_3_ID = 3'b011; // Item 3
    localparam ITEM_4_ID = 3'b100; // Item 4

    // Internal registers
    reg [4:0] coins_accumulated;   // Tracks accumulated coins
    reg [4:0] amount;

    // Define state enum
    typedef enum logic [3:0] {
        IDLE                = 4'd0,     // Idle state
        ITEM_SELECTION      = 4'd1,     // Waiting for item selection
        PAYMENT_VALIDATION  = 4'd2,     // Validating coin input
        DISPENSING_ITEM     = 4'd3,     // Dispensing the item
        RETURN_CHANGE       = 4'd4,     // Returning change
        RETURN_MONEY        = 4'd5      // Returning all money
    } state_t;

    state_t current_state, next_state;         // Current and next states of the vending machine
    reg [4:0] next_coins_accumulated;
    reg       next_return_change;
    reg [4:0] next_item_price;
    reg [4:0] next_change_amount;
    reg       next_error;
    reg [2:0] next_dispense_item_id;
    reg       next_return_money;
    reg       next_dispense_item;
    reg [4:0] next_amount;

    // State Machine logic
    always_ff @(posedge clk or posedge rst) begin
        if (rst) begin
            // Reset all outputs and internal registers
            coins_accumulated <= 5'd0;
            return_change     <= 1'b0;
            item_price        <= 5'd0;
            change_amount     <= 5'd0;
            error             <= 1'b0;
            dispense_item_id  <= 3'd0;
            return_money      <= 1'b0;
            dispense_item     <= 1'b0;
            current_state     <= IDLE;
        end else begin
            current_state       <= next_state;
            coins_accumulated   <= next_coins_accumulated;
            item_price          <= next_item_price;
            change_amount       <= next_change_amount;
            error               <= next_error;
            dispense_item_id    <= next_dispense_item_id;
            return_money        <= next_return_money;
            dispense_item       <= next_dispense_item;
            return_change       <= next_return_change;
            amount              <= next_amount;

        end
        
    end

    // Next state logic (combinatorial)
    always @ (*) begin
        next_state = current_state;
        next_coins_accumulated = coins_accumulated;
        next_item_price        = item_price;
        next_change_amount     = change_amount;
        next_error             = error;
        next_dispense_item_id  = dispense_item_id;
        next_return_money      = return_money;
        next_dispense_item     = dispense_item;
        next_return_change     = return_change;
        next_amount            = amount;
        case (current_state)
            // IDLE State
            IDLE: begin
                if (item_button) begin
                    next_return_money  = 1'b0;
                    next_return_change = 1'b0; 
                    next_change_amount = 5'd0;
                    next_coins_accumulated = 5'd0; 
                    next_error   = 1'b0;
                    next_amount = 5'd0;
                    next_dispense_item_id = 3'd0;
                    next_state = ITEM_SELECTION;
                end else if (coin_input > 0) begin
                    next_return_change = 1'b0; 
                    next_coins_accumulated =  coin_input;
                    next_error   = 1'b1;
                    next_amount = 5'd0;;    
                    next_dispense_item_id  = 3'd0;
                    next_state = RETURN_MONEY;
                end else begin
                    next_return_money  = 1'b0;
                    next_return_change = 1'b0; 
                    next_change_amount = 5'd0;
                    next_error   = 1'b0;
                    next_amount = 5'd0;
                    next_dispense_item_id  = 3'd0;
                end
            end

            // ITEM_SELECTION State
            ITEM_SELECTION: begin
                if (cancel) begin
                    next_error   = 1'b1;
                    next_state = RETURN_MONEY;
                end else if (item_selected >= 3'b000 ) begin
                    case (item_selected)
                        3'b001: next_item_price = ITEM_1_PRICE;
                        3'b010: next_item_price = ITEM_2_PRICE;
                        3'b011: next_item_price = ITEM_3_PRICE;
                        3'b100: next_item_price = ITEM_4_PRICE;
                        default: next_item_price = NO_ITEM;
                    endcase
                    next_state = PAYMENT_VALIDATION;
                end
            end

            // PAYMENT_VALIDATION State
            PAYMENT_VALIDATION: begin
                if (cancel) begin
                    next_error   = 1'b1;
                    next_state = RETURN_MONEY;
                end else if ( item_selected == 3'b000 ) begin
                    next_error = 1'b1;
                    next_coins_accumulated = coins_accumulated + coin_input;
                    next_state = RETURN_MONEY;
                end else if ( item_price == NO_ITEM ) begin
                    next_error = 1'b1;
                    next_coins_accumulated = coins_accumulated + coin_input;
                    next_state = RETURN_MONEY;
                end else if (error) begin
                    next_state = IDLE; 
                end else if (coins_accumulated >= item_price) begin
                    next_state = DISPENSING_ITEM;
                    next_dispense_item   = 1'b1;
                end else if (coin_input > 0) begin
                    // Validate the coin input (only 1, 2, 5, or 10 is accepted)
                    if (coin_input == 4'd1 || coin_input == 4'd2 || coin_input == 4'd5 || coin_input == 4'd10) begin
                        next_coins_accumulated = coins_accumulated + coin_input; 
                        next_state = PAYMENT_VALIDATION;  
                    end else begin  
                        next_amount =  coins_accumulated + coin_input;  
                        next_error = 1'b1; 
                        next_return_money = 1'b1; 
                        next_change_amount = next_amount;
                        next_state = IDLE;      
                    end
                end
            end

            // DISPENSING_ITEM State
            DISPENSING_ITEM: begin
                next_dispense_item = 1'b0;
                case (item_selected)
                    3'b001: next_dispense_item_id = ITEM_1_ID;
                    3'b010: next_dispense_item_id = ITEM_2_ID;
                    3'b011: next_dispense_item_id = ITEM_3_ID;
                    3'b100: next_dispense_item_id = ITEM_4_ID;
                    default: next_dispense_item_id = NO_ITEM;
                endcase

                if (coins_accumulated > item_price) begin
                    next_state = RETURN_CHANGE;
                    next_amount = coins_accumulated - item_price;
                end else begin
                    next_coins_accumulated = 5'd0;
                    next_state = IDLE;
                end
                next_coins_accumulated = 5'd0;
            end

            // RETURN_CHANGE State
            RETURN_CHANGE: begin
                next_return_change = 1'b1;
                next_change_amount = amount;
                next_error   = 1'b0;
                next_dispense_item_id  = 3'b000;
                next_coins_accumulated = 5'b0;
                next_state = IDLE;
            end

            // RETURN_MONEY State
            RETURN_MONEY: begin
                if (coins_accumulated == 0) begin
                    next_return_money  = 1'b0;
                end else if (coins_accumulated > 0) begin
                    next_return_money  = 1'b1;
                    next_change_amount = coins_accumulated;
                end
                next_error   = 1'b0;
                next_state = IDLE;
            end

            default: next_state = IDLE;
        endcase
    end

endmodule

