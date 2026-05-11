module async_reset (
  input   logic        clk,
  input   logic        reset,

  output  logic        release_reset_o,
  output  logic        gate_clk_o,
  output  logic [4:0]  cnt_q
);

// --------------------------------------------------------
// Asynchronous reset and counter logic
// --------------------------------------------------------
always_ff @(posedge clk or posedge reset) begin
  if (reset) begin
    cnt_q <= 5'h1F; // Initialize to 31
  end else begin
    if (cnt_q > 0) begin
      cnt_q <= cnt_q - 1;
    end else begin
      cnt_q <= cnt_q; // Hold at 0
    end
  end
end

// --------------------------------------------------------
// Output assignments
// --------------------------------------------------------
assign gate_clk_o      = (cnt_q < 5'hE) && (cnt_q != 0); // (cnt_q < 14) && (cnt_q != 0)
assign release_reset_o = (cnt_q < 5'h8);                  // (cnt_q < 8)

endmodule