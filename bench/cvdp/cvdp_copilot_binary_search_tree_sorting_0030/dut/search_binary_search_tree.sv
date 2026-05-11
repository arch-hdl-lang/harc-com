module search_binary_search_tree #(
    parameter DATA_WIDTH = 16,         // Width of the data
    parameter ARRAY_SIZE = 15          // Maximum number of elements in the BST
) (

    input clk,                         // Clock signal
    input reset,                       // Reset signal
    input reg start,                   // Start signal to initiate the search
    input reg [DATA_WIDTH-1:0] search_key, // Key to search in the BST
    input reg [$clog2(ARRAY_SIZE):0] root, // Root node of the BST
    input reg [ARRAY_SIZE*DATA_WIDTH-1:0] keys, // Node keys in the BST
    input reg [ARRAY_SIZE*($clog2(ARRAY_SIZE)+1)-1:0] left_child, // Left child pointers
    input reg [ARRAY_SIZE*($clog2(ARRAY_SIZE)+1)-1:0] right_child, // Right child pointers
    output reg [$clog2(ARRAY_SIZE):0] key_position, // Position of the found key
    output reg complete_found,         // Signal indicating search completion
    output reg search_invalid
);
                                                                                                                                        
    // Parameters for FSM states
    parameter S_IDLE = 3'b000,                 // Idle state
              S_INIT = 3'b001,                 // Initialization state
              S_SEARCH_LEFT = 3'b010,          // Search in left subtree
              S_SEARCH_LEFT_RIGHT = 3'b011,    // Search in both left and right subtrees
              S_COMPLETE_SEARCH = 3'b100;      // Search completion state
   
    // Registers to store the current FSM state
    reg [2:0] search_state;

    // Variables to manage traversal
    reg [$clog2(ARRAY_SIZE):0] position;       // Position of the current node
    reg found;                                 // Indicates if the key is found

    reg left_done, right_done;                 // Flags to indicate completion of left and right subtree traversals

    // Stacks for managing traversal of left and right subtrees
    reg [ARRAY_SIZE*($clog2(ARRAY_SIZE)+1)-1:0] left_stack;  // Stack for left subtree traversal
    reg [ARRAY_SIZE*($clog2(ARRAY_SIZE)+1)-1:0] right_stack; // Stack for right subtree traversal
    reg [$clog2(ARRAY_SIZE)-1:0] sp_left;         // Stack pointer for left subtree
    reg [$clog2(ARRAY_SIZE)-1:0] sp_right;        // Stack pointer for right subtree

    // Pointers for the current nodes in left and right subtrees
    reg [$clog2(ARRAY_SIZE):0] current_left_node;  // Current node in the left subtree
    reg [$clog2(ARRAY_SIZE):0] current_right_node; // Current node in the right subtree

    // Output indices for traversal
    reg [$clog2(ARRAY_SIZE)-1:0] left_output_index;  // Output index for left subtree
    reg [$clog2(ARRAY_SIZE)-1:0] right_output_index; // Output index for right subtree

    // Integer for loop iterations
    integer i;

    // Always block triggered on the rising edge of the clock or reset signal
    always @(posedge clk or posedge reset) begin
        if (reset) begin
            // Reset all states and variables
            search_state <= S_IDLE;  // Set state to IDLE
            found <= 0;              // Reset found flag
            position <= {($clog2(ARRAY_SIZE)+1){1'b1}}; // Invalid position
            complete_found <= 0;     // Reset complete_found signal
            key_position <= {($clog2(ARRAY_SIZE)+1){1'b1}}; // Invalid key position
            left_output_index <= 0;  // Reset left output index
            right_output_index <= 0; // Reset right output index
            sp_left <= 0;            // Reset left stack pointer
            sp_right <= 0;           // Reset right stack pointer
            left_done <= 0;          // Reset left_done flag
            right_done <= 0;         // Reset right_done flag
            search_state <= S_IDLE;  // Set state to IDLE
            search_invalid <= 0;        // Set invalid_key to 0
            
            // Clear the stacks
            for (i = 0; i < ARRAY_SIZE; i = i + 1) begin
                left_stack[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                right_stack[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
            end

        end else begin
            // Main FSM logic
            case (search_state)
                S_IDLE: begin
                    // Reset intermediate variables
                    for (i = 0; i < ARRAY_SIZE+1; i = i + 1) begin
                        left_stack[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                        right_stack[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                    end
                    complete_found <= 0;
                    search_invalid <= 0;

                    if (start) begin
                        // Start the search
                        left_output_index <= 0;
                        right_output_index <= 0;
                        sp_left <= 0;
                        sp_right <= 0;
                        left_done <= 0;
                        right_done <= 0;
                        found <= 0;
                        position <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                        key_position <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                        search_state <= S_INIT; // Move to INIT state
                        $display("ROOT NODE: %0h", root);
                    end
                end

                S_INIT: begin
                    if (root != {($clog2(ARRAY_SIZE)+1){1'b1}}) begin
                        // Compare the search key with the root key
                        if (search_key == keys[root*DATA_WIDTH +: DATA_WIDTH]) begin
                            found <= 1;
                            if (left_child[0*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] == {($clog2(ARRAY_SIZE)+1){1'b1}}) begin
                                position <= 0;
                                search_state <= S_COMPLETE_SEARCH; // Move to complete search state
                            end else begin
                                search_state <= S_SEARCH_LEFT; // Search in the left subtree
                                current_left_node <= left_child[root*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            end
                        end else if (keys[0*DATA_WIDTH +: DATA_WIDTH] > search_key) begin
                            search_state <= S_SEARCH_LEFT;
                            current_left_node <= left_child[root*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                        end else begin
                            current_left_node <= left_child[root*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            current_right_node <= right_child[root*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            search_state <= S_SEARCH_LEFT_RIGHT; // Search in both left and right subtrees
                        end
                    end else begin
                        search_state <= S_COMPLETE_SEARCH; // Root is null
                    end
                end

                S_SEARCH_LEFT: begin
                    // Traverse the left subtree
                    if (current_left_node != {($clog2(ARRAY_SIZE)+1){1'b1}}) begin
                        left_stack[sp_left*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= current_left_node;
                        sp_left <= sp_left + 1;
                        current_left_node <= left_child[current_left_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                    end else if (sp_left > 0) begin
                        sp_left <= sp_left - 1;
                        current_left_node <= left_stack[(sp_left - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                        if (search_key == keys[left_stack[(sp_left - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*DATA_WIDTH +: DATA_WIDTH]) begin
                            found <= 1;
                            position <= left_output_index;   
                            search_state <= S_COMPLETE_SEARCH; // Move to complete search state
                        end
                        left_output_index <= left_output_index + 1;
                        current_left_node <= right_child[left_stack[(sp_left - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                    end else begin //if (!left_done) begin
                        if (found == 1) begin
                            position <= left_output_index;
                        end 
                        left_done <= 1;
                        search_state <= S_COMPLETE_SEARCH;
                    end
                end

                S_SEARCH_LEFT_RIGHT: begin
                    // Traverse both left and right subtrees
                    if (!left_done && current_left_node != {($clog2(ARRAY_SIZE)+1){1'b1}}) begin
                        left_stack[sp_left*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= current_left_node;
                        sp_left <= sp_left + 1;
                        current_left_node <= left_child[current_left_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                    end else if (!left_done && sp_left > 0) begin
                        sp_left <= sp_left - 1;
                        current_left_node <= left_stack[(sp_left - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                        left_output_index <= left_output_index + 1;
                        current_left_node <= right_child[left_stack[(sp_left - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                    end else if (!left_done) begin
                        left_done <= 1;
                    end

                    if (!found) begin
                        if (!right_done && current_right_node != {($clog2(ARRAY_SIZE)+1){1'b1}}) begin
                            right_stack[sp_right*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= current_right_node;
                            sp_right <= sp_right + 1;
                            current_right_node <= left_child[current_right_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                        end else if (!right_done && sp_right > 0) begin
                            sp_right <= sp_right - 1;
                            current_right_node <= right_stack[(sp_right - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                                if (search_key == keys[right_stack[(sp_right - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*DATA_WIDTH +: DATA_WIDTH]) begin
                                    found <= 1;
                                end
                            right_output_index <= right_output_index + 1;
                            current_right_node <= right_child[right_stack[(sp_right - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                        end else if (!right_done) begin
                            right_done <= 1;
                        end

                    end else if (left_done) begin
                        position <= left_output_index + right_output_index;
                        search_state <= S_COMPLETE_SEARCH;  
                    end

                    if (right_done && left_done && !found) begin
                        search_state <= S_COMPLETE_SEARCH;
                    end
                end

                S_COMPLETE_SEARCH: begin
                    // Complete the search
                    if (!found) begin
                        complete_found <= 0;
                        key_position <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                        search_invalid <= 1;
                    end else begin
                        complete_found  <= 1;
                        key_position <= position;
                        search_invalid <= 0;
                    end
                    search_state <= S_IDLE; // Return to IDLE state
                end

                default: begin
                    search_state <= S_IDLE; // Default to IDLE state
                end
            endcase
        end
    end

endmodule
