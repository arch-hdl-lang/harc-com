`timescale 1ns/1ps

module hill_cipher (
    input logic clk,
    input logic reset,
    input logic start,
    input logic [14:0] plaintext,   // 3 letters, 5 bits each
    input logic [44:0] key,         // 9 elements, 5 bits each
    output logic [14:0] ciphertext, // 3 letters, 5 bits each 
    output logic done
);

    logic [4:0] P0, P1, P2;
    logic [4:0] K00, K01, K02;
    logic [4:0] K10, K11, K12;
    logic [4:0] K20, K21, K22;

    logic [5:0] temp0, temp1, temp2;
    logic [4:0] C0_reg, C1_reg, C2_reg;

    typedef enum logic [1:0] {
        IDLE        = 2'b00,
        COMPUTE     = 2'b01,
        COMPUTE_MOD = 2'b10,
        DONE        = 2'b11
    } state_t;

    state_t current_state, next_state;

    assign P0 = plaintext[14:10];
    assign P1 = plaintext[9:5];
    assign P2 = plaintext[4:0];

    assign K00 = key[44:40];
    assign K01 = key[39:35];
    assign K02 = key[34:30];
    assign K10 = key[29:25];
    assign K11 = key[24:20];
    assign K12 = key[19:15];
    assign K20 = key[14:10];
    assign K21 = key[9:5];
    assign K22 = key[4:0];

    always_ff @(posedge clk or posedge reset) begin
        if (reset)
            current_state <= IDLE;
        else
            current_state <= next_state;
    end

    always_comb begin
        next_state = current_state;
        done = 1'b0;

        case (current_state)
            IDLE: begin
                if (start)
                    next_state = COMPUTE;
            end
            COMPUTE: begin
                next_state = COMPUTE_MOD;
            end
            COMPUTE_MOD: begin
                next_state = DONE;
            end
            DONE: begin
                done = 1'b1;
                next_state = IDLE;
            end
            default: next_state = IDLE;
        endcase
    end

    always_ff @(posedge clk or posedge reset) begin
        if (reset) begin
            temp0 <= 12'b0;
            temp1 <= 12'b0;
            temp2 <= 12'b0;
            C0_reg <= 5'b0;
            C1_reg <= 5'b0;
            C2_reg <= 5'b0;
        end
        else begin
            case (current_state)
                COMPUTE: begin
                    temp0 <= (K00 * P0)%26 + (K01 * P1)%26 + (K02 * P2)%26;
                    temp1 <= (K10 * P0)%26 + (K11 * P1)%26 + (K12 * P2)%26;
                    temp2 <= (K20 * P0)%26 + (K21 * P1)%26 + (K22 * P2)%26;
                end
                COMPUTE_MOD: begin
                    C0_reg <= temp0%26;
                    C1_reg <= temp1%26;
                    C2_reg <= temp2%26;
                end
                default: begin
                    temp0 <= 12'b0;
                    temp1 <= 12'b0;
                    temp2 <= 12'b0;
                    C0_reg <= 5'b0;
                    C1_reg <= 5'b0;
                    C2_reg <= 5'b0;
                end
            endcase
        end
    end

    assign ciphertext = {C0_reg, C1_reg, C2_reg};

endmodule