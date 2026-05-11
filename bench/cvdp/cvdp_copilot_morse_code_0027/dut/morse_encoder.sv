module morse_encoder (
    input wire [7:0] ascii_in,       // ASCII input character
    output reg [5:0] morse_out,      // Morse code output 
    output reg [3:0] morse_length    // Length of the Morse code sequence
);

    always @(*) begin
        case (ascii_in)
            8'h41: begin morse_out = 6'b000001; morse_length = 2; end  
            8'h42: begin morse_out = 6'b100000; morse_length = 4; end  
            8'h43: begin morse_out = 6'b101000; morse_length = 4; end  
            8'h44: begin morse_out = 6'b010000; morse_length = 3; end  
            8'h45: begin morse_out = 6'b000001; morse_length = 1; end  
            8'h46: begin morse_out = 6'b001000; morse_length = 4; end  
            8'h47: begin morse_out = 6'b110000; morse_length = 3; end  
            8'h48: begin morse_out = 6'b000000; morse_length = 4; end  
            8'h49: begin morse_out = 6'b000001; morse_length = 2; end  
            8'h4A: begin morse_out = 6'b011100; morse_length = 4; end  
            8'h4B: begin morse_out = 6'b101000; morse_length = 3; end  
            8'h4C: begin morse_out = 6'b010000; morse_length = 4; end  
            8'h4D: begin morse_out = 6'b110000; morse_length = 2; end  
            8'h4E: begin morse_out = 6'b100000; morse_length = 2; end  
            8'h4F: begin morse_out = 6'b111000; morse_length = 3; end  
            8'h50: begin morse_out = 6'b011000; morse_length = 4; end  
            8'h51: begin morse_out = 6'b110100; morse_length = 4; end  
            8'h52: begin morse_out = 6'b010000; morse_length = 3; end  
            8'h53: begin morse_out = 6'b000000; morse_length = 3; end  
            8'h54: begin morse_out = 6'b000001; morse_length = 1; end  
            8'h55: begin morse_out = 6'b001000; morse_length = 3; end  
            8'h56: begin morse_out = 6'b000100; morse_length = 4; end  
            8'h57: begin morse_out = 6'b011000; morse_length = 3; end  
            8'h58: begin morse_out = 6'b100100; morse_length = 4; end  
            8'h59: begin morse_out = 6'b101100; morse_length = 4; end  
            8'h5A: begin morse_out = 6'b110000; morse_length = 4; end  
            8'h30: begin morse_out = 6'b111111; morse_length = 5; end  
            8'h31: begin morse_out = 6'b011111; morse_length = 5; end  
            8'h32: begin morse_out = 6'b001111; morse_length = 5; end  
            8'h33: begin morse_out = 6'b000111; morse_length = 5; end  
            8'h34: begin morse_out = 6'b000011; morse_length = 5; end  
            8'h35: begin morse_out = 6'b000001; morse_length = 5; end  
            8'h36: begin morse_out = 6'b100000; morse_length = 5; end  
            8'h37: begin morse_out = 6'b110000; morse_length = 5; end  
            8'h38: begin morse_out = 6'b111000; morse_length = 5; end  
            8'h39: begin morse_out = 6'b111100; morse_length = 5; end  
            8'h2E: begin morse_out = 6'b010101; morse_length = 6; end  
            8'h2C: begin morse_out = 6'b110011; morse_length = 6; end  
            8'h3F: begin morse_out = 6'b001100; morse_length = 6; end  
            8'h27: begin morse_out = 6'b011110; morse_length = 6; end  
            8'h21: begin morse_out = 6'b101011; morse_length = 6; end  
            8'h2F: begin morse_out = 6'b100010; morse_length = 5; end  
            8'h28: begin morse_out = 6'b101100; morse_length = 5; end  
            8'h29: begin morse_out = 6'b101101; morse_length = 6; end  
            8'h26: begin morse_out = 6'b010000; morse_length = 5; end  
            8'h3A: begin morse_out = 6'b111000; morse_length = 6; end  
            8'h3B: begin morse_out = 6'b101010; morse_length = 6; end  
            8'h3D: begin morse_out = 6'b100001; morse_length = 5; end  
            8'h2B: begin morse_out = 6'b010100; morse_length = 5; end  
            8'h2D: begin morse_out = 6'b100001; morse_length = 6; end  
            8'h22: begin morse_out = 6'b010010; morse_length = 6; end  
            8'h40: begin morse_out = 6'b011010; morse_length = 6; end  
            default: begin
                morse_out = 6'b000000;                 
                morse_length = 4'b0000;
            end
        endcase
    end
endmodule
