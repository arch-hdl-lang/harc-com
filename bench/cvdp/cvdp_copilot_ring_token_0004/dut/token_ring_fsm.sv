module token_ring_fsm (
    input logic clk,              // Clock input
    input logic rst,              // Reset input
    input logic [3:0] data_in,    // Data input from node
    output reg [3:0] data_out,    // Data output to node
    input logic has_data_to_send, // Signal indicating data is ready to send
    output reg [3:0] token_received, // Token received status for nodes
    output reg [3:0] token        // Token status being passed between nodes
);

typedef enum logic [1:0] {
    NODE0 = 2'b00,
    NODE1 = 2'b01,
    NODE2 = 2'b10,
    NODE3 = 2'b11
} state_t;

state_t current_state, next_state;

always_ff @(posedge clk or posedge rst) begin
    if (rst) begin
        current_state <= NODE0;
        token <= 4'b0001;
        token_received <= 4'b0001;
        data_out <= 4'b0000;
    end else begin
        case (current_state)
            NODE0: begin
                token_received <= 4'b0001;
                token <= 4'b0010;
                next_state = NODE1;
            end
            NODE1: begin
                token_received <= 4'b0010;
                token <= 4'b0100;
                next_state = NODE2;
            end
            NODE2: begin
                token_received <= 4'b0100;
                token <= 4'b1000;
                next_state = NODE3;
            end
            NODE3: begin
                token_received <= 4'b1000;
                token <= 4'b0001;
                next_state = NODE0;
            end
            default: begin
                next_state = NODE0;
            end
        endcase

        if (token_received[current_state] && has_data_to_send) begin
            data_out <= data_in;
        end else begin
            data_out <= 4'b0000;
        end

        current_state <= next_state;
    end
end

endmodule