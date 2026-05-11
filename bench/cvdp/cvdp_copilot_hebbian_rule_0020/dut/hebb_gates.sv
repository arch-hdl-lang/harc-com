`timescale 1ns/1ps
module hebb_gates(
   input  logic               clk,          // Posedge clk
   input  logic               rst,          // Asynchronous negedge rst
   input  logic               start,        // To start the FSM
   input  logic         [1:0] target_select,// To select different targets for a given gate
   input  logic  signed [3:0] a,            // First Input
   input  logic  signed [3:0] b,            // Second Input
   input  logic         [1:0] gate_select,  // To select a given gate
   output logic  signed [3:0] w1,           // Weight 1 obtained by training
   output logic  signed [3:0] w2,           // Weight 2 obtained by training
   output logic  signed [3:0] bias,         // Bias obtained by training
   output logic       [3:0] present_state,  // Present State of the Training FSM
   output logic       [3:0] next_state      // Next_State of the Training FSM
);
   logic signed [3:0] t1;
   logic signed [3:0] t2;
   logic signed [3:0] t3;
   logic signed [3:0] t4;
   
   gate_target dut (
      .gate_select   (gate_select),
      .target_select (target_select),
      .o_1           (t1),
      .o_2           (t2),
      .o_3           (t3),
      .o_4           (t4)
   );
   
   
   localparam [3:0] State_0  = 4'd0;
   localparam [3:0] State_1  = 4'd1;
   localparam [3:0] State_2  = 4'd2;
   localparam [3:0] State_3  = 4'd3;
   localparam [3:0] State_4  = 4'd4;
   localparam [3:0] State_5  = 4'd5;
   localparam [3:0] State_6  = 4'd6;
   localparam [3:0] State_7  = 4'd7;
   localparam [3:0] State_8  = 4'd8;
   localparam [3:0] State_9  = 4'd9;
   localparam [3:0] State_10 = 4'd10;
   
   
   logic [2:0]          iteration;
   logic signed [3:0]   x1;
   logic signed [3:0]   x2;
   logic signed [3:0]   delta_w1;
   logic signed [3:0]   delta_w2;
   logic signed [3:0]   delta_b;
   logic signed [3:0]   w1_reg;
   logic signed [3:0]   w2_reg;
   logic signed [3:0]   bias_reg;
   logic signed [1:0]   target;
   logic                delta_en;
   logic                sum_en;
   logic                clr_en;
   logic                cap_en;
  
   
   always @(*) begin
      if (cap_en) begin
         x1 = a;
         x2 = b;
      end else begin
         x1 = x1 + 4'h0;
         x2 = x2 + 4'h0;
      end
   end
   
   
   always @(*) begin
      if (delta_en) begin
         delta_w1 = x1 * target;
         delta_w2 = x2 * target;
         delta_b  = target;
      end else begin
         delta_w1 = delta_w1 + 4'h0;
         delta_w2 = delta_w2 + 4'h0;
         delta_b  = delta_b + 4'h0;
      end
   end
   
   
   always @(*) begin
      if (sum_en) begin
         w1_reg   = w1_reg + delta_w1;
         w2_reg   = w2_reg + delta_w2;
         bias_reg = bias_reg + delta_b;
      end else begin
         w1_reg   = w1_reg + 4'h0;
         w2_reg   = w2_reg + 4'h0;
         bias_reg = bias_reg + 4'h0;
      end
   end
   
   
   always @(*) begin
      if (clr_en) begin
         w1_reg   = 0;
         w2_reg   = 0;
         bias_reg = 0;
      end else begin
         w1_reg   = w1_reg + 4'h0;
         w2_reg   = w2_reg + 4'h0;
         bias_reg = bias_reg + 4'h0;
      end
   end
   
   
   always @(posedge clk or negedge rst) begin
      if (!rst) begin
         present_state <= State_0;
         iteration     <= 0;
      end else begin
         present_state <= next_state;
      end
   end
   
   
   always @(*) begin
      next_state = present_state;
      case (present_state)
         State_0: begin 
            if (start)
               next_state = State_1;
            else
               next_state = State_0;
         end
         State_1: begin 
            next_state = State_2;
         end
         State_2: begin 
            if (iteration == 0)
               next_state = State_3;
            else if (iteration == 1)
               next_state = State_4;
            else if (iteration == 2)
               next_state = State_5;
            else 
               next_state = State_6;
         end
         State_3: begin 
            next_state = State_7;
         end
         State_4: begin 
            next_state = State_7;
         end
         State_5: begin 
            next_state = State_7;
         end
         State_6: begin 
            next_state = State_7;
         end
         State_7: begin
            next_state = State_8;
         end
         State_8: begin
            next_state = State_9;
         end
         State_9: begin
            if (iteration < 4)
               next_state = State_1;
            else
               next_state = State_10;
         end
         State_10: begin
            next_state = State_0;
         end
         default: ;
      endcase
   end 
   
   
   always @(*) begin    
      case (present_state)
         State_0: begin
            clr_en    = 1;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = 0;
            target    = target + 4'h0;
         end 
         State_1: begin
            clr_en    = 0;
            cap_en    = 1;
            delta_en  = 0;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = target + 4'h0;
         end
         State_2: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = target + 4'h0;
         end
         State_3: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = t1;
         end
         State_4: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = t2;
         end     
         State_5: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = t3;
         end  
         State_6: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = t4;
         end        
         State_7: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 1;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = target + 4'h0;
         end
         State_8: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 1;
            iteration = iteration + 1;
            target    = target + 4'h0;
         end
         State_9: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = target + 4'h0;
         end  
         State_10: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = iteration + 0;
            target    = target + 4'h0;
         end
         default: begin
            clr_en    = 0;
            cap_en    = 0;
            delta_en  = 0;
            sum_en    = 0;
            iteration = 0;
            target    = target + 4'h0;
         end
      endcase
   end
   
   
   assign w1   = w1_reg;
   assign w2   = w2_reg;
   assign bias = bias_reg;
   
endmodule


`timescale 1ns/1ps
module gate_target(
   input  logic        [1:0] gate_select,
   input  logic        [1:0] target_select,
   output logic signed [3:0] o_1,
   output logic signed [3:0] o_2,
   output logic signed [3:0] o_3,
   output logic signed [3:0] o_4
);
   always @(*) begin
      case (gate_select)
         2'b00: begin 
            case (target_select)
               2'b00: begin
                  o_1 =  4'b0001; 
                  o_2 = -4'b0001; 
                  o_3 = -4'b0001; 
                  o_4 = -4'b0001; 
               end
               2'b01: begin
                  o_1 = -4'b0001;
                  o_2 = -4'b0001;
                  o_3 = -4'b0001;
                  o_4 =  4'b0001;
               end
               2'b10: begin
                  o_1 = -4'b0001;
                  o_2 =  4'b0001;
                  o_3 = -4'b0001;
                  o_4 = -4'b0001;
               end
               2'b11: begin
                  o_1 = -4'b0001;
                  o_2 = -4'b0001;
                  o_3 =  4'b0001;
                  o_4 = -4'b0001;
               end
               default: begin
                  o_1 = 4'b0000; 
                  o_2 = 4'b0000; 
                  o_3 = 4'b0000; 
                  o_4 = 4'b0000; 
               end
            endcase
         end
         2'b01: begin 
            case (target_select)
               2'b00: begin  
                  o_1 =  4'b0001; 
                  o_2 =  4'b0001; 
                  o_3 =  4'b0001; 
                  o_4 = -4'b0001; 
               end
               2'b01: begin  
                  o_1 =  4'b0001; 
                  o_2 =  4'b0001; 
                  o_3 = -4'b0001; 
                  o_4 =  4'b0001;
               end
               2'b10: begin  
                  o_1 =  4'b0001; 
                  o_2 = -4'b0001; 
                  o_3 =  4'b0001; 
                  o_4 =  4'b0001;
               end
               2'b11: begin  
                  o_1 = -4'b0001; 
                  o_2 =  4'b0001; 
                  o_3 =  4'b0001; 
                  o_4 =  4'b0001;
               end 
               default: begin
                  o_1 = 4'b0000; 
                  o_2 = 4'b0000; 
                  o_3 = 4'b0000; 
                  o_4 = 4'b0000; 
               end
            endcase
         end                
         2'b10: begin 
            case (target_select)
               2'b00: begin
                  o_1 =  4'b0001; 
                  o_2 =  4'b0001; 
                  o_3 =  4'b0001; 
                  o_4 = -4'b0001; 
               end
               2'b01: begin
                  o_1 =  4'b0001;
                  o_2 =  4'b0001;
                  o_3 = -4'b0001;
                  o_4 =  4'b0001;
               end
               2'b10: begin
                  o_1 =  4'b0001;
                  o_2 = -4'b0001;
                  o_3 =  4'b0001;
                  o_4 =  4'b0001;
               end
               2'b11: begin
                  o_1 = -4'b0001;
                  o_2 =  4'b0001;
                  o_3 =  4'b0001;
                  o_4 =  4'b0001;
               end
               default: begin
                  o_1 = 4'b0000; 
                  o_2 = 4'b0000; 
                  o_3 = 4'b0000; 
                  o_4 = 4'b0000; 
               end
            endcase
         end
         2'b11: begin 
            case (target_select)
               2'b00: begin 
                  o_1 =  4'b0001; 
                  o_2 = -4'b0001; 
                  o_3 = -4'b0001; 
                  o_4 = -4'b0001; 
               end
               2'b01: begin
                  o_1 = -4'b0001; 
                  o_2 =  4'b0001; 
                  o_3 = -4'b0001; 
                  o_4 = -4'b0001;
               end
               2'b10: begin
                  o_1 = -4'b0001; 
                  o_2 = -4'b0001; 
                  o_3 =  4'b0001; 
                  o_4 = -4'b0001;
               end
               2'b11: begin
                  o_1 = -4'b0001; 
                  o_2 = -4'b0001; 
                  o_3 = -4'b0001; 
                  o_4 =  4'b0001; 
               end
               default: begin
                  o_1 = 4'b0000; 
                  o_2 = 4'b0000; 
                  o_3 = 4'b0000; 
                  o_4 = 4'b0000; 
               end
            endcase
         end
         default: begin
            o_1 = 4'b0000; 
            o_2 = 4'b0000; 
            o_3 = 4'b0000; 
            o_4 = 4'b0000; 
         end
      endcase
   end
endmodule
