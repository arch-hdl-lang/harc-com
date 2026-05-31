// Vendored DUT — LZ4 block decompressor
// Source: arch-com/examples/lz4_decomp/Lz4Decomp.arch
//
// Implements a streaming LZ4 block decompressor with:
//   - AXI-Stream-like input  (s_valid/s_data/s_last/s_ready)
//   - AXI-Stream-like output (m_valid/m_data/m_last/m_ready)
//   - Latency-0 async-read history RAM for back-reference copy
//   - done pulses for one cycle when the block ends

`timescale 1ns/1ps
module Lz4Decomp #(
  parameter int HIST_BITS  = 12,
  parameter int HIST_DEPTH = 4096
) (
  input  logic       clk,
  input  logic       rst,
  input  logic       s_valid,
  input  logic [7:0] s_data,
  input  logic       s_last,
  output logic       s_ready,
  output logic       m_valid,
  output logic [7:0] m_data,
  output logic       m_last,
  input  logic       m_ready,
  output logic       done
);

  // ── History RAM (latency-0 async read, clocked write) ──────────────
  logic [7:0]           hist_ram   [0:HIST_DEPTH-1];
  logic                 hist_wr_en;
  logic [HIST_BITS-1:0] hist_wr_addr;
  logic [7:0]           hist_wr_data;
  logic [HIST_BITS-1:0] hist_rd_addr;
  logic [7:0]           hist_rd_data;

  assign hist_rd_data = hist_ram[hist_rd_addr];

  always_ff @(posedge clk)
    if (hist_wr_en) hist_ram[hist_wr_addr] <= hist_wr_data;

  // ── FSM state encoding ─────────────────────────────────────────────
  typedef enum logic [3:0] {
    S_TOKEN      = 4'd0,
    S_LIT_EXT    = 4'd1,
    S_LIT_IN     = 4'd2,
    S_LIT_OUT    = 4'd3,
    S_MATCH_OFF0 = 4'd4,
    S_MATCH_OFF1 = 4'd5,
    S_MATCH_EXT  = 4'd6,
    S_MATCH_OUT  = 4'd7,
    S_DONE       = 4'd8
  } state_t;

  state_t               state;
  logic [17:0]          lit_len;
  logic [17:0]          match_len;
  logic [15:0]          match_off;
  logic [3:0]           match_nibble;
  logic [7:0]           lit_byte;
  logic [HIST_BITS-1:0] wr_ptr;
  logic                 last_byte;
  logic                 done_r;

  assign done   = done_r;
  assign m_last = 1'b0;

  // ── Combinational output driver ────────────────────────────────────
  always_comb begin
    s_ready      = 1'b0;
    m_valid      = 1'b0;
    m_data       = 8'h00;
    hist_wr_en   = 1'b0;
    hist_wr_addr = '0;
    hist_wr_data = 8'h00;
    hist_rd_addr = '0;

    case (state)
      S_TOKEN,
      S_LIT_EXT,
      S_LIT_IN,
      S_MATCH_OFF0,
      S_MATCH_OFF1,
      S_MATCH_EXT:  s_ready = 1'b1;

      S_LIT_OUT: begin
        m_valid      = 1'b1;
        m_data       = lit_byte;
        hist_wr_en   = m_ready;
        hist_wr_addr = wr_ptr;
        hist_wr_data = lit_byte;
      end

      S_MATCH_OUT: begin
        hist_rd_addr = wr_ptr - match_off[HIST_BITS-1:0];
        m_valid      = 1'b1;
        m_data       = hist_rd_data;
        hist_wr_en   = m_ready;
        hist_wr_addr = wr_ptr;
        hist_wr_data = hist_rd_data;
      end

      default: begin end
    endcase
  end

  // ── Sequential state machine ───────────────────────────────────────
  always_ff @(posedge clk) begin
    if (rst) begin
      state        <= S_TOKEN;
      lit_len      <= '0;
      match_len    <= '0;
      match_off    <= '0;
      match_nibble <= '0;
      lit_byte     <= '0;
      wr_ptr       <= '0;
      last_byte    <= 1'b0;
      done_r       <= 1'b0;
    end else begin
      done_r <= 1'b0;

      case (state)

        // ── Read token byte: upper nibble = lit_len, lower = match extra ──
        S_TOKEN: begin
          if (s_valid) begin
            lit_len      <= {14'b0, s_data[7:4]};
            match_nibble <= s_data[3:0];
            match_len    <= {14'b0, s_data[3:0]} + 18'd4;
            last_byte    <= s_last;
            if (s_data[7:4] == 4'hF)
              state <= S_LIT_EXT;
            else if (s_data[7:4] != 4'h0)
              state <= S_LIT_IN;
            else if (!s_last)
              state <= S_MATCH_OFF0;
            else begin
              done_r <= 1'b1;
              state  <= S_DONE;
            end
          end
        end

        // ── Accumulate literal-length extension bytes (each 0xFF adds 255) ──
        S_LIT_EXT: begin
          if (s_valid) begin
            lit_len   <= lit_len + {10'b0, s_data};
            last_byte <= s_last;
            if (s_data != 8'hFF)
              state <= S_LIT_IN;
          end
        end

        // ── Latch one literal byte, then output it ──
        S_LIT_IN: begin
          if (s_valid) begin
            lit_byte  <= s_data;
            last_byte <= s_last;
            state     <= S_LIT_OUT;
          end
        end

        S_LIT_OUT: begin
          if (m_ready) begin
            wr_ptr  <= wr_ptr + 1'b1;
            lit_len <= lit_len - 1'b1;
            if (lit_len == 18'd1) begin
              if (!last_byte)
                state <= S_MATCH_OFF0;
              else begin
                done_r <= 1'b1;
                state  <= S_DONE;
              end
            end else
              state <= S_LIT_IN;
          end
        end

        // ── Read two-byte little-endian match offset ──
        S_MATCH_OFF0: begin
          if (s_valid) begin
            match_off <= {8'h00, s_data};
            last_byte <= s_last;
            state     <= S_MATCH_OFF1;
          end
        end

        S_MATCH_OFF1: begin
          if (s_valid) begin
            match_off <= {s_data, match_off[7:0]};
            last_byte <= s_last;
            state     <= (match_nibble == 4'hF) ? S_MATCH_EXT : S_MATCH_OUT;
          end
        end

        // ── Accumulate match-length extension bytes ──
        S_MATCH_EXT: begin
          if (s_valid) begin
            match_len <= match_len + {10'b0, s_data};
            last_byte <= s_last;
            if (s_data != 8'hFF)
              state <= S_MATCH_OUT;
          end
        end

        // ── Copy match_len bytes from history buffer ──
        S_MATCH_OUT: begin
          if (m_ready) begin
            wr_ptr    <= wr_ptr + 1'b1;
            match_len <= match_len - 1'b1;
            if (match_len == 18'd1)
              state <= S_TOKEN;
          end
        end

        S_DONE: begin
          // terminal — stays until external reset
        end

        default: state <= S_TOKEN;
      endcase
    end
  end

endmodule
