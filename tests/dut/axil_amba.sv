// Tiny AXI4-Lite slave with AMBA-style port names (single-word
// `awvalid`/`awaddr`/etc. — no underscore between channel and
// signal). Exists specifically to exercise HARC's
// `bind ... with { ch.sig: "port" }` per-signal remap.
//
// Storage: one 32-bit register at address 0.

module axil_amba (
  input  logic        clk,
  input  logic        rst,

  // Write address channel (AMBA naming).
  input  logic        s_axi_awvalid,
  output logic        s_axi_awready,
  input  logic [7:0]  s_axi_awaddr,

  // Write data channel.
  input  logic        s_axi_wvalid,
  output logic        s_axi_wready,
  input  logic [31:0] s_axi_wdata,
  input  logic [3:0]  s_axi_wstrb,

  // Write response.
  output logic        s_axi_bvalid,
  input  logic        s_axi_bready,
  output logic [1:0]  s_axi_bresp,

  // Read address channel.
  input  logic        s_axi_arvalid,
  output logic        s_axi_arready,
  input  logic [7:0]  s_axi_araddr,

  // Read data channel.
  output logic        s_axi_rvalid,
  input  logic        s_axi_rready,
  output logic [31:0] s_axi_rdata,
  output logic [1:0]  s_axi_rresp,

  // Observable storage output (for the fixture to cross-check
  // without going through the bus).
  output logic [31:0] storage_out
);

  logic [31:0] reg0;
  assign storage_out = reg0;

  // Always-ready writes/reads (no backpressure modeling).
  assign s_axi_awready = 1'b1;
  assign s_axi_wready  = 1'b1;
  assign s_axi_arready = 1'b1;
  assign s_axi_bresp   = 2'b00;
  assign s_axi_rresp   = 2'b00;

  // Write side: single-cycle address+data accept, then a
  // single-cycle bvalid response.
  always_ff @(posedge clk) begin
    if (rst) begin
      reg0         <= 32'b0;
      s_axi_bvalid <= 1'b0;
    end else begin
      if (s_axi_awvalid && s_axi_wvalid && (s_axi_awaddr == 8'h00)) begin
        reg0         <= s_axi_wdata;
        s_axi_bvalid <= 1'b1;
      end else if (s_axi_bvalid && s_axi_bready) begin
        s_axi_bvalid <= 1'b0;
      end
    end
  end

  // Read side: single-cycle address accept, then a single-cycle
  // rvalid response with the stored value.
  always_ff @(posedge clk) begin
    if (rst) begin
      s_axi_rvalid <= 1'b0;
      s_axi_rdata  <= 32'b0;
    end else begin
      if (s_axi_arvalid && (s_axi_araddr == 8'h00)) begin
        s_axi_rvalid <= 1'b1;
        s_axi_rdata  <= reg0;
      end else if (s_axi_rvalid && s_axi_rready) begin
        s_axi_rvalid <= 1'b0;
      end
    end
  end

endmodule
