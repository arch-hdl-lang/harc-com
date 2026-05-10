// Minimal 256-bit register DUT for wide-bit-vector codegen testing.
// Captures `data_in` into `data_out` when `we` is high; otherwise
// holds. 256 bits exceeds the `_harc_u128` (128b) capacity, so
// HARC's testbench codegen must route assignments and equality
// through `harc_assign_words` / `harc_eq_words`.

module WideReg #(
  parameter int WIDTH = 256
) (
  input  logic              clk,
  input  logic              rst,
  input  logic              we,
  input  logic [WIDTH-1:0]  data_in,
  output logic [WIDTH-1:0]  data_out
);

  always_ff @(posedge clk) begin
    if (rst) begin
      data_out <= '0;
    end else if (we) begin
      data_out <= data_in;
    end
  end

endmodule
