// LZ4 block decompressor — streaming byte-serial FSM with circular history.
//
// Interface (AXI-Stream-like, no TKEEP/TSTRB):
//   in_valid / in_data / in_last / in_ready   — compressed byte stream
//   out_valid / out_data / out_last / out_ready — decompressed byte stream
//
// in_last is asserted with the TOKEN byte of the LAST sequence in a block.
// The last sequence has literals only (no offset/match), per LZ4 spec §2.
//
// Timing / synthesis notes:
//   * All stream outputs (out_valid, out_data, out_last) are REGISTERED
//     (`pipe_reg<_,1>`): no combinational input pin -> output pin path.
//   * History RAM is `latency 1` (REGISTERED read) so it maps to a single
//     7-series block RAM (RAMB36E1) with a fast, fixed clock-to-out, instead
//     of a 4096-deep distributed-RAM + LUT mux tree. Proven via yosys
//     synth_xilinx: latency-0 = 192x RAM64M + 224x LUT6 (8 logic levels);
//     latency-1 = 1x RAMB36E1 (4 logic levels).
//
//   The registered read costs one cycle of read latency, which the COPY state
//   absorbs with a fill+emit pipeline:
//     - On entry, COPY spends one "fill" cycle issuing the first read address.
//     - Thereafter it emits one byte per cycle: present copy_src+1 while
//       emitting the byte read for copy_src.
//
//   Overlap handling (LZ4 allows offset < match_len, e.g. RLE offset=1):
//   when the read address being captured collides with the same-cycle write
//   address (copy_src == wr_ptr, which happens iff offset == 1), the block RAM
//   returns the stale (read-first) value. A 1-deep write->read forwarding
//   register (fwd_pend / fwd_data) bypasses the just-written byte into the
//   emit path. offset >= 2 needs no forwarding: the byte was committed at
//   least one cycle before the read is presented.
// domain SysDomain
//   freq_mhz: 100

module HistBuf #(
  parameter int DEPTH = 4096,
  parameter int DATA_WIDTH = 8
) (
  input logic clk,
  input logic rd_port_en,
  input logic [11:0] rd_port_addr,
  output logic [7:0] rd_port_data,
  input logic wr_port_en,
  input logic [11:0] wr_port_addr,
  input logic [7:0] wr_port_data
);

  logic [DATA_WIDTH-1:0] mem [0:DEPTH-1];
  logic [DATA_WIDTH-1:0] rd_port_data_r;
  
  always_ff @(posedge clk) begin
    if (wr_port_en)
      mem[wr_port_addr] <= wr_port_data;
    if (rd_port_en)
      rd_port_data_r <= mem[rd_port_addr];
  end
  assign rd_port_data = rd_port_data_r;

endmodule

typedef enum logic [2:0] {
  TOKEN = 3'd0,
  EXTRA_LL = 3'd1,
  LITERAL = 3'd2,
  OFFS_LO = 3'd3,
  OFFS_HI = 3'd4,
  EXTRA_ML = 3'd5,
  COPY = 3'd6
} LzState;

module Lz4Decomp (
  input logic clk,
  input logic rst,
  input logic in_valid,
  input logic [7:0] in_data,
  input logic in_last,
  output logic in_ready,
  input logic out_ready,
  output logic out_valid,
  output logic [7:0] out_data,
  output logic out_last
);

  logic [3:0] ll4;
  logic [3:0] ml4_v;
  logic [15:0] full_offset;
  logic can_emit;
  logic copy_step;
  logic produce_literal;
  logic produce_copy;
  logic [7:0] copy_byte;
  // Registered stream outputs — no combinational input->output path.
  LzState state;
  logic [15:0] lit_rem;
  logic [15:0] match_rem;
  logic [3:0] ml4;
  logic [7:0] offs_lo;
  logic [11:0] wr_ptr;
  logic [11:0] copy_src;
  logic is_last;
  // COPY read pipeline state.
  logic copy_primed;
  // hist_rd_data holds a valid copied byte this cycle
  logic fwd_pend;
  // forward fwd_data instead of hist_rd_data this cycle
  logic [7:0] fwd_data;
  // Upper nibble = literal length, lower = match-length prefix (from TOKEN).
  assign ll4 = in_data[7:4];
  assign ml4_v = in_data[3:0];
  // Offset reconstruction (only valid in OFFS_HI state).
  assign full_offset = 16'($unsigned(in_data)) << 8 | 16'($unsigned(offs_lo));
  // The output register slot is free when it is empty or being drained.
  assign can_emit = !out_valid || out_ready;
  // COPY advances its read pipeline one step per cycle while it can emit.
  assign copy_step = state == COPY && can_emit;
  // A decompressed byte is produced this cycle when its source state is active
  // and the output register can take it. Literals also require an input byte;
  // copies require the read pipeline to be primed.
  assign produce_literal = state == LITERAL && in_valid && can_emit;
  assign produce_copy = state == COPY && copy_primed && can_emit;
  logic [7:0] hist_rd_data;
  logic hist_rd_en;
  logic hist_wr_en;
  logic [7:0] hist_wr_data;
  // Byte emitted by COPY: forwarded write data on an offset-1 collision,
  // otherwise the registered block-RAM output.
  assign copy_byte = fwd_pend ? fwd_data : hist_rd_data;
  HistBuf hist (
    .clk(clk),
    .rd_port_en(hist_rd_en),
    .rd_port_addr(copy_src),
    .rd_port_data(hist_rd_data),
    .wr_port_en(hist_wr_en),
    .wr_port_addr(wr_ptr),
    .wr_port_data(hist_wr_data)
  );
  always_comb begin
    in_ready = 0;
    hist_rd_en = 0;
    hist_wr_en = 0;
    hist_wr_data = 0;
    if (state == LITERAL) begin
      // Accept a literal byte only when we can emit it this cycle.
      in_ready = can_emit;
      hist_wr_en = produce_literal;
      hist_wr_data = in_data;
    end else if (state == COPY) begin
      // Issue a read every step; write back the emitted byte.
      hist_rd_en = copy_step;
      hist_wr_en = produce_copy;
      hist_wr_data = copy_byte;
    end else begin
      // TOKEN, EXTRA_LL, OFFS_LO, OFFS_HI, EXTRA_ML: consume input byte.
      in_ready = 1;
    end
  end
  always_ff @(posedge clk) begin
    if (rst) begin
      copy_primed <= 0;
      copy_src <= 0;
      fwd_data <= 0;
      fwd_pend <= 0;
      is_last <= 0;
      lit_rem <= 0;
      match_rem <= 0;
      ml4 <= 0;
      offs_lo <= 0;
      out_data <= 0;
      out_last <= 0;
      out_valid <= 0;
      state <= TOKEN;
      wr_ptr <= 0;
    end else begin
      // ---- Registered output / handshake ----
      if (produce_literal) begin
        out_data <= in_data;
        out_valid <= 1;
        out_last <= is_last && lit_rem == 1;
      end else if (produce_copy) begin
        out_data <= copy_byte;
        out_valid <= 1;
        // A copy is never the block's final byte: the last LZ4 sequence is
        // literals-only, so out_last is asserted only on a final literal.
        out_last <= 0;
      end else if (out_ready) begin
        // Slot drained, nothing new produced.
        out_valid <= 0;
        out_last <= 0;
      end
      // ---- FSM ----
      if (state == TOKEN) begin
        if (in_valid) begin
          ml4 <= ml4_v;
          is_last <= in_last;
          if (ll4 == 15) begin
            lit_rem <= 15;
          end else begin
            lit_rem <= 16'($unsigned(ll4));
          end
          if (ml4_v == 15) begin
            match_rem <= 15;
          end else begin
            match_rem <= 16'(16'($unsigned(ml4_v)) + 4);
          end
          // Determine next state.
          if (ll4 == 15) begin
            state <= EXTRA_LL;
          end else if (ll4 != 0) begin
            state <= LITERAL;
          end else if (in_last) begin
            // Last sequence with zero literals: block done; stay idle.
            state <= TOKEN;
          end else begin
            state <= OFFS_LO;
          end
        end
      end else if (state == EXTRA_LL) begin
        if (in_valid) begin
          lit_rem <= 16'(lit_rem + 16'($unsigned(in_data)));
          if (in_data < 255) begin
            state <= LITERAL;
          end
        end
      end else if (state == LITERAL) begin
        if (produce_literal) begin
          wr_ptr <= (12 > 1 ? 12 : 1)'(wr_ptr + 1);
          lit_rem <= 16'(lit_rem - 1);
          if (lit_rem == 1) begin
            if (is_last) begin
              state <= TOKEN;
            end else begin
              state <= OFFS_LO;
            end
          end
        end
      end else if (state == OFFS_LO) begin
        if (in_valid) begin
          offs_lo <= in_data;
          state <= OFFS_HI;
        end
      end else if (state == OFFS_HI) begin
        if (in_valid) begin
          copy_src <= 12'(16'(16'($unsigned(wr_ptr)) - full_offset));
          copy_primed <= 0;
          // first COPY cycle is a read-address fill
          fwd_pend <= 0;
          if (ml4 == 15) begin
            state <= EXTRA_ML;
          end else begin
            state <= COPY;
          end
        end
      end else if (state == EXTRA_ML) begin
        if (in_valid) begin
          if (in_data == 255) begin
            match_rem <= 16'(match_rem + 255);
          end else begin
            match_rem <= 16'(match_rem + 16'($unsigned(in_data)) + 4);
            copy_primed <= 0;
            fwd_pend <= 0;
            state <= COPY;
          end
        end
      end else if (state == COPY) begin
        if (copy_step) begin
          // Advance the read pointer every step (fill or emit).
          copy_src <= (12 > 1 ? 12 : 1)'(copy_src + 1);
          copy_primed <= 1;
          if (copy_primed) begin
            // Emit cycle: write the byte back, update forwarding, count down.
            wr_ptr <= (12 > 1 ? 12 : 1)'(wr_ptr + 1);
            match_rem <= 16'(match_rem - 1);
            // Same-cycle read/write collision (offset == 1) → forward next cycle.
            fwd_pend <= copy_src == wr_ptr;
            fwd_data <= copy_byte;
            if (match_rem == 1) begin
              state <= TOKEN;
            end
          end
        end
      end
    end
  end

endmodule

