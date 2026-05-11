module cvdp_copilot_decode_firstbit #(
    parameter integer InWidth_g = 32,
    parameter InReg_g = 1,
    parameter OutReg_g = 1,
    parameter integer PlRegs_g = 1,
    parameter OutputFormat_g = 0  // 0: Binary, 1: One-Hot Encoding
)(
    input wire Clk,
    input wire Rst,
    input wire [InWidth_g-1:0] In_Data,
    input wire In_Valid,

    output reg [InWidth_g-1:0] Out_FirstBit,   // Binary index or one-hot bit
    output reg Out_Found,
    output reg Out_Valid
);

    // Local parameters
    localparam integer BinBits_c = $clog2(InWidth_g);

    // Internal signals
    reg [InWidth_g-1:0] In_Data_r;
    reg In_Valid_r;
    reg [PlRegs_g:0] Valid_pipeline;
    reg [PlRegs_g:0] Found_pipeline;
    reg [BinBits_c-1:0] FirstBit_pipeline [PlRegs_g:0];
    reg [InWidth_g-1:0] OneHotBit_pipeline [PlRegs_g:0];

    // Optional input register
    generate
    if (InReg_g == 1) begin : input_reg_block
        always @(posedge Clk or posedge Rst) begin
            if (Rst) begin
                In_Data_r <= {InWidth_g{1'b0}};
                In_Valid_r <= 1'b0;
            end else begin
                In_Data_r <= In_Data;
                In_Valid_r <= In_Valid;
            end
        end
    end else begin : no_input_reg_block
        always @(*) begin
            In_Data_r = In_Data;
            In_Valid_r = In_Valid;
        end
    end
    endgenerate

    // Function to find the index of the first '1' bit from LSB to MSB
    function [BinBits_c-1:0] find_first_one(input [InWidth_g-1:0] data_in);
        integer i;
        reg found;
        begin
            find_first_one = {BinBits_c{1'b0}};
            found = 0;
            for (i = 0; i < InWidth_g; i = i + 1) begin
                if (!found && data_in[i]) begin
                    find_first_one = i[BinBits_c-1:0];
                    found = 1;
                end
            end
        end
    endfunction

    // Function to generate one-hot encoding of the first set bit
    function [InWidth_g-1:0] one_hot_encode(input [BinBits_c-1:0] binary_index);
        begin
            one_hot_encode = {InWidth_g{1'b0}};
            one_hot_encode[binary_index] = 1'b1;
        end
    endfunction

    // Stage 0: Compute the first '1' bit position and its one-hot encoding
    always @(posedge Clk or posedge Rst) begin
        if (Rst) begin
            Valid_pipeline[0] <= 1'b0;
            Found_pipeline[0] <= 1'b0;
            FirstBit_pipeline[0] <= {BinBits_c{1'b0}};
            OneHotBit_pipeline[0] <= {InWidth_g{1'b0}};
        end else begin
            Valid_pipeline[0] <= In_Valid_r;
            Found_pipeline[0] <= |In_Data_r;
            if (|In_Data_r) begin
                FirstBit_pipeline[0] <= find_first_one(In_Data_r);
                OneHotBit_pipeline[0] <= one_hot_encode(find_first_one(In_Data_r));
            end else begin
                FirstBit_pipeline[0] <= {BinBits_c{1'b0}};
                OneHotBit_pipeline[0] <= {InWidth_g{1'b0}};
            end
        end
    end

    // Pipeline stages
    genvar k;
    generate
        for (k = 1; k <= PlRegs_g; k = k + 1) begin : pipeline_stages
            always @(posedge Clk or posedge Rst) begin
                if (Rst) begin
                    Valid_pipeline[k] <= 1'b0;
                    Found_pipeline[k] <= 1'b0;
                    FirstBit_pipeline[k] <= {BinBits_c{1'b0}};
                    OneHotBit_pipeline[k] <= {InWidth_g{1'b0}};
                end else begin
                    Valid_pipeline[k] <= Valid_pipeline[k-1];
                    Found_pipeline[k] <= Found_pipeline[k-1];
                    FirstBit_pipeline[k] <= FirstBit_pipeline[k-1];
                    OneHotBit_pipeline[k] <= OneHotBit_pipeline[k-1];
                end
            end
        end
    endgenerate

    // Optional output register
    generate
    if (OutReg_g == 1) begin : output_reg_block
        always @(posedge Clk or posedge Rst) begin
            if (Rst) begin
                Out_Valid <= 1'b0;
                Out_Found <= 1'b0;
                Out_FirstBit <= {InWidth_g{1'b0}};
            end else begin
                Out_Valid <= Valid_pipeline[PlRegs_g];
                Out_Found <= Found_pipeline[PlRegs_g];
                if (OutputFormat_g == 0) begin
                    // Binary encoding: Zero-extend to match width
                    Out_FirstBit <= {{(InWidth_g - BinBits_c){1'b0}}, FirstBit_pipeline[PlRegs_g]};
                end else begin
                    // One-hot encoding
                    Out_FirstBit <= OneHotBit_pipeline[PlRegs_g];
                end
            end
        end
    end else begin : no_output_reg_block
        always @(*) begin
            Out_Valid = Valid_pipeline[PlRegs_g];
            Out_Found = Found_pipeline[PlRegs_g];
            if (OutputFormat_g == 0) begin
                // Binary encoding: Zero-extend to match width
                Out_FirstBit = {{(InWidth_g - BinBits_c){1'b0}}, FirstBit_pipeline[PlRegs_g]};
            end else begin
                // One-hot encoding
                Out_FirstBit = OneHotBit_pipeline[PlRegs_g];
            end
        end
    end
    endgenerate

endmodule