module microcode_sequencer (
   // Inputs
   input logic clk, // Input Clock
   input logic c_n_in,  // Input Carry for Carry Lookahead Adder
   input logic c_inc_in, // Input Carry for Carry Lookahead Program Counter Incrementer
   input logic r_en, // Auxiliary Register enable
   input logic cc, // Condition Code
   input logic ien, // Instruction Enable
   input logic [3:0] d_in, // Data Input
   input logic [4:0] instr_in, // 5-bit Instruction Opcode
   input logic oen, // Output Enable

   // Outputs
   output logic [3:0] d_out, // Data Output
   output logic c_n_out , // Output Carry from Carry Lookahead Adder
   output logic c_inc_out, // Output Carry from Program Counter Incrementer
   output logic full, // Stack Full Condition
   output logic empty, // Stack Empty Condition

   // Additional Outputs
   output logic g_n_out, // Group Generate output from  Carry Lookahead adder
   output logic p_n_out, // Group Propagate output from Carry Lookahead adder
   output logic g_inc_out, // Group Generate output from Carry Lookahead Program Counter Incrementer
   output logic p_inc_out  // Group Propagate output from Carry Lookahead Program Counter Incrementer
 );

 
 logic [3:0] pc_data_w;
 logic stack_reset_w;
 logic [3:0] stack_data_w;
 logic [3:0] fa_out_w;
 logic [3:0] data_out_w;
  
 logic rsel_w;
 logic rce_w;
 logic [1:0] a_mux_sel_w;
 logic [1:0] b_mux_sel_w;
 logic cen_w;
 logic rst_w;
 logic stack_push_w;
 logic stack_pop_w;
 logic pc_mux_sel_w;
 logic stack_mux_sel_w;
 logic stack_we_w;
 logic stack_re_w;
 logic inc_w;
 logic oen_w;
 logic out_en_w;
 
 lifo_stack dut_1 (
    .clk              (clk),
    .stack_data_1_in  (d_in),
    .stack_data_2_in  (pc_data_w),
    .stack_reset      (rst_w),
    .stack_push       (stack_push_w),
    .stack_pop        (stack_pop_w),
    .stack_mux_sel    (stack_mux_sel_w),
    .stack_we         (stack_we_w),
    .stack_re         (stack_re_w),
    .stack_data_out   (stack_data_w),
    .full_o           (full),
    .empty_o          (empty)
 );

 program_counter dut_2 (
    .clk               (clk),
    .full_adder_data_i (fa_out_w),
    .pc_c_in           (c_inc_in),
    .inc               (inc_w),
    .pc_mux_sel        (pc_mux_sel_w),
    .pc_out            (pc_data_w),
    .pc_c_out          (c_inc_out),
    .pc_g_out          (g_inc_out),
    .pc_p_out          (p_inc_out)
 );
 
 microcode_arithmetic dut_3(
    .clk              (clk),
    .fa_in            (data_out_w),
    .d_in             (d_in),
    .stack_data_in    (stack_data_w),
    .pc_data_in       (pc_data_w),
    .reg_en           (r_en),
    .oen              (oen_w),
    .rsel             (rsel_w),
    .rce              (rce_w),
    .cen              (cen_w),
    .a_mux_sel        (a_mux_sel_w),
    .b_mux_sel        (b_mux_sel_w),
    .arith_cin        (c_n_in),
    .arith_cout       (c_n_out),
    .arith_g_out      (g_n_out),
    .arith_p_out      (p_n_out),
    .oe               (oen),
    .d_out            (fa_out_w)
 );
 
 instruction_decoder dut_4 (
    .instr_in         (instr_in),
    .cc_in            (cc),
    .instr_en         (ien),
    .cen              (cen_w), 
    .rst              (rst_w), 
    .oen              (oen_w), 
    .inc              (inc_w), 
    .rsel             (rsel_w), 
    .rce              (rce_w),  
    .pc_mux_sel       (pc_mux_sel_w), 
    .a_mux_sel        (a_mux_sel_w), 
    .b_mux_sel        (b_mux_sel_w), 
    .push             (stack_push_w),
    .pop              (stack_pop_w), 
    .src_sel          (stack_mux_sel_w),
    .stack_we         (stack_we_w),
    .stack_re         (stack_re_w),
    .out_ce           (out_ce_w)
);

 result_register dut_5 (
   .clk     (clk),
   .data_in (fa_out_w),
   .out_ce  (out_ce_w),
   .data_out(data_out_w)
 );
assign d_out = fa_out_w;
endmodule
 
module stack_pointer (
  input logic clk,
  input logic rst,
  input logic push,
  input logic pop,
  output logic [4:0] stack_addr,
  output logic full,
  output logic empty
);
logic [4:0] stack_addr_reg;
logic full_r;
logic empty_r;
assign full_r     = (stack_addr_reg == 5'b10000);
assign empty_r    = (stack_addr_reg == 5'b00000);
always_ff@(posedge clk) begin
   if(rst)
     stack_addr_reg <= 5'b00000;
   else if(push && !full_r) begin
     stack_addr_reg <= stack_addr_reg + 5'b00001;
   end else if(pop && !empty_r) begin
     stack_addr_reg <= stack_addr_reg - 5'b00001;
   end 
   
end
assign full       = full_r;
assign empty      = empty_r;
assign stack_addr = stack_addr_reg; 
endmodule

module stack_ram (
  input logic clk,
  input logic  [4:0] stack_addr,
  input logic  [3:0] stack_data_in,
  input logic  stack_we,
  input logic  stack_re,
  output logic [3:0] stack_data_out
);

logic [3:0] stack_arr [16:0];
always_ff@(posedge clk) begin
  if(stack_we) begin
    stack_arr[stack_addr] = stack_data_in;
  end
end

assign stack_data_out = stack_re ? stack_arr[stack_addr] : '0; 
endmodule

module stack_data_mux (
  input  logic [3:0] data_in,
  input  logic [3:0] pc_in,
  input  logic stack_mux_sel,
  output logic [3:0] stack_mux_out
);
assign stack_mux_out = stack_mux_sel ? data_in : pc_in;
endmodule

module lifo_stack (
  input  logic clk,
  input  logic [3:0] stack_data_1_in,
  input  logic [3:0] stack_data_2_in,
  input  logic stack_reset,
  input  logic stack_push,
  input  logic stack_pop,
  input  logic stack_mux_sel,
  input  logic stack_we,
  input  logic stack_re,
  output logic [3:0] stack_data_out,
  output logic full_o,
  output logic empty_o
);

logic [3:0] stack_data_in_w;
logic [4:0] stack_addr_w;

stack_data_mux dut_1(
    .data_in       (stack_data_1_in),
    .pc_in         (stack_data_2_in),
    .stack_mux_sel (stack_mux_sel),
    .stack_mux_out (stack_data_in_w)
);

stack_pointer dut_2 (
     .clk        (clk),
     .rst        (stack_reset),
     .push       (stack_push),
     .pop        (stack_pop),
     .stack_addr (stack_addr_w),
     .full       (full_o),
     .empty      (empty_o)
);

stack_ram dut_3 (
     .clk            (clk),
     .stack_addr     (stack_addr_w),
     .stack_data_in  (stack_data_in_w),
     .stack_we       (stack_we),
     .stack_re       (stack_re),
     .stack_data_out (stack_data_out)
);
endmodule

module pc_mux (
    input logic [3:0] full_adder_data,
    input logic [3:0] pc_data,
    input logic pc_mux_sel,
    output logic [3:0] pc_mux_out
);
assign pc_mux_out = pc_mux_sel ? pc_data : full_adder_data;
endmodule


module pc_incrementer (
  input  logic pc_c_in,         
  input  logic inc,             
  input  logic [3:0] pc_data_in,
  output logic [3:0] pc_inc_out,
  output logic pc_c_out,        
  output logic pc_g_out,        
  output logic pc_p_out         
);
  
    logic g[3:0];
    logic p[3:0];
    logic c[4:0];

    assign g[0]  = 1'b0;
    assign g[1]  = 1'b0;
    assign g[2]  = 1'b0;
    assign g[3]  = 1'b0;

    assign p[0]  = pc_data_in[0] ^ 1'b0; 
    assign p[1]  = pc_data_in[1] ^ 1'b0; 
    assign p[2]  = pc_data_in[2] ^ 1'b0; 
    assign p[3]  = pc_data_in[3] ^ 1'b0; 
    
    assign c[0]  = pc_c_in;
    assign c[1]  = (p[0] & c[0]);
    assign c[2]  = (c[0] & p[0] & p[1]);
    assign c[3]  = (c[0] & p[0] & p[1] & p[2]);
    assign c[4]  = (c[0] & p[0] & p[1] & p[2] & p[3]);

    assign pc_inc_out[0]  = inc ? p[0] ^ c[0] : p[0];
    assign pc_inc_out[1]  = inc ? p[1] ^ c[1] : p[1];
    assign pc_inc_out[2]  = inc ? p[2] ^ c[2] : p[2];
    assign pc_inc_out[3]  = inc ? p[3] ^ c[3] : p[3];
    assign pc_p_out = p[0] & p[1] & p[2] & p[3];
    assign pc_g_out = 1'b0;
    assign pc_c_out = c[4];
endmodule


module pc_reg (
 input logic clk,
 input logic[3:0] pc_data_in,
 output logic [3:0] pc_data_out
);
always_ff@(posedge clk) begin
   pc_data_out <= pc_data_in;
   
end
endmodule

module program_counter(
  input logic clk,
  input logic [3:0] full_adder_data_i,
  input logic pc_c_in,
  input logic inc,
  input logic pc_mux_sel,
  output logic [3:0] pc_out,
  output logic pc_c_out,
  output logic pc_g_out,
  output logic pc_p_out
);
 
 logic [3:0] pc_out_w;
 logic [3:0] pc_mux_out_w;
 logic [3:0] pc_inc_out_w;
 logic pc_c_out_w;
 
 
pc_mux dut_1 (
   .full_adder_data(full_adder_data_i),
   .pc_data(pc_out_w),
   .pc_mux_sel(pc_mux_sel),
   .pc_mux_out(pc_mux_out_w)
);

pc_incrementer dut_2 (
  .pc_c_in(pc_c_in),
  .inc(inc),
  .pc_data_in(pc_mux_out_w),
  .pc_inc_out(pc_inc_out_w),
  .pc_c_out(pc_c_out_w),
  .pc_g_out(pc_g_out),
  .pc_p_out(pc_p_out)
);

pc_reg dut_3 (
   .clk(clk),
   .pc_data_in(pc_inc_out_w),
   .pc_data_out(pc_out_w)
);

assign pc_out = pc_out_w;
assign pc_c_out = pc_c_out_w;
endmodule

module instruction_decoder(
    input logic [4:0] instr_in,
    input logic cc_in,
    input logic instr_en,
    output logic cen, 
    output logic rst, 
    output logic oen, 
    output logic inc, 
    output logic rsel, 
    output logic rce,  
    output logic pc_mux_sel, 
    output logic [1:0] a_mux_sel, 
    output logic [1:0] b_mux_sel, 
    output logic push,
    output logic pop, 
    output logic src_sel,
    output logic stack_we,
    output logic stack_re,
    output logic out_ce
 );

 always_comb begin
    casex({instr_in,cc_in,instr_en})
       7'bxxxxxx1 : begin  
                      rst        = 1'b0;
                      out_ce     = 1'b0;
                      rsel       = 1'b0;
                      rce        = 1'b0;
                      cen        = 1'b0;
                      stack_re   = 1'b0;
                      pop        = 1'b0;
                      a_mux_sel  = 2'b10;
                      b_mux_sel  = 2'b10;
                      oen        = 1'b1;
                      pc_mux_sel = 1'b0;
                      inc        = 1'b0;
                      src_sel    = 1'b0;
                      push       = 1'b0;
                      stack_we   = 1'b0;
                   end
      7'b00000x0 : begin  
                      rst        = 1'b1;
                      out_ce     = 1'b0;
                      rsel       = 1'b0;
                      rce        = 1'b1; 
                      cen        = 1'b1;  
                      stack_re   = 1'b0;
                      pop        = 1'b0;
                      a_mux_sel  = 2'b10;  
                      b_mux_sel  = 2'b10;  
                      oen        = 1'b1;  
                      pc_mux_sel = 1'b0; 
                      inc        = 1'b1; 
                      src_sel    = 1'b0; 
                      push       = 1'b0;
                      stack_we   = 1'b0;
                   end
      7'b00001x0 : begin  
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b00;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                   end
      7'b00010x0 : begin  
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b01; 
                     b_mux_sel   = 2'b10;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                   end
      7'b00011x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0;  
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00; 
                     b_mux_sel   = 2'b10;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
     7'b00100x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1;  
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00; 
                     b_mux_sel   = 2'b11;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
     7'b00101x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00; 
                     b_mux_sel   = 2'b00;  
                     oen         = 1'b1;  
                     pc_mux_sel  = 1'b1;  
                     inc         = 1'b1;  
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
     7'b00110x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b01; 
                     b_mux_sel   = 2'b00;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
     7'b00111x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b1;
                     pop         = 1'b1;
                     a_mux_sel   = 2'b00; 
                     b_mux_sel   = 2'b01;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
     7'b01000x0 : begin  
                     rst         = 1'b0;
                     out_ce      = 1'b1;
                     rsel        = 1'b1;
                     rce         = 1'b0;
                     cen         = 1'b0; 
                     stack_re    = 1'b0; 
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b00; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
     7'b01001x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b1;
                     rsel        = 1'b0;
                     rce         = 1'b1;
                     cen         = 1'b1; 
                     stack_re    = 1'b0;  
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00; 
                     b_mux_sel   = 2'b11; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0; 
                     stack_we    = 1'b0;
                  end                    
     7'b01010x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b1;
                     rce         = 1'b0;
                     cen         = 1'b0; 
                     stack_re    = 1'b0;  
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b00; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
     7'b01011x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0;
                     rce         = 1'b1;
                     cen         = 1'b0; 
                     stack_re    = 1'b0;  
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b00; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0; 
                     push        = 1'b1; 
                     stack_we    = 1'b1; 
                  end   
     7'b01100x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0;
                     rce         = 1'b1;
                     cen         = 1'b0; 
                     stack_re    = 1'b0; 
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b00; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b1; 
                     push        = 1'b1; 
                     stack_we    = 1'b1; 
                  end
     7'b01101x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0;
                     rce         = 1'b1;
                     cen         = 1'b0; 
                     stack_re    = 1'b1; 
                     pop         = 1'b1; 
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b01; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0; 
                     push        = 1'b0; 
                     stack_we    = 1'b0; 
                  end    
    7'b01110x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0;
                     rce         = 1'b1;
                     cen         = 1'b0; 
                     stack_re    = 1'b1; 
                     pop         = 1'b1; 
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b01; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0; 
                     push        = 1'b0; 
                     stack_we    = 1'b0; 
                 end    
    7'b01111x0 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0;
                     rce         = 1'b1;
                     cen         = 1'b0; 
                     stack_re    = 1'b1; 
                     pop         = 1'b1; 
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b00; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b0; 
                     src_sel     = 1'b0; 
                     push        = 1'b0; 
                     stack_we    = 1'b0; 
                  end
     7'b1xxxx10 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10; 
                     b_mux_sel   = 2'b00;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                   end 
     7'b1000000 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b01;
                     b_mux_sel   = 2'b10;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
     7'b1000100 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00;
                     b_mux_sel   = 2'b10;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end
    7'b1001000 :  begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10;
                     b_mux_sel   = 2'b10; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                  end  
    7'b1001100 :  begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00;
                     b_mux_sel   = 2'b11;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;  
                  end
    7'b1010000 :  begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00;
                     b_mux_sel   = 2'b00;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0; 
                  end  
                   
    7'b1010100 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b01;
                     b_mux_sel   = 2'b00;  
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;  
                 end                    
    7'b1011000 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b01;
                     b_mux_sel   = 2'b10; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b1;
                     stack_we    = 1'b1;
                 end
    7'b1011100 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00;
                     b_mux_sel   = 2'b10; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b1;
                     stack_we    = 1'b1; 
                 end
    7'b1100000 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10;
                     b_mux_sel   = 2'b10; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b1;
                     stack_we    = 1'b1; 
                 end
    7'b1100100 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00;
                     b_mux_sel   = 2'b11; 
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b1;
                     stack_we    = 1'b1;
                 end
    7'b1101000 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b00;
                     b_mux_sel   = 2'b00;
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b1;
                     stack_we    = 1'b1; 
                 end
    7'b1101100 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b0;
                     pop         = 1'b0;
                     a_mux_sel   = 2'b01;
                     b_mux_sel   = 2'b00;
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b1;
                     stack_we    = 1'b1; 
                end 
   7'b1110000 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b1;
                     pop         = 1'b1;
                     a_mux_sel   = 2'b10;
                     b_mux_sel   = 2'b01;
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0; 
                end
   7'b1110100 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b1; 
                     stack_re    = 1'b1; 
                     pop         = 1'b1;
                     a_mux_sel   = 2'b00;
                     b_mux_sel   = 2'b01;
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b1; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;  
               end
  7'b1111000 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0; 
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10;
                     b_mux_sel   = 2'b00;
                     oen         = 1'b1; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b0; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0; 
               end
  7'b1111100 : begin 
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b1; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0; 
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10;
                     b_mux_sel   = 2'b00;
                     oen         = 1'b0; 
                     pc_mux_sel  = 1'b1; 
                     inc         = 1'b0; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                end        
    default   : begin
                     rst         = 1'b0;
                     out_ce      = 1'b0;
                     rsel        = 1'b0; 
                     rce         = 1'b0; 
                     cen         = 1'b0; 
                     stack_re    = 1'b0; 
                     pop         = 1'b0;
                     a_mux_sel   = 2'b10;
                     b_mux_sel   = 2'b10;
                     oen         = 1'b0; 
                     pc_mux_sel  = 1'b0; 
                     inc         = 1'b0; 
                     src_sel     = 1'b0;
                     push        = 1'b0;
                     stack_we    = 1'b0;
                   end 
     endcase
  end
endmodule


module full_adder (
    input  logic [3:0] a_in,
    input  logic [3:0] b_in,
    input  logic cen,
    input  logic c_in,
    output logic [3:0] y_out,
    output logic c_out,
    output logic g_out,
    output logic p_out
);
    
    logic g[3:0];
    logic p[3:0];
    logic c[4:0];

    assign g[0]  = a_in[0] & b_in[0];
    assign g[1]  = a_in[1] & b_in[1];
    assign g[2]  = a_in[2] & b_in[2];
    assign g[3]  = a_in[3] & b_in[3];

    assign p[0]  = a_in[0] ^ b_in[0];
    assign p[1]  = a_in[1] ^ b_in[1];
    assign p[2]  = a_in[2] ^ b_in[2];
    assign p[3]  = a_in[3] ^ b_in[3];
    
    assign c[0]  = c_in;
    assign c[1]  = g[0] | (p[0] & c[0]);
    assign c[2]  = g[1] | (g[0] & p[1]) | (c[0] & p[0] & p[1]);
    assign c[3]  = g[2] | (g[1] & p[2]) | (g[0] & p[1] & p[2]) | (c[0] & p[0] & p[1] & p[2]);
    assign c[4]  = g[3] | (g[2] & p[3]) | (g[1] & p[2] & p[3]) | (g[0] & p[1] & p[2] & p[3]) | (c[0] & p[0] & p[1] & p[2] & p[3]);

    assign y_out[0]  = cen ? p[0] ^ c[0] : p[0];
    assign y_out[1]  = cen ? p[1] ^ c[1] : p[1];
    assign y_out[2]  = cen ? p[2] ^ c[2] : p[2];
    assign y_out[3]  = cen ? p[3] ^ c[3] : p[3];
    assign p_out = p[0] & p[1] & p[2] & p[3];
    assign g_out = g[3] | (g[2] & p[3]) | (g[1] & p[3] & p[2]) | (g[0] & p[3] & p[2] & p[1]);
    assign c_out = c[4];
endmodule

module aux_reg_mux (
  input logic [3:0] reg1_in, 
  input logic [3:0] reg2_in, 
  input logic rsel,
  input logic re,
  output logic [3:0] reg_mux_out
);

assign reg_mux_out = (~re & rsel) ? reg1_in : reg2_in;
endmodule

module aux_reg (
    input logic clk,
    input logic [3:0] reg_in,
    input logic rce,
    input logic re,
    output logic [3:0] reg_out
);
 always_ff@(posedge clk) begin
   if(rce | ~re) 
    reg_out <= reg_in;
 end
endmodule

module a_mux (
  input logic [3:0] register_data,
  input logic [3:0] data_in,
  input logic [1:0] a_mux_sel,
  output logic [3:0] a_mux_out
);
always_comb begin
  case(a_mux_sel) 
      2'b00 : a_mux_out = data_in;
      2'b01 : a_mux_out = register_data;
      2'b10 : a_mux_out = 4'b0000;
      default : a_mux_out = 4'b0000;
  endcase
end
endmodule

module b_mux (
  input logic [3:0] register_data,
  input logic [3:0] stack_data,
  input logic [3:0] pc_data,
  input logic [1:0] b_mux_sel,
  output logic [3:0] b_mux_out
);

always_comb begin
    case(b_mux_sel)
      2'b00 : b_mux_out = pc_data;
      2'b01 : b_mux_out = stack_data;
      2'b10 : b_mux_out = 4'b0000;
      2'b11 : b_mux_out = register_data;
      default : ;
    endcase
end
endmodule

module microcode_arithmetic ( 
  input  logic clk,
  input  logic [3:0] fa_in,
  input  logic [3:0] d_in,
  input  logic [3:0] stack_data_in,
  input  logic [3:0] pc_data_in,
  input  logic       reg_en,
  input  logic       oen,
  input  logic       rsel,
  input  logic       rce,
  input  logic       cen,
  input  logic [1:0] a_mux_sel,
  input  logic [1:0] b_mux_sel,
  input  logic       arith_cin,
  output logic       arith_cout,
  output logic       arith_g_out,
  output logic       arith_p_out,
  input  logic       oe,
  output logic [3:0] d_out
);

logic [3:0] fa_out_w;
logic [3:0] reg_mux_out_w;
logic [3:0] reg_out_w;
logic [3:0] a_mux_out_w;
logic [3:0] b_mux_out_w;


aux_reg_mux dut_1 (
  .reg1_in     (fa_in),
  .reg2_in     (d_in),
  .rsel        (rsel),
  .re          (reg_en),
  .reg_mux_out (reg_mux_out_w)
);

aux_reg dut_2 (
    .clk     (clk),
    .reg_in  (reg_mux_out_w),
    .rce     (rce),
    .re      (reg_en),
    .reg_out (reg_out_w)
);

a_mux dut_3(
  .register_data(reg_out_w),
  .data_in      (d_in),
  .a_mux_sel    (a_mux_sel),
  .a_mux_out    (a_mux_out_w)
);

b_mux dut_4(
  .register_data  (reg_out_w),
  .stack_data     (stack_data_in),
  .pc_data        (pc_data_in),
  .b_mux_sel      (b_mux_sel),
  .b_mux_out      (b_mux_out_w)
);

full_adder dut_5 (
  .a_in  (a_mux_out_w),
  .b_in  (b_mux_out_w),
  .cen   (cen),
  .c_in  (arith_cin),
  .y_out (fa_out_w),
  .c_out (arith_cout),
  .g_out (arith_g_out),
  .p_out (arith_p_out)
);
assign d_out = oen & ~oe ? fa_out_w : 4'bzzzz;
endmodule
 
module result_register ( 
  input  logic clk,
  input  logic [3:0] data_in,
  input  logic out_ce,
  output logic [3:0] data_out
);
always_latch begin
   if(out_ce)
     data_out = data_in;
end
endmodule    
