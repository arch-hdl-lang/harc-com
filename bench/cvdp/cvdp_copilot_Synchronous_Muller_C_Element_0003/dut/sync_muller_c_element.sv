module sync_muller_c_element #(
  parameter NUM_INPUT  = 2, // Number of input signals
  parameter PIPE_DEPTH = 1  // Number of pipeline stages
) (
  input  logic                  clk   , // Clock signal
  input  logic                  srst  , // Synchronous reset signal
  input  logic                  clr   , // Clear pipeline and output
  input  logic                  clk_en, // Clock enable signal
  input  logic  [NUM_INPUT-1:0] inp   , // Input signals (NUM_INPUT-width vector)
  output logic                  out     // Output signal
);

  // Pipeline to store intermediate states of inputs
  logic [(PIPE_DEPTH*NUM_INPUT)-1:0] pipe; 
  genvar i;

  // Generate block for pipeline implementation
  generate
    for (i = 0; i < PIPE_DEPTH; i = i + 1) begin : pipeline_stage
      always_ff @(posedge clk) begin
        if (srst | clr) begin
          // Reset the pipeline stage
          pipe[i*NUM_INPUT+:NUM_INPUT] <= '0;
        end
        else if (clk_en) begin
          if (i == 0) begin
            // Load inputs into the first stage of the pipeline
            pipe[i*NUM_INPUT+:NUM_INPUT] <= inp;
          end
          else begin
            // Shift data from the previous stage to the current stage
            pipe[i*NUM_INPUT+:NUM_INPUT] <= pipe[(i-1)*NUM_INPUT+:NUM_INPUT];
          end
        end
      end
    end
  endgenerate

  // Always block to compute the output signal
  always_ff @(posedge clk) begin
    if (srst | clr) begin
      // Reset the output signal
      out <= 1'b0;
    end
    else if (clk_en) begin
      // Output logic: High if all inputs in the last pipeline stage are high
      // Low if all inputs in the last pipeline stage are low
      if (&pipe[(PIPE_DEPTH-1)*NUM_INPUT+:NUM_INPUT]) begin
        out <= 1'b1; 
      end
      else if (!(|pipe[(PIPE_DEPTH-1)*NUM_INPUT+:NUM_INPUT])) begin
        out <= 1'b0;
      end
    end
  end

endmodule