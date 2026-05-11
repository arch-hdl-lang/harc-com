`timescale 1ns/1ps
module image_stego #(
  parameter row = 2,
  parameter col = 2,
  parameter EMBED_COUNT_WIDTH = 3
)(
  input [(row*col*8)-1:0] img_in,
  input [(row*col*4)-1:0] data_in,
  input [2:0] bpp,
  input [7:0] encryption_key,
  input [(row*col)-1:0] mask,
  output reg [(row*col*8)-1:0] img_out,
  output reg error_out,
  output reg [EMBED_COUNT_WIDTH-1:0] embedded_pixel_count,
  output reg [1:0] embedding_done
);

integer p;
reg parity;
always @(*) begin
  parity = 0;
  for (p = 0; p < (row*col*4); p = p + 1) begin
    parity = parity ^ data_in[p];
  end
  error_out = (parity == 1);
end

reg [(row*col*4)-1:0] data_in_encrypted;
integer block;
always @(*) begin
  data_in_encrypted = 0;
  for (block = 0; block < (row*col*4); block = block + 8) begin
    data_in_encrypted[block +: 8] = data_in[block +: 8] ^ encryption_key;
  end
end

integer i;
always @(*) begin
  img_out = img_in;
  embedded_pixel_count = 0;
  embedding_done = 2'b00;
  if (mask != 0) begin
    embedding_done = 2'b01;
    for (i = 0; i < (row*col); i = i + 1) begin
      if (mask[i]) begin
        case (bpp)
          3'b000: img_out[i*8 +: 8] = {img_in[i*8 + 1 +: 7], data_in_encrypted[i]};
          3'b001: img_out[i*8 +: 8] = {img_in[i*8 + 2 +: 6], data_in_encrypted[(2*i) +: 2]};
          3'b010: img_out[i*8 +: 8] = {img_in[i*8 + 3 +: 5], data_in_encrypted[(3*i) +: 3]};
          3'b011: img_out[i*8 +: 8] = {img_in[i*8 + 4 +: 4], data_in_encrypted[(4*i) +: 4]};
          default: img_out[i*8 +: 8] = img_in[i*8 +: 8];
        endcase
        if (bpp <= 3'b011) begin
          embedded_pixel_count = embedded_pixel_count + 1;
        end
      end
    end
    if (embedded_pixel_count > 0) begin
      embedding_done = 2'b10;
    end else begin
      embedding_done = 2'b11;
    end
  end
end

endmodule
