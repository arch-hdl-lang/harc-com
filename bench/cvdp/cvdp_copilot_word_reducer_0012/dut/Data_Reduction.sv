`timescale 1ns / 1ps

module Bit_Difference_Counter
#(
    parameter BIT_WIDTH    = 3,                          // Defines the width of the input vectors.
    localparam COUNT_WIDTH = $clog2(BIT_WIDTH + 1)       // Calculates the width required to represent the count of differing bits.
)
(
    input  wire [BIT_WIDTH-1:0] input_A,                // First input vector.
    input  wire [BIT_WIDTH-1:0] input_B,                // Second input vector.
    output reg  [COUNT_WIDTH-1:0] bit_difference_count  // Count of differing bits (Hamming distance).
);

    wire [BIT_WIDTH-1:0] different_bits;
    integer idx;

    // Instantiate the Data_Reduction module to compute bitwise XOR between input_A and input_B.
    Data_Reduction
    #(
        .REDUCTION_OP (3'b010), // XOR operation
        .DATA_WIDTH  (BIT_WIDTH),
        .DATA_COUNT  (2)
    )
    compare_bits
    (
        .data_in      ({input_A, input_B}),
        .reduced_data_out   (different_bits)
    );

    // Count set bits in different_bits to compute Hamming distance
    always @(*) begin
        bit_difference_count = 0;
        for (idx = 0; idx < BIT_WIDTH; idx = idx + 1) begin
            bit_difference_count = bit_difference_count + different_bits[idx];
        end
    end

endmodule

module Data_Reduction
#(
    parameter [2:0] REDUCTION_OP = 3'b000, // Default operation: AND
    parameter DATA_WIDTH         = 4,      // Width of each data element
    parameter DATA_COUNT         = 4,      // Number of data elements
    localparam TOTAL_INPUT_WIDTH = DATA_WIDTH * DATA_COUNT
)
(
    input  wire [TOTAL_INPUT_WIDTH-1:0] data_in,
    output reg  [DATA_WIDTH-1:0]        reduced_data_out
);

    generate
        genvar bit_index;

        for (bit_index = 0; bit_index < DATA_WIDTH; bit_index = bit_index + 1) begin : bit_processing
            wire [DATA_COUNT-1:0] extracted_bits;

            genvar data_index;
            for (data_index = 0; data_index < DATA_COUNT; data_index = data_index + 1) begin : bit_extraction
                assign extracted_bits[data_index] = data_in[(data_index * DATA_WIDTH) + bit_index];
            end

            Bitwise_Reduction
            #(
                .REDUCTION_OP (REDUCTION_OP),
                .BIT_COUNT    (DATA_COUNT)
            )
            reducer_instance
            (
                .input_bits  (extracted_bits),
                .reduced_bit (reduced_data_out[bit_index])
            );
        end
    endgenerate

endmodule


module Bitwise_Reduction
#(
    parameter [2:0] REDUCTION_OP = 3'b000, // Default operation: AND
    parameter BIT_COUNT          = 4       // Number of bits to reduce
)
(
    input  wire [BIT_COUNT-1:0] input_bits,
    output reg                  reduced_bit
);

    // Reduction Operation Codes
    localparam [2:0] AND_OP  = 3'b000;
    localparam [2:0] OR_OP   = 3'b001;
    localparam [2:0] XOR_OP  = 3'b010;
    localparam [2:0] NAND_OP = 3'b011;
    localparam [2:0] NOR_OP  = 3'b100;
    localparam [2:0] XNOR_OP = 3'b101;

    int i;
    reg temp_result; 

    always @(*) begin
        temp_result = input_bits[0];

        for (i = 1; i < BIT_COUNT; i = i + 1) begin
            case (REDUCTION_OP)
                AND_OP, NAND_OP  : temp_result = temp_result & input_bits[i];
                OR_OP,  NOR_OP   : temp_result = temp_result | input_bits[i];
                XOR_OP, XNOR_OP  : temp_result = temp_result ^ input_bits[i];
                default          : temp_result = temp_result & input_bits[i];
            endcase
        end

        case (REDUCTION_OP)
            NAND_OP : reduced_bit = ~temp_result;
            NOR_OP  : reduced_bit = ~temp_result;
            XNOR_OP : reduced_bit = ~temp_result;
            default : reduced_bit = temp_result;
        endcase
    end
endmodule