module cache_mshr_tlm_top (
    input wire        clk,
    input wire        reset,

    input wire        allocate_valid,
    output wire       allocate_ready,
    input wire [9:0]  allocate_addr,
    input wire        allocate_rw,
    input wire [55:0] allocate_data,
    output wire [4:0] allocate_id,
    output wire       allocate_pending,
    output wire [4:0] allocate_previd,

    input wire        finalize_valid,
    input wire [4:0]  finalize_id,

    input wire        alloc_req_valid,
    input wire [9:0]  alloc_addr,
    input wire        alloc_rw,
    input wire [55:0] alloc_data,
    output wire       alloc_req_ready,
    output wire       alloc_rsp_valid,
    output wire [10:0] alloc_rsp_data,
    input wire        alloc_rsp_ready
);
    wire inner_allocate_valid;
    wire inner_allocate_ready;
    wire [9:0] inner_allocate_addr;
    wire inner_allocate_rw;
    wire [55:0] inner_allocate_data;

    reg rsp0_valid, rsp1_valid;
    reg [10:0] rsp0_data, rsp1_data;
    reg capture_valid;

    assign alloc_req_ready = inner_allocate_ready && !rsp1_valid;
    assign inner_allocate_valid = alloc_req_valid ? (alloc_req_valid && alloc_req_ready) : allocate_valid;
    assign inner_allocate_addr = alloc_req_valid ? alloc_addr : allocate_addr;
    assign inner_allocate_rw = alloc_req_valid ? alloc_rw : allocate_rw;
    assign inner_allocate_data = alloc_req_valid ? alloc_data : allocate_data;

    assign allocate_ready = inner_allocate_ready;
    assign alloc_rsp_valid = rsp0_valid;
    assign alloc_rsp_data = rsp0_data;

    cache_mshr dut_i (
        .clk(clk),
        .reset(reset),
        .allocate_valid(inner_allocate_valid),
        .allocate_ready(inner_allocate_ready),
        .allocate_addr(inner_allocate_addr),
        .allocate_rw(inner_allocate_rw),
        .allocate_data(inner_allocate_data),
        .allocate_id(allocate_id),
        .allocate_pending(allocate_pending),
        .allocate_previd(allocate_previd),
        .finalize_valid(finalize_valid),
        .finalize_id(finalize_id)
    );

    wire [10:0] captured_rsp = {allocate_pending, allocate_previd, allocate_id};

    always @(posedge clk) begin
        if (reset) begin
            rsp0_valid <= 1'b0;
            rsp1_valid <= 1'b0;
            rsp0_data <= 11'd0;
            rsp1_data <= 11'd0;
            capture_valid <= 1'b0;
        end else begin
            if (alloc_rsp_ready && rsp0_valid) begin
                rsp0_valid <= rsp1_valid;
                rsp0_data <= rsp1_data;
                rsp1_valid <= 1'b0;
                rsp1_data <= 11'd0;
            end

            if (capture_valid) begin
                if (!rsp0_valid || (alloc_rsp_ready && rsp0_valid)) begin
                    rsp0_valid <= 1'b1;
                    rsp0_data <= captured_rsp;
                end else if (!rsp1_valid) begin
                    rsp1_valid <= 1'b1;
                    rsp1_data <= captured_rsp;
                end
            end

            capture_valid <= alloc_req_valid && alloc_req_ready;
        end
    end
endmodule
