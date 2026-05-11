module prim_max_find #(
  parameter int NumSrc = 8,
  parameter int Width = 8,
  // Derived parameters
  localparam int SrcWidth = $clog2(NumSrc),
  localparam int NumLevels = $clog2(NumSrc),
  localparam int NumNodes = 2**(NumLevels+1)-1
) (
  input                         clk_i,
  input                         rst_ni,
  input [Width*NumSrc-1:0]      values_i,    // Flattened Input values
  input [NumSrc-1:0]            valid_i,     // Input valid bits
  output wire [Width-1:0]       max_value_o, // Maximum value
  output wire [SrcWidth-1:0]    max_idx_o,   // Index of the maximum value
  output wire                   max_valid_o  // Whether any of the inputs is valid
);

  reg [NumNodes-1:0]                vld_tree [0:NumLevels];
  reg [SrcWidth-1:0]                 idx_tree [0:NumLevels][NumNodes-1:0];
  reg [Width-1:0]                    max_tree [0:NumLevels][NumNodes-1:0];

  generate
    for (genvar level = 0; level <= NumLevels; level++) begin : gen_tree
      localparam int Base0 = (2**level)-1;
      localparam int Base1 = (2**(level+1))-1;

      for (genvar offset = 0; offset < 2**level; offset++) begin : gen_level
        localparam int Pa = Base0 + offset;
        localparam int C0 = Base1 + 2*offset;
        localparam int C1 = Base1 + 2*offset + 1;

        if (level == NumLevels) begin : gen_leafs
          if (offset < NumSrc) begin : gen_assign
            always @(posedge clk_i or negedge rst_ni) begin
              if (!rst_ni) begin
                vld_tree[level][Pa] <= 1'b0;
                idx_tree[level][Pa] <= '0;
                max_tree[level][Pa] <= '0;
              end else begin
                vld_tree[level][Pa] <= valid_i[offset];
                idx_tree[level][Pa] <= offset;
                max_tree[level][Pa] <= values_i[(offset+1)*Width-1 : offset*Width];
              end
            end
          end else begin : gen_tie_off
            always @(posedge clk_i or negedge rst_ni) begin
              if (!rst_ni) begin
                vld_tree[level][Pa] <= 1'b0;
                idx_tree[level][Pa] <= '0;
                max_tree[level][Pa] <= '0;
              end
            end
          end
        end

        else begin : gen_nodes
          reg sel; 
          always @(posedge clk_i or negedge rst_ni) begin
            if (!rst_ni) begin
              vld_tree[level][Pa] <= 1'b0;
              idx_tree[level][Pa] <= '0;
              max_tree[level][Pa] <= '0;
            end else begin
              sel = (~vld_tree[level+1][C0] & vld_tree[level+1][C1]) |
                    (vld_tree[level+1][C0] & vld_tree[level+1][C1] & (max_tree[level+1][C1] > max_tree[level+1][C0]));

              vld_tree[level][Pa] <= (sel) ? vld_tree[level+1][C1] : vld_tree[level+1][C0];
              idx_tree[level][Pa] <= (sel) ? idx_tree[level+1][C1] : idx_tree[level+1][C0];
              max_tree[level][Pa] <= (sel) ? max_tree[level+1][C1] : max_tree[level+1][C0];
            end
          end
        end
      end : gen_level
    end : gen_tree
  endgenerate

  assign max_valid_o = vld_tree[0][0];
  assign max_idx_o   = idx_tree[0][0];
  assign max_value_o = max_tree[0][0];

endmodule
