module _M_threads (
  input logic clk,
  input logic rst,
  input logic ack,
  input logic [15:0] burst,
  input logic doit,
  input logic go,
  output logic done
);

  logic [2:0] _t0_state = 0;
  logic [31:0] _t0_cnt = 0;
  logic [15:0] _t0_loop_cnt = 0;
  always_comb begin
    done = 0;
    if (_t0_state == 4) begin
      done = 1;
    end
  end
  always_ff @(posedge clk or negedge rst) begin
    if ((!rst)) begin
      _t0_state <= 0;
    end else begin
      if (_t0_state == 0) begin
        if (go) begin
          _t0_state <= 1;
        end
      end
      if (_t0_state == 1) begin
        if (doit) begin
          _t0_state <= 2;
        end
        if (!doit) begin
          _t0_state <= 5;
        end
      end
      if (_t0_state == 2) begin
        _t0_state <= 3;
      end
      if (_t0_state == 3) begin
        if (ack) begin
          _t0_state <= 4;
        end
      end
      if (_t0_state == 4) begin
        if (_t0_loop_cnt < 16'(burst - 1)) begin
          _t0_state <= 3;
        end
        if (_t0_loop_cnt >= 16'(burst - 1)) begin
          _t0_state <= 6;
        end
      end
      if (_t0_state == 5) begin
        if (_t0_cnt == 0) begin
          _t0_state <= 6;
        end
      end
      if (_t0_state == 6) begin
        if (_t0_cnt == 0) begin
          _t0_state <= 0;
        end
      end
    end
  end
  always_ff @(posedge clk) begin
    if (_t0_state == 2) begin
      _t0_loop_cnt <= 0;
    end
    if (_t0_state == 4) begin
      _t0_loop_cnt <= 16'(_t0_loop_cnt + 16'd1);
      _t0_cnt <= 32'(1 - 32'd1);
    end
    if (_t0_state == 5) begin
      if (_t0_cnt == 0) begin
        _t0_cnt <= 32'(1 - 32'd1);
      end
      _t0_cnt <= 32'(_t0_cnt - 32'd1);
    end
    if (_t0_state == 6) begin
      _t0_cnt <= 32'(_t0_cnt - 32'd1);
    end
  end

endmodule

module M (
  input logic clk,
  input logic rst,
  input logic go,
  input logic doit,
  input logic ack,
  input logic [15:0] burst,
  output logic done
);

  _M_threads _threads (
    .clk(clk),
    .rst(rst),
    .ack(ack),
    .burst(burst),
    .doit(doit),
    .go(go),
    .done(done)
  );

endmodule

