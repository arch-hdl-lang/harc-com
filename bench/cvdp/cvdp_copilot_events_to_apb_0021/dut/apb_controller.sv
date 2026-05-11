module apb_controller(
    input  logic         clk,              // Clock signal
    input  logic         reset_n,          // Active low asynchronous reset signal
    input  logic         select_a_i,       // Select signal for event A
    input  logic         select_b_i,       // Select signal for event B
    input  logic         select_c_i,       // Select signal for event C
    input  logic [31:0]  addr_a_i,         // 32-bit address for event A transaction
    input  logic [31:0]  data_a_i,         // 32-bit data for event A transaction
    input  logic [31:0]  addr_b_i,         // 32-bit address for event B transaction
    input  logic [31:0]  data_b_i,         // 32-bit data for event B transaction
    input  logic [31:0]  addr_c_i,         // 32-bit address for event C transaction
    input  logic [31:0]  data_c_i,         // 32-bit data for event C transaction
    output logic         apb_psel_o,       // APB select signal
    output logic         apb_penable_o,    // APB enable signal
    output logic [31:0]  apb_paddr_o,      // 32-bit APB address output
    output logic         apb_pwrite_o,     // APB write signal
    output logic [31:0]  apb_pwdata_o,     // 32-bit APB write data output
    input  logic         apb_pready_i      // APB ready signal from the peripheral
);

    // State definitions
    typedef enum logic [1:0] {
       IDLE,   
       SETUP,  
       ACCESS
    } state_t; 

    logic [3:0]  count;
    logic [2:0]  event_sel; 
    logic [2:0]  event_sel_ff; 
    logic [2:0]  event_list;    
    state_t      current_state, next_state;
    logic [31:0] sel_addr_next, sel_data_next;  
    logic [31:0] sel_addr, sel_data;          
    logic select_a_pulse, select_b_pulse, select_c_pulse;

    assign apb_psel_o    = (current_state == SETUP || current_state == ACCESS) ? 1'b1 : 1'b0;
    assign apb_penable_o = (current_state == ACCESS) ? 1'b1 : 1'b0;
    assign apb_pwrite_o  = (current_state == SETUP || current_state == ACCESS) ? 1'b1 : 1'b0;
    assign apb_paddr_o   = (current_state == SETUP || current_state == ACCESS) ? sel_addr : 32'b0;
    assign apb_pwdata_o  = (current_state == SETUP || current_state == ACCESS) ? sel_data : 32'b0;


    always_ff @(posedge clk or negedge reset_n) begin
        if (!reset_n) begin
            current_state <= IDLE;
            event_list    <= 3'b000; 
            event_sel_ff    <= 3'b000; 
            select_a_pulse  <= 1'b0;  
            select_b_pulse  <= 1'b0; 
            select_c_pulse  <= 1'b0;             
        end else begin
            select_a_pulse <= (select_a_i) ? 1'b1 : select_a_pulse;
            select_b_pulse <= (select_b_i) ? 1'b1 : select_b_pulse;
            select_c_pulse <= (select_c_i) ? 1'b1 : select_c_pulse;      
            event_sel_ff   <= event_sel;              
            current_state  <= next_state;
            if (current_state == IDLE) begin
               event_list <= (select_a_pulse ? 3'b001 : 3'b000) | 
                               (select_b_pulse ? 3'b010 : 3'b000) |
                               (select_c_pulse ? 3'b100 : 3'b000); 
            end 
            else if (next_state == IDLE) begin
               select_a_pulse <= (event_sel[0]) ? 1'b0 : select_a_pulse;
               select_b_pulse <= (event_sel[1]) ? 1'b0 : select_b_pulse;
               select_c_pulse <= (event_sel[2]) ? 1'b0 : select_c_pulse;    
               event_list   <= event_list & ~event_sel; 
            end            
        end
    end


    always @(*) begin
        // Default values
        next_state = current_state;
        event_sel  = event_sel_ff; 
        case (current_state)
            IDLE: begin
                if (event_list[0]) begin
                   next_state = SETUP;
                   event_sel  = 3'b001;
                end else if (event_list[1]) begin
                   next_state = SETUP;
                   event_sel  = 3'b010;
                end else if (event_list[2]) begin
                   next_state = SETUP;
                   event_sel  = 3'b100;
                end else begin
                   next_state = IDLE;
                   event_sel  = 3'b000;
                end
            end
            SETUP: begin
                next_state = ACCESS;
            end
            ACCESS: begin
                if (apb_pready_i || count == 15) begin
                    next_state = IDLE;
                end    
            end
            default: begin
                next_state = IDLE; 
                event_sel  = 3'b000;                 
            end    
        endcase
    end

    always_ff @(posedge clk or negedge reset_n) begin
        if (!reset_n) begin
            count <= 0;
        end else if (current_state == ACCESS && !apb_pready_i) begin
            count <= count + 1;
        end else begin
            count <= 0;
        end
    end

    always_ff @(posedge clk or negedge reset_n) begin
        if (!reset_n) begin
            sel_addr <= 32'b0;
            sel_data <= 32'b0;
        end else if (current_state == IDLE) begin
            sel_addr <= sel_addr_next;
            sel_data <= sel_data_next;
        end
    end
    
    always @(*) begin
        if (!reset_n) begin
           sel_addr_next = sel_addr;
           sel_data_next = sel_data;       
        end 
        else begin 
            if (next_state == SETUP || next_state == ACCESS) begin
                case (event_sel)
                    3'b001: begin    
                       sel_addr_next = addr_a_i; 
                       sel_data_next = data_a_i;                     
                    end    
                    3'b010: begin                    
                       sel_addr_next = addr_b_i; 
                       sel_data_next = data_b_i;    
                    end
                    3'b100: begin                      
                       sel_addr_next = addr_c_i; 
                       sel_data_next = data_c_i; 
                    end
                    default: begin
                       sel_addr_next = 32'b0; 
                       sel_data_next = 32'b0;                     
                    end 
                endcase           
            end     
        end        
    end    

endmodule
