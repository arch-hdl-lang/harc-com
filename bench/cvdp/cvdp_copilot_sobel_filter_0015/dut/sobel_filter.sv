module sobel_filter (
    input   logic           clk,
    input   logic           rst_n,
    input   logic   [7:0]   pixel_in,
    input   logic           valid_in,
    output  logic   [7:0]   edge_out,
    output  logic           valid_out
);
 
    logic signed [10:0]  Gx, Gy; 
    logic        [7:0]   pixel_buffer  [8:0]; 
    logic        [3:0]   pixel_count; 
    localparam THRESHOLD = 11'd128;
    integer i;

always @(posedge clk or negedge rst_n) begin
    if (!rst_n) begin
        for ( i = 0; i < 9; i = i + 1) begin
            pixel_buffer[i] <= 8'd0;
        end
    end else if (valid_in) begin
        for ( i = 8; i > 0; i = i - 1) begin
            pixel_buffer[i] <= pixel_buffer[i-1];
        end
        pixel_buffer[0] <= pixel_in;
    end
end

always @(posedge clk or negedge rst_n) begin
    if (!rst_n) begin
       pixel_count <= 4'd0;
    end else if (valid_in)begin
        pixel_count <= pixel_count + 1;
        if (pixel_count == 4'd9) begin
          pixel_count <= 4'd0;
        end
    end else begin
       pixel_count <= 0;
    end    
end

always @(*) begin    
    Gx = 11'sd0;
    Gy = 11'sd0;
    edge_out = 8'd0;
    valid_out = 1'b0;
    if (pixel_count == 4'd9 ) begin
        valid_out = 1'b1;
        Gx = (pixel_buffer[0]) + (-(pixel_buffer[2])) + (pixel_buffer[3] << 1) + (-(pixel_buffer[5] << 1)) + (pixel_buffer[6]) + (-pixel_buffer[8]);        
        Gy = (pixel_buffer[0]) + ((pixel_buffer[1] << 1)) + (pixel_buffer[2]) + (-pixel_buffer[6]) + (-(pixel_buffer[7] << 1)) + (-pixel_buffer[8]);        
        edge_out = ((Gx < 0 ? -Gx : Gx) + (Gy < 0 ? -Gy : Gy)) > THRESHOLD ? 8'd255 : 8'd0;
    end 
end
endmodule