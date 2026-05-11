`timescale 1ns / 1ps

module fixed_priority_arbiter(
    input clk,             // Clock signal
    input reset,           // Active high reset signal
    input [7:0] req,       // 8-bit request signal; each bit represents a request from a different source
    output reg [7:0] grant // 8-bit grant signal; only one bit will be set high based on priority
); 
  
    always @(posedge clk or posedge reset) begin
    
        if (reset) 
            grant <= 8'b00000000;
        else begin
            if (req[0])
                grant <= 8'b00000001;
            else if (req[1])
                grant <= 8'b00000010;
            else if (req[2])
                grant <= 8'b00000100;
            else if (req[3])
                grant <= 8'b00001000;
            else if (req[4])
                grant <= 8'b00010000;
            else if (req[5])
                grant <= 8'b00100000;
            else if (req[6])
                grant <= 8'b01000000;
            else if (req[7])
                grant <= 8'b10000000;
            else
                grant <= 8'b00000000;
        end
    end
endmodule