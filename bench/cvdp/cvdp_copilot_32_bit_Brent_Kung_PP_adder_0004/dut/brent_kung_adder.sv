module brent_kung_adder(
    input  logic [31:0] a,
    input  logic [31:0] b,
    input  logic carry_in,
    output logic [31:0] sum,
    output logic carry_out
);
    logic [31:0] P1, G1;
    logic [32:1] C;
    logic [15:0] G2, P2;
    logic [7:0] G3, P3;
    logic [3:0] G4, P4;
    logic [1:0] G5, P5;
    logic G6, P6;
    
    assign P1 = a ^ b;
    assign G1 = a & b;
    
    genvar i;
    generate
        for(i=0; i<=30; i=i+2) begin: second_stage  
            assign G2[i/2] = G1[i+1] | (P1[i+1] & G1[i]);
            assign P2[i/2] = P1[i+1] & P1[i];
        end
    endgenerate
    
    generate
        for(i=0; i<=14; i=i+2) begin: third_stage   
            assign G3[i/2] = G2[i+1] | (P2[i+1] & G2[i]);
            assign P3[i/2] = P2[i+1] & P2[i];
        end
    endgenerate
    
    generate
        for(i=0; i<=6; i=i+2) begin: fourth_stage  
            assign G4[i/2] = G3[i+1] | (P3[i+1] & G3[i]);
            assign P4[i/2] = P3[i+1] & P3[i];
        end
    endgenerate
    
    generate
        for(i=0; i<=2; i=i+2) begin: fifth_stage  
            assign G5[i/2] = G4[i+1] | (P4[i+1] & G4[i]);
            assign P5[i/2] = P4[i+1] & P4[i];
        end
    endgenerate
        
    assign G6 = G5[1] | (P5[1] & G5[0]);
    assign P6 = P5[1] & P5[0];
       
    assign C[1] = G1[0] | (P1[0] & carry_in);
    assign C[2] = G2[0] | (P2[0] & carry_in);
    assign C[4] = G3[0] | (P3[0] & carry_in);
    assign C[8] = G4[0] | (P4[0] & carry_in);
    assign C[16] = G5[0] | (P5[0] & carry_in);
    assign C[32] = G6 | (P6 & carry_in);
        
    assign C[3] = G1[2] | (P1[2] & C[2]);
    assign C[5] = G1[4] | (P1[4] & C[4]);
    assign C[6] = G2[2] | (P2[2] & C[4]);
    assign C[7] = G1[6] | (P1[6] & C[6]);
    
    assign C[9] = G1[8] | (P1[8] & C[8]);
    assign C[10] = G2[4] | (P2[4] & C[8]);
    assign C[11] = G1[10] | (P1[10] & C[10]);
    assign C[12] = G3[2] | (P3[2] & C[8]);
    assign C[13] = G1[12] | (P1[12] & C[12]);
    assign C[14] = G2[6] | (P2[6] & C[12]);
    assign C[15] = G1[14] | (P1[14] & C[14]);
    
    assign C[17] = G1[16] | (P1[16] & C[16]);
    assign C[18] = G2[8] | (P2[8] & C[16]);
    assign C[19] = G1[18] | (P1[18] & C[18]);
    assign C[20] = G3[4] | (P3[4] & C[16]);
    assign C[21] = G1[20] | (P1[20] & C[20]);
    assign C[22] = G2[10] | (P2[10] & C[20]);
    assign C[23] = G1[22] | (P1[22] & C[22]);
    assign C[24] = G4[2] | (P4[2] & C[16]);
    assign C[25] = G1[24] | (P1[24] & C[24]);
    assign C[26] = G2[12] | (P2[12] & C[24]);
    assign C[27] = G1[26] | (P1[26] & C[26]);
    assign C[28] = G3[6] | (P3[6] & C[24]);
    assign C[29] = G1[28] | (P1[28] & C[28]);
    assign C[30] = G2[14] | (P2[14] & C[28]);
    assign C[31] = G1[30] | (P1[30] & C[30]);
        
    assign sum = P1 ^ {C[31:1], carry_in};
    assign carry_out = C[32];
    
endmodule
