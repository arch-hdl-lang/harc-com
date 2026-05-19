module wide1024_tlm (
    input  logic          clk,
    input  logic          rst,
    input  logic [1023:0] payload,

    input  logic          send_req_valid,
    input  logic [1023:0] send_data,
    output logic          send_req_ready,
    output logic          send_rsp_valid,
    output logic [1023:0] send_rsp_data,
    input  logic          send_rsp_ready
);
    assign send_req_ready = 1'b1;

    always_ff @(posedge clk) begin
        if (rst) begin
            send_rsp_valid <= 1'b0;
            send_rsp_data <= '0;
        end else begin
            if (send_rsp_ready) begin
                send_rsp_valid <= 1'b0;
            end
            if (send_req_valid) begin
                send_rsp_valid <= 1'b1;
                send_rsp_data <= send_data;
            end
        end
    end
endmodule
