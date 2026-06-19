// stream_burst — a minimal valid/ready stream source that HOLDS the
// handshake high across multiple consecutive cycles, emitting a new
// payload (`strm_s_data`) each cycle the beat is accepted.
//
// Purpose: exercise the bound-monitor sampling cadence for a
// MULTI-CYCLE-HELD handshake. A single-beat handshake (valid high for
// one cycle) cannot distinguish v1's `wait_until + wait_cycles(1)`
// coroutine cadence from a naive rising-edge or level checker; this DUT
// holds `strm_s_valid` high for several cycles in a row so the cadence is
// observable.
//
// Behaviour after reset deasserts:
//   - `strm_s_valid` is held high for BURST_LEN consecutive cycles.
//   - while valid && ready, `strm_s_data` increments each accepted beat
//     (1, 2, 3, ... so each captured beat carries a distinct value).
//   - after the burst, `strm_s_valid` drops and stays low.
//
// Ports use the flat `<channel>_<signal>` naming the HARC bus binding
// expects (channel `s`, signals `valid`/`ready`/`data`).
module stream_burst #(
  parameter BURST_LEN = 5
) (
  input  logic       clk,
  input  logic       rst,
  output logic       strm_s_valid,
  input  logic       strm_s_ready,
  output logic [7:0] strm_s_data
);
  logic [7:0] count;   // beats accepted so far
  logic       running;

  assign strm_s_valid = running;
  // Payload is the 1-based index of the beat about to be accepted.
  assign strm_s_data  = count + 8'd1;

  always_ff @(posedge clk) begin
    if (rst) begin
      count   <= 8'd0;
      running <= 1'b1;
    end else begin
      if (running && strm_s_ready) begin
        count <= count + 8'd1;
        if (count + 8'd1 >= BURST_LEN[7:0]) begin
          running <= 1'b0;
        end
      end
    end
  end
endmodule
