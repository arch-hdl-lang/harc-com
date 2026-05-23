module PostEvalProvider(
    input  logic        clk,
    input  logic        rst,
    output logic        req_valid,
    output logic [7:0]  req_addr,
    input  logic        rsp_valid,
    input  logic [31:0] rsp_data,
    output logic        rsp_ready,
    output logic [31:0] accepted_count,
    output logic [31:0] duplicate_count,
    output logic [31:0] last_data,
    output logic        done
);
    logic [31:0] cycle_count;
    logic [31:0] last_accepted_cycle;

    always_ff @(posedge clk) begin
        if (rst) begin
            cycle_count <= 0;
            req_valid <= 0;
            req_addr <= 0;
            rsp_ready <= 0;
            accepted_count <= 0;
            duplicate_count <= 0;
            last_data <= 0;
            last_accepted_cycle <= 32'hffff_ffff;
            done <= 0;
        end else begin
            cycle_count <= cycle_count + 1;
            req_valid <= 0;
            rsp_ready <= 1;

            case (cycle_count)
                1: begin
                    req_valid <= 1;
                    req_addr <= 8'h10;
                end
                2: begin
                    req_valid <= 1;
                    req_addr <= 8'h20;
                end
                3: begin
                    req_valid <= 1;
                    req_addr <= 8'h30;
                    rsp_ready <= 0;
                end
                4: begin
                    rsp_ready <= 0;
                end
                5: begin
                    rsp_ready <= 1;
                end
                8: begin
                    done <= 1;
                end
                default: begin
                end
            endcase

            if (rsp_valid && rsp_ready) begin
                accepted_count <= accepted_count + 1;
                last_data <= rsp_data;
                if (last_accepted_cycle + 1 == cycle_count && rsp_data == last_data) begin
                    duplicate_count <= duplicate_count + 1;
                end
                last_accepted_cycle <= cycle_count;
            end
        end
    end
endmodule
