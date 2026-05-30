// Huffman literal encoder — fixed RFC 1951 DEFLATE codes.
//
// Symbols   0-143: 8-bit code = sym + 8'd48  (0x030 .. 0x0BF)
// Symbols 144-255: 9-bit code = sym + 9'd256 (0x190 .. 0x1FF)
//
// One clock cycle of pipeline latency. Back-pressure via m_ready stalls
// the pipeline register without dropping data.
//
// Equivalent to: arch build examples/huff_enc.arch

module HuffEnc (
    input  logic        clk,
    input  logic        rst,

    input  logic        s_valid,
    output logic        s_ready,
    input  logic [7:0]  s_data,
    input  logic        s_last,

    output logic        m_valid,
    input  logic        m_ready,
    output logic [8:0]  m_code,
    output logic [3:0]  m_len,
    output logic        m_last
);

    logic        valid_r;
    logic [8:0]  code_r;
    logic [3:0]  len_r;
    logic        last_r;

    logic        stall;
    logic [8:0]  code_w;
    logic [3:0]  len_w;

    // RFC 1951 fixed Huffman literal code lookup (combinational)
    assign code_w = (s_data < 8'd144) ? (9'(s_data) + 9'd48)
                                       : (9'(s_data) + 9'd256);
    assign len_w  = (s_data < 8'd144) ? 4'd8 : 4'd9;

    // AXI-Stream handshake
    assign stall   = valid_r & ~m_ready;
    assign s_ready = ~stall;
    assign m_valid = valid_r;
    assign m_code  = code_r;
    assign m_len   = len_r;
    assign m_last  = last_r;

    // Pipeline register
    always_ff @(posedge clk) begin
        if (rst) begin
            valid_r <= 1'b0;
            code_r  <= 9'b0;
            len_r   <= 4'b0;
            last_r  <= 1'b0;
        end else if (~stall) begin
            valid_r <= s_valid;
            if (s_valid) begin
                code_r <= code_w;
                len_r  <= len_w;
                last_r <= s_last;
            end
        end
    end

endmodule
