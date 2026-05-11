module encoder_8b10b (
    input  logic        clk_in,           // Trigger on rising edge
    input  logic        reset_in,         // Reset, assert HI
    input  logic        control_in,       // Control character, assert HI for control words
    input  logic        disparity_data_in,// Current running disparity input for data
    input  logic [7:0]  encoder_in,       // 8-bit input
    output logic        disparity_out,    // Running disparity: HI = +1, LO = 0
    output logic [9:0]  encoder_out       // 10-bit codeword output
);

    logic disparity_control_out;
    logic disparity_data_out;
    logic [9:0] encoder_control_out;
    logic [9:0] encoder_data_out;

    assign disparity_out = control_in ? disparity_control_out : disparity_data_out;
    assign encoder_out   = control_in ? encoder_control_out : encoder_data_out;

    encoder_8b10b_control enc_control (
        .clk_in(clk_in),
        .reset_in(reset_in),
        .control_in(control_in),
        .encoder_in(encoder_in),
        .disparity_out(disparity_control_out),
        .encoder_out(encoder_control_out)
    );
    
    encoder_8b10b_data enc_data (
        .clk_in(clk_in),
        .reset_in(reset_in),
        .control_in(control_in),
        .encoder_in(encoder_in),
        .ein_rd(disparity_data_in),
        .disparity_out(disparity_data_out),
        .encoder_out(encoder_data_out)
    );

endmodule

module encoder_8b10b_control (
    input  logic        clk_in,
    input  logic        reset_in,
    input  logic        control_in,
    input  logic [7:0]  encoder_in,
    output logic        disparity_out,
    output logic [9:0]  encoder_out
);

    typedef enum logic [1:0] {RD_MINUS, RD_PLUS} t_state_type;
    t_state_type current_disparity;

    parameter [31:0] disparity_table_6b = 32'b11101000100000011000000110010111;
    parameter [7:0]  disparity_table_4b = 8'b10001001;

    logic        disparity_track;
    logic [7:0]  registered_input;
    logic        registered_control;
    logic [2:0]  index_3b;
    logic        disparity_4b;
    logic [4:0]  index_5b;
    logic        disparity_6b;

    assign index_3b     = registered_input[7:5];
    assign disparity_4b = disparity_table_4b[index_3b];
    assign index_5b     = registered_input[4:0];
    assign disparity_6b = disparity_table_6b[index_5b];
    assign disparity_out = disparity_track;

    always_comb begin
        logic [9:0] control_code;

        if (registered_control) begin
            case (registered_input)
                8'h1C: control_code = 10'b0011110100;
                8'h3C: control_code = 10'b0011111001;
                8'h5C: control_code = 10'b0011110101;
                8'h7C: control_code = 10'b0011110011;
                8'h9C: control_code = 10'b0011110010;
                8'hBC: control_code = 10'b0011111010;
                8'hDC: control_code = 10'b0011110110;
                8'hFC: control_code = 10'b0011111000;
                8'hF7: control_code = 10'b1110101000;
                8'hFB: control_code = 10'b1101101000;
                8'hFD: control_code = 10'b1011101000;
                8'hFE: control_code = 10'b0111101000;
                default: control_code = 10'b0000000000;
            endcase

            if (current_disparity == RD_MINUS) begin
                encoder_out = control_code;
                disparity_track = 1'b0;
            end else begin
                encoder_out = ~control_code;
                disparity_track = 1'b1;
            end
        end else begin
            encoder_out = 10'b0000000000;
            control_code = 10'b0;
            disparity_track = 1'b0;
        end
    end

    always_ff @(posedge clk_in or posedge reset_in) begin
        if (reset_in) begin
            current_disparity <= RD_MINUS;
        end else begin
            if ((registered_input[1:0] != 2'b00) && registered_control) begin
                current_disparity <= current_disparity;
            end else begin
                case (current_disparity)
                    RD_MINUS: begin
                        if ((registered_control ^ disparity_6b ^ disparity_4b) != 1'b0) begin
                            current_disparity <= RD_PLUS;
                        end
                    end
                    RD_PLUS: begin
                        if ((registered_control ^ disparity_6b ^ disparity_4b) != 1'b0) begin
                            current_disparity <= RD_MINUS;
                        end
                    end
                    default: current_disparity <= RD_MINUS;
                endcase
            end
        end
    end

    always_ff @(posedge clk_in or posedge reset_in) begin
        if (reset_in) begin
            registered_input  <= 8'b00000000;
            registered_control <= 1'b0;
        end else begin
            registered_input  <= encoder_in;
            registered_control <= control_in;
        end
    end

endmodule

module encoder_8b10b_data (
    input  logic        clk_in,
    input  logic        reset_in,
    input  logic        control_in,
    input  logic        ein_rd,
    input  logic [7:0]  encoder_in,
    output logic        disparity_out,
    output logic [9:0]  encoder_out
);

    wire [7:0] encoder_in_w;
    wire K;

    wire is_all_low;
    wire is_three_low;
    wire is_two_low_two_high;
    wire is_three_high;
    wire is_all_high;

    wire disparity_case_0;
    wire disparity_case_1;
    wire disparity_case_2;
    wire disparity_case_3;
    wire invert_a_i;
    wire disp4, disp5, disp6, invert_fj;
    
    logic a_w, b_w, c_w, d_w, e_w, i_w, f_w, g_w, h_w, j_w;
    logic a, b, c, d, e, i, f, g, h, j;
    logic rd1_part, rd1;
    logic disparity_reg;
    logic SorK;

    assign encoder_in_w = encoder_in;
    assign K = control_in;
    assign is_all_low          = (!encoder_in_w[0] & !encoder_in_w[1] & !encoder_in_w[2] & !encoder_in_w[3]);
    assign is_three_low        = (!encoder_in_w[0] & !encoder_in_w[1] & !encoder_in_w[2] &  encoder_in_w[3]) |
                                 (!encoder_in_w[0] & !encoder_in_w[1] &  encoder_in_w[2] & !encoder_in_w[3]) |
                                 (!encoder_in_w[0] &  encoder_in_w[1] & !encoder_in_w[2] & !encoder_in_w[3]) |
                                 ( encoder_in_w[0] & !encoder_in_w[1] & !encoder_in_w[2] & !encoder_in_w[3]);
    assign is_two_low_two_high = (!encoder_in_w[0] & !encoder_in_w[1] &  encoder_in_w[2] &  encoder_in_w[3]) |
                                 (!encoder_in_w[0] &  encoder_in_w[1] &  encoder_in_w[2] & !encoder_in_w[3]) |
                                 ( encoder_in_w[0] &  encoder_in_w[1] & !encoder_in_w[2] & !encoder_in_w[3]) |
                                 ( encoder_in_w[0] & !encoder_in_w[1] &  encoder_in_w[2] & !encoder_in_w[3]) |
                                 ( encoder_in_w[0] & !encoder_in_w[1] & !encoder_in_w[2] &  encoder_in_w[3]) |
                                 (!encoder_in_w[0] &  encoder_in_w[1] & !encoder_in_w[2] &  encoder_in_w[3]);
    assign is_three_high       = ( encoder_in_w[0] &  encoder_in_w[1] &  encoder_in_w[2] & !encoder_in_w[3]) |
                                 ( encoder_in_w[0] &  encoder_in_w[1] & !encoder_in_w[2] &  encoder_in_w[3]) |
                                 ( encoder_in_w[0] & !encoder_in_w[1] &  encoder_in_w[2] &  encoder_in_w[3]) |
                                 (!encoder_in_w[0] &  encoder_in_w[1] &  encoder_in_w[2] &  encoder_in_w[3]);
    assign is_all_high         = ( encoder_in_w[0] &  encoder_in_w[1] &  encoder_in_w[2] &  encoder_in_w[3]);

    assign disparity_case_0    = (!is_two_low_two_high & !is_three_high & !encoder_in_w[4]);
    assign disparity_case_1    = (is_three_high & !encoder_in_w[3] & !encoder_in_w[4]);
    assign disparity_case_2    = (is_three_low &  encoder_in_w[3] &  encoder_in_w[4]);
    assign disparity_case_3    = (!is_two_low_two_high & !is_three_low &  encoder_in_w[4]);
    assign invert_a_i          = !(ein_rd ? (disparity_case_3 | disparity_case_1 | K) : (disparity_case_0 | disparity_case_2));

    always_comb begin
        a_w = !encoder_in_w[0];
        b_w = ((is_all_low) ? 1'b0 : (is_all_high) ? 1'b1 : !encoder_in_w[1]);
        c_w = ((is_all_low) ? 1'b0 : (is_three_low & encoder_in_w[3] & encoder_in_w[4]) ? 1'b0 : !encoder_in_w[2]);
        d_w = ((is_all_high) ? 1'b1 : !encoder_in_w[3]);
        e_w = ((is_three_low & !encoder_in_w[4]) ? 1'b0 : (is_three_low & encoder_in_w[3] & encoder_in_w[4]) ? 1'b1 : !encoder_in_w[4]);
        i_w = ((is_two_low_two_high & !encoder_in_w[4]) ? 1'b0 : (is_all_low & encoder_in_w[4]) ? 1'b0 : 
               (is_three_low & !encoder_in_w[3] & encoder_in_w[4]) ? 1'b0 : (is_all_high & encoder_in_w[4]) ? 1'b0 : 
               (is_two_low_two_high & K) ? 1'b0 : 1'b1);

        rd1_part = (disparity_case_0 | disparity_case_2 | disparity_case_3);
        rd1 = (rd1_part | K) ^ ein_rd;
    end

    assign {a, b, c, d, e, i} = (~invert_a_i) ? {a_w, b_w, c_w, d_w, e_w, i_w} : ~{a_w, b_w, c_w, d_w, e_w, i_w};

    always_comb begin
        SorK = (e & i & !rd1) | (!e & !i & rd1) | K;
    end

    assign disp4 = (!encoder_in_w[5] & !encoder_in_w[6]);
    assign disp5 = (encoder_in_w[5] & encoder_in_w[6]);
    assign disp6 = ((encoder_in_w[5] ^ encoder_in_w[6])) | K;
    assign invert_fj = !(rd1 ? disp5 : (disp4 | disp6));

    always_comb begin
        f_w = ((encoder_in_w[5] & encoder_in_w[6] & encoder_in_w[7] & SorK) ? 1'b1 : !encoder_in_w[5]);
        g_w = ((!encoder_in_w[5] & !encoder_in_w[6] & !encoder_in_w[7]) ? 1'b0 : !encoder_in_w[6]);
        h_w = !encoder_in_w[7];
        j_w = (((encoder_in_w[5] ^ encoder_in_w[6]) & !encoder_in_w[7]) ? 1'b0 : (encoder_in_w[5] & encoder_in_w[6] & encoder_in_w[7] & SorK) ? 1'b0 : 1'b1);
    end

    assign {f, g, h, j} = (invert_fj | disp6) ? ~{f_w, g_w, h_w, j_w} : {f_w, g_w, h_w, j_w};

    assign disparity_reg = (reset_in) ? 1'b0 : (disp4 | (encoder_in_w[5] & encoder_in_w[6] & encoder_in_w[7])) ^ rd1;

    always_ff @(posedge clk_in or posedge reset_in) begin
        if (reset_in) begin
            disparity_out <= 1'b0;
            encoder_out <= 10'b0000000000;
        end else begin
            if (!control_in) begin
                disparity_out <= disparity_reg;
                encoder_out <= {a, b, c, d, e, i, f, g, h, j};
            end else begin
                disparity_out <= 1'b0;
                encoder_out <= 10'b0000000000;
            end
        end
    end

endmodule

