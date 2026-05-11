module ping_pong_buffer (
    input logic clk,
    input logic rst_n,
    input logic write_enable,
    input logic read_enable,
    input logic [7:0] data_in,
    output logic [7:0] data_out,
    output logic buffer_full,
    output logic buffer_empty,
    output reg buffer_select
);

    localparam DEPTH = 256; 
    localparam ADDR_WIDTH = 8; 

    logic [ADDR_WIDTH-1:0] write_ptr, read_ptr; 
    logic [7:0] data_out0, data_out1; 

    // Memory 0
    dual_port_memory memory0 (
        .clk(clk),
        .we(write_enable && !buffer_select && !buffer_full),
        .write_addr(write_ptr),
        .din(data_in),
        .read_addr(read_ptr),
        .dout(data_out0)
    );

    // Memory 1
    dual_port_memory memory1 (
        .clk(clk),
        .we(write_enable && buffer_select && !buffer_full),
        .write_addr(write_ptr),
        .din(data_in),
        .read_addr(read_ptr),
        .dout(data_out1)
    );

    always_ff @(posedge clk or negedge rst_n) begin
        if (!rst_n) begin
            write_ptr <= 0;
            read_ptr <= 0;
            buffer_select <= 0;
            buffer_full <= 0;
            buffer_empty <= 1;
        end else begin
            // Write operation logic
            if (write_enable && !buffer_full) begin
                write_ptr <= (write_ptr + 1) % DEPTH;
                if (write_ptr == DEPTH - 1) begin
                    buffer_full <= 1; 
                end
            end

            if (read_enable && !buffer_empty) begin
                read_ptr <= (read_ptr + 1) % DEPTH;
                buffer_empty <= (read_ptr == write_ptr); 
                if (read_ptr == DEPTH - 1) begin
                    buffer_select <= !buffer_select; 
                end
            end

            buffer_full <= (write_ptr + 1) % DEPTH == read_ptr;
            buffer_empty <= read_ptr == write_ptr;
        end
    end

    assign data_out = buffer_select ? data_out1 : data_out0;
endmodule
module dual_port_memory (
    input logic clk,
    input logic we,
    input logic [7:0] write_addr,
    input logic [7:0] din,
    input logic [7:0] read_addr,
    output logic [7:0] dout
);

    logic [7:0] mem [255:0];

    always_ff @(posedge clk) begin
        if (we) begin
            mem[write_addr] <= din; 
        end
        dout <= mem[read_addr]; 
    end
endmodule
