`timescale 1ns/1ps

module gf_multiplier_tb;
    reg  [3:0] A, B;
    wire [3:0] result;
    
    // Instantiate the gf_multiplier module
    gf_multiplier uut (
        .A(A),
        .B(B),
        .result(result)
    );
    
    initial begin
        // Optional: create a VCD file for waveform viewing
        $dumpfile("gf_multiplier_tb.vcd");
        $dumpvars(0, gf_multiplier_tb);
        
        // Testcase 1
        A = 4'h0; B = 4'h0;
        #5;
        $display("TC1  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 2
        A = 4'h0; B = 4'h1;
        #5;
        $display("TC2  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 3
        A = 4'h1; B = 4'h0;
        #5;
        $display("TC3  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 4
        A = 4'h1; B = 4'h1;
        #5;
        $display("TC4  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 5
        A = 4'h3; B = 4'h4;
        #5;
        $display("TC5  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 6
        A = 4'h7; B = 4'h9;
        #5;
        $display("TC6  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 7
        A = 4'ha; B = 4'h5;
        #5;
        $display("TC7  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 8
        A = 4'hf; B = 4'hf;
        #5;
        $display("TC8  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 9
        A = 4'hc; B = 4'h3;
        #5;
        $display("TC9  : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 10
        A = 4'h8; B = 4'h8;
        #5;
        $display("TC10 : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 11
        A = 4'hd; B = 4'h2;
        #5;
        $display("TC11 : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 12
        A = 4'h2; B = 4'hf;
        #5;
        $display("TC12 : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 13
        A = 4'h9; B = 4'h9;
        #5;
        $display("TC13 : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 14
        A = 4'h6; B = 4'ha;
        #5;
        $display("TC14 : A=%h, B=%h -> result=%h", A, B, result);

        // Testcase 15
        A = 4'h5; B = 4'h7;
        #5;
        $display("TC15 : A=%h, B=%h -> result=%h", A, B, result);

        // Finish simulation
        $finish;
    end
endmodule