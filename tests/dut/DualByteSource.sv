module DualByteSource (
  input  logic       clk,
  input  logic       rst,
  output logic       left_beat_valid,
  input  logic       left_beat_ready,
  output logic [7:0] left_beat_data,
  output logic       right_beat_valid,
  input  logic       right_beat_ready,
  output logic [7:0] right_beat_data
);
  logic [3:0] cycle;

  always_ff @(posedge clk) begin
    if (rst) begin
      cycle <= 0;
    end else begin
      cycle <= cycle + 1'b1;
    end
  end

  always_comb begin
    left_beat_valid = cycle == 2;
    left_beat_data = 8'd11;
    right_beat_valid = cycle == 5;
    right_beat_data = 8'd22;
  end
endmodule
