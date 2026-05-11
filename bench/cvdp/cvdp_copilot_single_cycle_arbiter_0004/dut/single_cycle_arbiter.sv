module single_cycle_arbiter #(
  parameter N = 32
) (
  input   logic          clk,
  input   logic          reset,
  input   logic [N-1:0]  req_i,
  output  logic [N-1:0]  gnt_o
);

  // --------------------------------------------------------
  // Internal wire and regs
  // --------------------------------------------------------
  logic [N-1:0] priority_req;

  // --------------------------------------------------------
  // Arbitration logic
  // --------------------------------------------------------
  assign priority_req[0] = 1'b0;
  if (N > 1) begin : PR_GEN
    for (genvar i = 0; i < N-1; i++) begin
      // Port[0] highest priority
      assign priority_req[i+1] = priority_req[i] | req_i[i];
    end
  end

  // -------------------------------------------------------
  // Output assignments
  // --------------------------------------------------------
  assign gnt_o[N-1:0] = req_i[N-1:0] & ~priority_req[N-1:0];

endmodule
