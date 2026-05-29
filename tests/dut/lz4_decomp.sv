// LZ4 block decompressor (subset) — modeled on CAST Inc.'s LZ4SNP-D IP.
//
// Hand-written SystemVerilog mirror of arch-com/examples/lz4_decomp.arch,
// used as the Verilator DUT for the HARC fixture lz4_decomp_test.harc.
//
// Parses LZ4 sequences (token nibbles, extended literal/match lengths,
// little-endian back-reference offsets) one byte per cycle into a 256-byte
// circular history/output buffer, then streams the decoded block out over
// a valid/ready interface. The final compressed byte is marked with
// in_last; the block ends after the trailing literal run.
//
// domain SysDomain
//   freq_mhz: 100
module Lz4Decomp (
  input  logic        clk,
  input  logic        rst,        // synchronous, active-high
  input  logic        in_valid,
  input  logic [7:0]  in_data,
  input  logic        in_last,
  output logic        in_ready,
  output logic        out_valid,
  output logic [7:0]  out_data,
  output logic        out_last,
  input  logic        out_ready,
  output logic        done
);

  typedef enum logic [3:0] {
    READ_TOKEN    = 4'd0,
    EXTRA_LIT_LEN = 4'd1,
    COPY_LITERAL  = 4'd2,
    READ_OFF_LO   = 4'd3,
    READ_OFF_HI   = 4'd4,
    EXTRA_MATCH   = 4'd5,
    COPY_MATCH    = 4'd6,
    EMIT          = 4'd7,
    DONE_ST       = 4'd8
  } state_t;

  state_t      state_r, state_n;
  logic [7:0]  hist [0:255];
  logic [7:0]  wr_ptr_r;   // bytes decoded so far this block
  logic [7:0]  em_ptr_r;   // emit pointer during EMIT
  logic [15:0] len_r;      // literal / match length countdown
  logic [7:0]  off_r;      // back-reference offset (low byte)
  logic [3:0]  ml_nib_r;   // match-length nibble from the token

  logic [7:0]  rd_ptr;
  assign rd_ptr = wr_ptr_r - off_r;   // circular back-reference source

  // ── Output decode ────────────────────────────────────────
  always_comb begin
    in_ready  = 1'b0;
    out_valid = 1'b0;
    out_data  = 8'h00;
    out_last  = 1'b0;
    done      = 1'b0;
    case (state_r)
      READ_TOKEN, EXTRA_LIT_LEN, COPY_LITERAL,
      READ_OFF_LO, READ_OFF_HI, EXTRA_MATCH:
        in_ready = 1'b1;
      EMIT: begin
        out_valid = 1'b1;
        out_data  = hist[em_ptr_r];
        out_last  = (em_ptr_r == (wr_ptr_r - 8'd1));
      end
      DONE_ST: done = 1'b1;
      default: ;
    endcase
  end

  // ── Next-state logic ────────────────────────────────────
  always_comb begin
    state_n = state_r;
    case (state_r)
      READ_TOKEN:
        if (in_valid) begin
          if (in_data[7:4] == 4'd15)     state_n = EXTRA_LIT_LEN;
          else if (in_data[7:4] != 4'd0) state_n = COPY_LITERAL;
          else                           state_n = READ_OFF_LO;
        end
      EXTRA_LIT_LEN:
        if (in_valid) state_n = (in_data == 8'd255) ? EXTRA_LIT_LEN : COPY_LITERAL;
      COPY_LITERAL:
        if (in_valid && (len_r == 16'd1))
          state_n = in_last ? EMIT : READ_OFF_LO;
      READ_OFF_LO:
        if (in_valid) state_n = READ_OFF_HI;
      READ_OFF_HI:
        if (in_valid) state_n = (ml_nib_r == 4'd15) ? EXTRA_MATCH : COPY_MATCH;
      EXTRA_MATCH:
        if (in_valid) state_n = (in_data == 8'd255) ? EXTRA_MATCH : COPY_MATCH;
      COPY_MATCH:
        if (len_r == 16'd1) state_n = READ_TOKEN;
      EMIT:
        if (out_ready && (em_ptr_r == (wr_ptr_r - 8'd1))) state_n = DONE_ST;
      DONE_ST:
        state_n = READ_TOKEN;
      default: state_n = READ_TOKEN;
    endcase
  end

  // ── Datapath ──────────────────────────────────────────
  always_ff @(posedge clk) begin
    if (rst) begin
      state_r  <= READ_TOKEN;
      wr_ptr_r <= 8'd0;
      em_ptr_r <= 8'd0;
      len_r    <= 16'd0;
      off_r    <= 8'd0;
      ml_nib_r <= 4'd0;
    end else begin
      state_r <= state_n;
      case (state_r)
        READ_TOKEN:
          if (in_valid) begin
            ml_nib_r <= in_data[3:0];
            if (in_data[7:4] == 4'd15) len_r <= 16'd15;
            else                       len_r <= {12'd0, in_data[7:4]};
          end
        EXTRA_LIT_LEN:
          if (in_valid) len_r <= len_r + {8'd0, in_data};
        COPY_LITERAL:
          if (in_valid) begin
            hist[wr_ptr_r] <= in_data;
            wr_ptr_r <= wr_ptr_r + 8'd1;
            len_r    <= len_r - 16'd1;
          end
        READ_OFF_LO:
          if (in_valid) off_r <= in_data;
        READ_OFF_HI:
          if (in_valid) begin
            if (ml_nib_r == 4'd15) len_r <= 16'd19;
            else                   len_r <= {12'd0, ml_nib_r} + 16'd4;
          end
        EXTRA_MATCH:
          if (in_valid) len_r <= len_r + {8'd0, in_data};
        COPY_MATCH: begin
          hist[wr_ptr_r] <= hist[rd_ptr];
          wr_ptr_r <= wr_ptr_r + 8'd1;
          len_r    <= len_r - 16'd1;
        end
        EMIT:
          if (out_ready) em_ptr_r <= em_ptr_r + 8'd1;
        DONE_ST: begin
          wr_ptr_r <= 8'd0;
          em_ptr_r <= 8'd0;
        end
        default: ;
      endcase
    end
  end

endmodule
