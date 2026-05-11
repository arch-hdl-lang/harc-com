`timescale 1ns / 1ps

module gray_to_binary #(
    parameter WIDTH = 4
) (
    input  logic [WIDTH-1:0] gray_in,    // Gray code input
    output logic [WIDTH-1:0] binary_out  // Binary output
);

  always @* begin
    binary_out[WIDTH-1] = gray_in[WIDTH-1];
    for (int i = WIDTH - 2; i >= 0; i = i - 1) begin
      binary_out[i] = binary_out[i+1] ^ gray_in[i];
    end
  end

endmodule

