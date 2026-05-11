module matrix_multiplier #(
  parameter ROW_A             = 4                                                   , // Number of rows in matrix A
  parameter COL_A             = 4                                                   , // Number of columns in matrix A
  parameter ROW_B             = 4                                                   , // Number of rows in matrix B
  parameter COL_B             = 4                                                   , // Number of columns in matrix B
  parameter INPUT_DATA_WIDTH  = 8                                                   , // Bit-width of input data
  parameter OUTPUT_DATA_WIDTH = (INPUT_DATA_WIDTH * 2) + $clog2(COL_A)                // Bit-width of output data
) (
  input  logic                                       clk      , // Clock input
  input  logic                                       srst     , // Active-high Synchronous reset
  input  logic                                       valid_in , // Indicates valid input matrices
  input  logic [ (ROW_A*COL_A*INPUT_DATA_WIDTH)-1:0] matrix_a , // Input matrix A in 1D form
  input  logic [ (ROW_B*COL_B*INPUT_DATA_WIDTH)-1:0] matrix_b , // Input matrix B in 1D form
  output logic                                       valid_out, // Indicates valid output matrix
  output logic [(ROW_A*COL_B*OUTPUT_DATA_WIDTH)-1:0] matrix_c   // Output matrix C in 1D form
);


  localparam MODIFIED_COL_A      = 1<<($clog2(COL_A));
  localparam HALF_MODIFIED_COL_A = MODIFIED_COL_A/2  ;

  generate
    logic [                                             $clog2(COL_A):0] valid_out_reg;
    logic [     (ROW_A*COL_B*MODIFIED_COL_A*(INPUT_DATA_WIDTH * 2))-1:0] mult_stage   ;

    always_ff @(posedge clk)
      if (srst)
        {valid_out, valid_out_reg} <= '0;
      else
        {valid_out, valid_out_reg} <= {valid_out_reg, valid_in}; 

    always_ff @(posedge clk)
      for (int i = 0 ; i < ROW_A ; i++) begin: mult_row_a_gb
        for (int j = 0 ; j < COL_B ; j++) begin: mult_col_b_gb
          for (int k = 0 ; k < MODIFIED_COL_A ; k++) begin: mult_gb
              if (srst)
                mult_stage[((((i*COL_B)+j)*MODIFIED_COL_A)+k)*(INPUT_DATA_WIDTH * 2)+:(INPUT_DATA_WIDTH * 2)] <= '0; 
              else
                mult_stage[((((i*COL_B)+j)*MODIFIED_COL_A)+k)*(INPUT_DATA_WIDTH * 2)+:(INPUT_DATA_WIDTH * 2)] <=  (k < COL_A) ? matrix_a[((i*COL_A)+k)*INPUT_DATA_WIDTH+:INPUT_DATA_WIDTH] * matrix_b[((k*COL_B)+j)*INPUT_DATA_WIDTH+:INPUT_DATA_WIDTH] : '0; 
          end
        end
      end

    if (HALF_MODIFIED_COL_A > 0) begin
      logic [($clog2(COL_A)*ROW_A*COL_B*HALF_MODIFIED_COL_A*OUTPUT_DATA_WIDTH)-1:0] add_stage    ; 
      always_ff @(posedge clk)
        for (int i = 0 ; i < ROW_A ; i++) begin: accum_row_a_gb
          for (int j = 0 ; j < COL_B ; j++) begin: accum_col_b_gb
            for (int k = 0 ; k < HALF_MODIFIED_COL_A ; k++) begin: accum_gb
              for (int l = 0 ; l < $clog2(COL_A) ; l++) begin: pipe_gb
                if (l == 0) begin
                  if (srst)
                    add_stage[((0*ROW_A*COL_B*HALF_MODIFIED_COL_A)+((((i*COL_B)+j)*HALF_MODIFIED_COL_A)+k))*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] <= '0; 
                  else if (valid_out_reg[0])
                    add_stage[((0*ROW_A*COL_B*HALF_MODIFIED_COL_A)+((((i*COL_B)+j)*HALF_MODIFIED_COL_A)+k))*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] <= mult_stage[((((i*COL_B)+j)*MODIFIED_COL_A)+(2*k))*(INPUT_DATA_WIDTH * 2)+:(INPUT_DATA_WIDTH * 2)] + mult_stage[((((i*COL_B)+j)*MODIFIED_COL_A)+((2*k)+1))*(INPUT_DATA_WIDTH * 2)+:(INPUT_DATA_WIDTH * 2)];
                end
                else begin
                  if (srst)
                    add_stage[((l*ROW_A*COL_B*HALF_MODIFIED_COL_A)+((((i*COL_B)+j)*HALF_MODIFIED_COL_A)+k))*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] <= '0; 
                  else if ((HALF_MODIFIED_COL_A > 1) && (k < (HALF_MODIFIED_COL_A/2)))
                    add_stage[((l*ROW_A*COL_B*HALF_MODIFIED_COL_A)+((((i*COL_B)+j)*HALF_MODIFIED_COL_A)+k))*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] <= add_stage[(((l-1)*ROW_A*COL_B*HALF_MODIFIED_COL_A)+((((i*COL_B)+j)*HALF_MODIFIED_COL_A)+(2*k)))*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] + add_stage[(((l-1)*ROW_A*COL_B*HALF_MODIFIED_COL_A)+((((i*COL_B)+j)*HALF_MODIFIED_COL_A)+((2*k)+1)))*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH];
                end
              end
            end
          end
        end

      always_ff @(posedge clk)
        for (int i = 0 ; i < ROW_A ; i++) begin: out_row_a_gb
          for (int j = 0 ; j < COL_B ; j++) begin: out_col_b_gb
            for (int k = 0 ; k < MODIFIED_COL_A ; k++) begin: out_add_gb
              if (srst)
                matrix_c[((i*COL_B)+j)*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] <= '0;
              else if (valid_out_reg[$clog2(COL_A)])
                matrix_c[((i*COL_B)+j)*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] <= add_stage[((($clog2(COL_A)-1)*ROW_A*COL_B*HALF_MODIFIED_COL_A)+((((i*COL_B)+j)*HALF_MODIFIED_COL_A)+0))*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH]; 
            end
          end
        end
    end
    else begin
      always_ff @(posedge clk)
        for (int i = 0 ; i < ROW_A ; i++) begin: out_row_a_gb
          for (int j = 0 ; j < COL_B ; j++) begin: out_col_b_gb
            for (int k = 0 ; k < MODIFIED_COL_A ; k++) begin: out_mult_gb
              if (srst)
                matrix_c[((i*COL_B)+j)*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] <= '0;
              else if (valid_out_reg[$clog2(COL_A)])
                matrix_c[((i*COL_B)+j)*OUTPUT_DATA_WIDTH+:OUTPUT_DATA_WIDTH] <= mult_stage[((((i*COL_B)+j)*MODIFIED_COL_A)+0)*(INPUT_DATA_WIDTH * 2)+:(INPUT_DATA_WIDTH * 2)]; 
            end
          end
        end
    end

  endgenerate

endmodule