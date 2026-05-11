module pseudoRandGenerator_ca (
    input  logic       clock,    // Clock input
    input  logic       reset,    // Active-high synchronous Reset
    input  logic [15:0] CA_seed,  // 16-bit Cellular Automata seed
    output logic [15:0] CA_out    // 16-bit Cellular Automata output
);

    logic q1, q2, q3, q4, q5, q6, q7, q8;
    logic q9, q10, q11, q12, q13, q14, q15, q16;

    assign q1  = CA_out[14];                             
    assign q2  = CA_out[15] ^ CA_out[13];                
    assign q3  = CA_out[14] ^ CA_out[13] ^ CA_out[12];   
    assign q4  = CA_out[13] ^ CA_out[11];                
    assign q5  = CA_out[12] ^ CA_out[11] ^ CA_out[10];   
    assign q6  = CA_out[11] ^ CA_out[9];                 
    assign q7  = CA_out[10] ^ CA_out[9] ^ CA_out[8];     
    assign q8  = CA_out[9] ^ CA_out[7];                  
    assign q9  = CA_out[8] ^ CA_out[7] ^ CA_out[6];      
    assign q10 = CA_out[7] ^ CA_out[5];                  
    assign q11 = CA_out[6] ^ CA_out[5] ^ CA_out[4];      
    assign q12 = CA_out[5] ^ CA_out[3];                  
    assign q13 = CA_out[4] ^ CA_out[3] ^ CA_out[2];      
    assign q14 = CA_out[3] ^ CA_out[1];                  
    assign q15 = CA_out[2] ^ CA_out[1] ^ CA_out[0];      
    assign q16 = CA_out[1];                              


    always_ff @(posedge clock) begin
        if (reset) begin
            CA_out <= CA_seed;
        end else begin
            CA_out[15] <= q1;
            CA_out[14] <= q2;
            CA_out[13] <= q3;
            CA_out[12] <= q4;
            CA_out[11] <= q5;
            CA_out[10] <= q6;
            CA_out[9]  <= q7;
            CA_out[8]  <= q8;
            CA_out[7]  <= q9;
            CA_out[6]  <= q10;
            CA_out[5]  <= q11;
            CA_out[4]  <= q12;
            CA_out[3]  <= q13;
            CA_out[2]  <= q14;
            CA_out[1]  <= q15;
            CA_out[0]  <= q16;
        end
    end

endmodule