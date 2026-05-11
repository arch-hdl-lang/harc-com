`timescale 1ns / 1ps

module wishbone_to_ahb_bridge
  // Wishbone ports from WB master
 (
  input  clk_i,             // Clock input for Wishbone
  input  rst_i,             // Reset input for Wishbone
  input  cyc_i,             // Cycle signal from Wishbone master
  input  stb_i,             // Strobe signal from Wishbone master
  input  [3:0] sel_i,       // Byte enable signals from Wishbone master
  input  we_i,              // Write enable signal from Wishbone master
  input  [31:0] addr_i,     // Address signal from Wishbone master
  input  [31:0] data_i,     // Data signal from Wishbone master
  output reg [31:0] data_o, // Data output to Wishbone master
  output ack_o,             // Acknowledgment signal to Wishbone master

  // AHB ports to AHB slave
  input  hclk,              // Clock input for AHB
  input  hreset_n,          // Reset input for AHB (active low)
  input  [31:0] hrdata,     // Data input from AHB slave
  input [1:0] hresp,        // Response signal from AHB slave
  input hready,             // Ready signal from AHB slave
  output [1:0] htrans,      // Transfer type signal for AHB
  output [2:0] hsize,       // Transfer size signal for AHB
  output [2:0] hburst,      // Burst type signal for AHB (always SINGLE)
  output hwrite,            // Write signal for AHB
  output [31:0] haddr,      // Address signal for AHB
  output reg [31:0] hwdata  // Data output to AHB slave
);

// Internal signals
reg ahb_prev_addr_hold;  // Holds the address phase state
reg ahb_data_phase;      // Indicates data phase
reg [1:0] trans;         // Transfer type (Idle, Non-sequential, etc.)
reg [2:0] size;          // Transfer size (Byte, Half-word, Word)
reg [31:0] addr_hold;    // Holds the address during wait states
reg [1:0] trans_hold;    // Holds transfer type during wait states
reg [2:0] size_hold;     // Holds transfer size during wait states
reg write_hold;          // Holds write signal during wait states
reg [31:0] wb_adr_fixed; // Fixed Wishbone address after alignment

// Fix up the Wishbone address
// Aligns the Wishbone address based on byte enables (sel_i)
always @(*) begin
  wb_adr_fixed[31:2] = addr_i[31:2]; 

  case (sel_i)
    4'b0001: wb_adr_fixed[1:0] = 2'b00; 
    4'b0010: wb_adr_fixed[1:0] = 2'b01;
    4'b0100: wb_adr_fixed[1:0] = 2'b10;
    4'b1000: wb_adr_fixed[1:0] = 2'b11;
    4'b0011: wb_adr_fixed[1:0] = 2'b00; 
    4'b1100: wb_adr_fixed[1:0] = 2'b10;
    4'b1111: wb_adr_fixed[1:0] = 2'b00; 
    default: wb_adr_fixed[1:0] = addr_i[1:0]; 
  endcase
end

always @(posedge hclk or negedge hreset_n) begin
  if (!hreset_n)
    ahb_prev_addr_hold <= 0;
  else if (!ahb_data_phase & stb_i & cyc_i & !hready)
    ahb_prev_addr_hold <= 1; 
  else
    ahb_prev_addr_hold <= 0; 
end

assign haddr  = ahb_prev_addr_hold ? addr_hold : wb_adr_fixed;
assign htrans = ahb_prev_addr_hold ? trans_hold : trans;
assign hsize  = ahb_prev_addr_hold ? size_hold : size;
assign hwrite = ahb_prev_addr_hold ? write_hold : we_i;
assign ack_o = ahb_data_phase ? hready : 0; 

assign hburst = 3'b000; // Fixed burst type (SINGLE)

always @(*) begin
  case (sel_i)
    4'b0001, 4'b0010, 4'b0100, 4'b1000: begin 
      hwdata = { data_i[7:0], data_i[15:8], data_i[23:16], data_i[31:24] };
      data_o = { hrdata[7:0], hrdata[15:8], hrdata[23:16], hrdata[31:24] };
    end
    4'b0011, 4'b1100: begin 
      hwdata = { data_i[15:0], data_i[31:16] };
      data_o = { hrdata[15:0], hrdata[31:16] };
    end
    default: begin 
      hwdata = data_i;
      data_o = hrdata;
    end
  endcase
end

always @(posedge hclk or negedge hreset_n) begin
  if (!hreset_n)
    ahb_data_phase <= 0;
  else if (ahb_data_phase & hready)
    ahb_data_phase <= 0; 
  else if (!ahb_data_phase & cyc_i & stb_i & hready)
    ahb_data_phase <= 1; 
end

always @(posedge hclk or negedge hreset_n) begin
  if (!hreset_n) begin
    addr_hold  <= 0;
    trans_hold <= 0;
    size_hold  <= 0;
    write_hold <= 0;
  end else if (!ahb_prev_addr_hold) begin
    addr_hold  <= wb_adr_fixed;
    trans_hold <= trans;
    size_hold  <= size;
    write_hold <= we_i;
  end
end

always @(*) begin
  if (ahb_data_phase)
    trans = 2'b00; 
  else if (cyc_i) begin
    if (stb_i)
      trans = 2'b10; 
    else
      trans = 2'b01; 
  end else
    trans = 2'b00; 
end

always @(*) begin
  case (sel_i)
    4'b0001, 4'b0010, 4'b0100, 4'b1000: size = 3'b000; 
    4'b0011, 4'b1100: size = 3'b001; 
    4'b1111: size = 3'b010; 
    default: size = 3'b010; 
  endcase
end

endmodule