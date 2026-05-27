// SHA-256 compression function, single 512-bit block, fixed IV.
// Ports use packed 2D arrays matching arch's Vec<UInt<32>,N> convention:
//   msg    : [15:0][31:0]  (element 0 at bits 31:0)
//   digest : [7:0][31:0]   (element 0 at bits 31:0)

module Sha256 (
    input  logic              clk,
    input  logic              rst,
    input  logic              start,
    input  logic [15:0][31:0] msg,
    output logic              done,
    output logic [7:0][31:0]  digest
);

    // ----------------------------------------------------------------
    // SHA-256 combinational helpers
    // ----------------------------------------------------------------
    function automatic logic [31:0] rotr (
        input logic [31:0] x,
        input integer       n
    );
        return (x >> n) | (x << (32 - n));
    endfunction

    function automatic logic [31:0] sigma0 (input logic [31:0] x);
        return rotr(x, 7) ^ rotr(x, 18) ^ (x >> 3);
    endfunction

    function automatic logic [31:0] sigma1 (input logic [31:0] x);
        return rotr(x, 17) ^ rotr(x, 19) ^ (x >> 10);
    endfunction

    function automatic logic [31:0] bsigma0 (input logic [31:0] x);
        return rotr(x, 2) ^ rotr(x, 13) ^ rotr(x, 22);
    endfunction

    function automatic logic [31:0] bsigma1 (input logic [31:0] x);
        return rotr(x, 6) ^ rotr(x, 11) ^ rotr(x, 25);
    endfunction

    function automatic logic [31:0] ch (
        input logic [31:0] x, y, z
    );
        return (x & y) ^ (~x & z);
    endfunction

    function automatic logic [31:0] maj (
        input logic [31:0] x, y, z
    );
        return (x & y) ^ (x & z) ^ (y & z);
    endfunction

    // ----------------------------------------------------------------
    // Round constants (FIPS 180-4)
    // ----------------------------------------------------------------
    logic [31:0] K [0:63];
    initial begin
        K[ 0] = 32'h428a2f98; K[ 1] = 32'h71374491;
        K[ 2] = 32'hb5c0fbcf; K[ 3] = 32'he9b5dba5;
        K[ 4] = 32'h3956c25b; K[ 5] = 32'h59f111f1;
        K[ 6] = 32'h923f82a4; K[ 7] = 32'hab1c5ed5;
        K[ 8] = 32'hd807aa98; K[ 9] = 32'h12835b01;
        K[10] = 32'h243185be; K[11] = 32'h550c7dc3;
        K[12] = 32'h72be5d74; K[13] = 32'h80deb1fe;
        K[14] = 32'h9bdc06a7; K[15] = 32'hc19bf174;
        K[16] = 32'he49b69c1; K[17] = 32'hefbe4786;
        K[18] = 32'h0fc19dc6; K[19] = 32'h240ca1cc;
        K[20] = 32'h2de92c6f; K[21] = 32'h4a7484aa;
        K[22] = 32'h5cb0a9dc; K[23] = 32'h76f988da;
        K[24] = 32'h983e5152; K[25] = 32'ha831c66d;
        K[26] = 32'hb00327c8; K[27] = 32'hbf597fc7;
        K[28] = 32'hc6e00bf3; K[29] = 32'hd5a79147;
        K[30] = 32'h06ca6351; K[31] = 32'h14292967;
        K[32] = 32'h27b70a85; K[33] = 32'h2e1b2138;
        K[34] = 32'h4d2c6dfc; K[35] = 32'h53380d13;
        K[36] = 32'h650a7354; K[37] = 32'h766a0abb;
        K[38] = 32'h81c2c92e; K[39] = 32'h92722c85;
        K[40] = 32'ha2bfe8a1; K[41] = 32'ha81a664b;
        K[42] = 32'hc24b8b70; K[43] = 32'hc76c51a3;
        K[44] = 32'hd192e819; K[45] = 32'hd6990624;
        K[46] = 32'hf40e3585; K[47] = 32'h106aa070;
        K[48] = 32'h19a4c116; K[49] = 32'h1e376c08;
        K[50] = 32'h2748774c; K[51] = 32'h34b0bcb5;
        K[52] = 32'h391c0cb3; K[53] = 32'h4ed8aa4a;
        K[54] = 32'h5b9cca4f; K[55] = 32'h682e6ff3;
        K[56] = 32'h748f82ee; K[57] = 32'h78a5636f;
        K[58] = 32'h84c87814; K[59] = 32'h8cc70208;
        K[60] = 32'h90befffa; K[61] = 32'ha4506ceb;
        K[62] = 32'hbef9a3f7; K[63] = 32'hc67178f2;
    end

    // ----------------------------------------------------------------
    // State machine registers
    // ----------------------------------------------------------------
    localparam logic [1:0] IDLE = 2'b00;
    localparam logic [1:0] EXEC = 2'b01;
    localparam logic [1:0] DONE = 2'b10;

    logic [1:0]        state;
    logic [31:0]       ra, rb, rc, rd_r, re, rf, rg, rh;
    logic [15:0][31:0] w_circ;
    logic [5:0]        round;
    logic [7:0][31:0]  digest_r;

    // ----------------------------------------------------------------
    // Combinational logic
    // ----------------------------------------------------------------
    logic [3:0]  idx_m2, idx_m7, idx_m15, idx_m16;
    logic [31:0] w_m2, w_m7, w_m15, w_m16;
    logic [31:0] w_new, w_cur, k_cur;
    logic [31:0] t1, t2, next_a, next_e;

    always_comb begin
        // Circular buffer indices — 4-bit overflow gives natural mod-16
        idx_m16 = round[3:0];
        idx_m15 = round[3:0] + 4'd1;
        idx_m7  = round[3:0] + 4'd9;
        idx_m2  = round[3:0] + 4'd14;

        w_m16 = w_circ[idx_m16];
        w_m15 = w_circ[idx_m15];
        w_m7  = w_circ[idx_m7];
        w_m2  = w_circ[idx_m2];

        w_new  = sigma1(w_m2) + w_m7 + sigma0(w_m15) + w_m16;
        w_cur  = (round < 6'd16) ? w_m16 : w_new;
        k_cur  = K[round];

        t1     = rh + bsigma1(re) + ch(re, rf, rg) + k_cur + w_cur;
        t2     = bsigma0(ra) + maj(ra, rb, rc);
        next_a = t1 + t2;
        next_e = rd_r + t1;

        done   = (state == DONE);
        digest = digest_r;
    end

    // ----------------------------------------------------------------
    // Sequential logic
    // ----------------------------------------------------------------
    always_ff @(posedge clk) begin
        if (rst) begin
            state    <= IDLE;
            ra       <= '0; rb   <= '0; rc  <= '0; rd_r <= '0;
            re       <= '0; rf   <= '0; rg  <= '0; rh   <= '0;
            round    <= '0;
            digest_r <= '0;
            w_circ   <= '0;
        end else begin
            case (state)
                IDLE: begin
                    if (start) begin
                        ra     <= 32'h6a09e667;
                        rb     <= 32'hbb67ae85;
                        rc     <= 32'h3c6ef372;
                        rd_r   <= 32'ha54ff53a;
                        re     <= 32'h510e527f;
                        rf     <= 32'h9b05688c;
                        rg     <= 32'h1f83d9ab;
                        rh     <= 32'h5be0cd19;
                        w_circ <= msg;
                        round  <= '0;
                        state  <= EXEC;
                    end
                end
                EXEC: begin
                    ra   <= next_a;
                    rb   <= ra;
                    rc   <= rb;
                    rd_r <= rc;
                    re   <= next_e;
                    rf   <= re;
                    rg   <= rf;
                    rh   <= rg;
                    if (round >= 6'd16)
                        w_circ[idx_m16] <= w_new;
                    if (round == 6'd63) begin
                        digest_r[0] <= next_a + 32'h6a09e667;
                        digest_r[1] <= ra     + 32'hbb67ae85;
                        digest_r[2] <= rb     + 32'h3c6ef372;
                        digest_r[3] <= rc     + 32'ha54ff53a;
                        digest_r[4] <= next_e + 32'h510e527f;
                        digest_r[5] <= re     + 32'h9b05688c;
                        digest_r[6] <= rf     + 32'h1f83d9ab;
                        digest_r[7] <= rg     + 32'h5be0cd19;
                        state <= DONE;
                    end else begin
                        round <= round + 6'd1;
                    end
                end
                DONE: begin
                    state <= IDLE;
                end
                default: state <= IDLE;
            endcase
        end
    end

endmodule
