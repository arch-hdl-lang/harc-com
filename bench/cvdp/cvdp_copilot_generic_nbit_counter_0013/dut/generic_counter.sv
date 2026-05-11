
module generic_counter #(parameter N = 8) (
    input logic clk_in,          // Clock input
    input logic rst_in,          // Active HIGH Reset input
    input logic [2:0] mode_in,   // Mode input (3 bits)
    input logic enable_in,       // Enable input
    input logic [N-1:0] ref_modulo, // Reference modulo value for Modulo-N counter
    output logic [N-1:0] o_count   // Output count (N bits)
);

    parameter BINARY_UP = 3'b000;
    parameter BINARY_DOWN = 3'b001;
    parameter MODULO_256 = 3'b010;
    parameter JOHNSON = 3'b011;
    parameter GRAY = 3'b100;
    parameter RING = 3'b101;

    logic [N-1:0] count;

    always_ff @(posedge clk_in or posedge rst_in) begin
        if (rst_in) begin
            count <= {N{1'b0}};
        end else if (enable_in) begin
            case (mode_in)
                BINARY_UP: begin
                    count <= count + 1;
                end
                
		BINARY_DOWN: begin
                    count <= count - 1;
                end
                
		MODULO_256: begin
                    if (count == ref_modulo) begin
                        count <= {N{1'b0}};
                    end else begin
                        count <= count + 1;
                    end
                end
                
		JOHNSON: begin
                    count <= {~count[0], count[N-1:1]};
                end
                
		GRAY: begin
                    count <= count + 1;
                end
                
		RING: begin
                    if (count == {N{1'b0}}) begin
                        count <= {{(N-1){1'b0}}, 1'b1};
                    end else begin
                        count <= {count[N-2:0], count[N-1]};
                    end
                end		
                
		default: begin
                    count <= {N{1'b0}};
                end
            endcase
        end
    end

    
    assign o_count = (mode_in == GRAY) ? ((count) ^ (count >> 1)) : count;

endmodule
