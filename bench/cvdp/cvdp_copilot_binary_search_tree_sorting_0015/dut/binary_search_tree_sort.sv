module binary_search_tree_sort #(
    parameter DATA_WIDTH = 32,
    parameter ARRAY_SIZE = 15
) (
    input clk,
    input reset,
    input reg [ARRAY_SIZE*DATA_WIDTH-1:0] data_in, // Input data to be sorted
    input start,
    output reg [ARRAY_SIZE*DATA_WIDTH-1:0] sorted_out, // Sorted output
    output reg done
);

    // Parameters for top-level FSM states
    parameter IDLE = 2'b00, BUILD_TREE = 2'b01, SORT_TREE = 2'b10;

    // Parameters for nested FSM states (Build Tree)
    parameter INIT = 2'b00, INSERT = 2'b01, TRAVERSE = 2'b10, COMPLETE = 2'b11;

    // Parameters for nested FSM states (Sort Tree)
    parameter S_INIT = 2'b00, S_SORT_LEFT_RIGHT = 2'b01, S_MERGE_RESULTS = 2'b10;
    // Registers for FSM states
    reg [1:0] top_state, build_state, sort_state;

    // BST representation
    reg [ARRAY_SIZE*DATA_WIDTH-1:0] data_in_copy;
    reg [ARRAY_SIZE*DATA_WIDTH-1:0] keys; // Array to store node keys
    reg [ARRAY_SIZE*($clog2(ARRAY_SIZE)+1)-1:0] left_child; // Left child pointers
    reg [ARRAY_SIZE*($clog2(ARRAY_SIZE)+1)-1:0] right_child; // Right child pointers
    reg [$clog2(ARRAY_SIZE):0] root; // Root node pointer
    reg [$clog2(ARRAY_SIZE)-1:0] next_free_node; // Pointer to the next free node

    // Working registers
    reg [$clog2(ARRAY_SIZE)-1:0] current_node; // Current node being processed
    reg [$clog2(ARRAY_SIZE):0] input_index; // Index for input data
    reg [DATA_WIDTH-1:0] temp_data; // Temporary data register

    // Intermediate buffers for storing sorted values of left and right subtrees
    reg [ARRAY_SIZE*DATA_WIDTH-1:0] left_sorted;  // Buffer for left subtree sorted values
    reg [ARRAY_SIZE*DATA_WIDTH-1:0] right_sorted; // Buffer for right subtree sorted values

    // Stack and pointers for left and right subtree traversal
    reg [ARRAY_SIZE*($clog2(ARRAY_SIZE)+1)-1:0] left_stack; // Stack for left subtree traversal
    reg [ARRAY_SIZE*($clog2(ARRAY_SIZE)+1)-1:0] right_stack; // Stack for right subtree traversal
    reg [$clog2(ARRAY_SIZE)-1:0] sp_left; // Stack pointer for left subtree
    reg [$clog2(ARRAY_SIZE)-1:0] sp_right; // Stack pointer for right subtree

    // Current node pointers for left and right subtrees
    reg [$clog2(ARRAY_SIZE):0] current_left_node; // Current node in left subtree
    reg [$clog2(ARRAY_SIZE):0] current_right_node; // Current node in right subtree

    // Flags to indicate when sorting of left and right subtrees is done
    reg left_done; // Flag for completion of left subtree sorting
    reg right_done; // Flag for completion of right subtree sorting

    // Output indices for left and right subtree buffers
    reg [$clog2(ARRAY_SIZE)-1:0] left_output_index; // Output index for left_sorted buffer
    reg [$clog2(ARRAY_SIZE)-1:0] right_output_index; // Output index for right_sorted buffer

    // Initialize all variables
    integer i, j;

    always @(posedge clk or posedge reset) begin
        if (reset) begin
            // Reset all states and variables
            top_state <= IDLE;
            build_state <= INIT;
            sort_state <= S_INIT;
            root <= {($clog2(ARRAY_SIZE)+1){1'b1}}; ; // Null pointer
            next_free_node <= 0;
            input_index <= 0;
            done <= 0;

            // Clear tree arrays
            for (i = 0; i < ARRAY_SIZE; i = i + 1) begin
                keys[i*DATA_WIDTH +: DATA_WIDTH] <= 0;
                left_child[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}}; 
                right_child[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                left_stack[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                right_stack[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                left_sorted[i*DATA_WIDTH +: DATA_WIDTH] <= 0;
                right_sorted[i*DATA_WIDTH +: DATA_WIDTH] <= 0;
                sorted_out[i*DATA_WIDTH +: DATA_WIDTH] <= 0;
            end

        end 
        else begin
            case (top_state)
                IDLE: begin
                    done <= 0;
                    input_index <= 0;
                    root <= {($clog2(ARRAY_SIZE)+1){1'b1}}; ; // Null pointer
                    next_free_node <= 0;
                    for (i = 0; i < ARRAY_SIZE; i = i + 1) begin
                        keys[i*DATA_WIDTH +: DATA_WIDTH] <= 0;
                        left_child[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}}; 
                        right_child[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                        left_stack[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                        right_stack[i*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= {($clog2(ARRAY_SIZE)+1){1'b1}};
                        left_sorted[i*DATA_WIDTH +: DATA_WIDTH] <= 0;
                        right_sorted[i*DATA_WIDTH +: DATA_WIDTH] <= 0;
                        sorted_out[i*DATA_WIDTH +: DATA_WIDTH] <= 0;
                    end
                    if (start) begin
                        // Load input data into input array
                        top_state <= BUILD_TREE;
                        build_state <= INIT;
                        data_in_copy <= data_in;
                    end
                end

                BUILD_TREE: begin
                    case (build_state)
                        INIT: begin
                            if (input_index < ARRAY_SIZE) begin
                                temp_data <= data_in_copy[input_index*DATA_WIDTH +: DATA_WIDTH]; 
                                input_index <= input_index + 1;
                                build_state <= INSERT;
                            end else begin
                                build_state <= COMPLETE;
                            end
                        end

                        INSERT: begin
                            if (root == {($clog2(ARRAY_SIZE)+1){1'b1}}) begin
                                // Tree is empty, insert at root
                                root <= next_free_node;
                                keys[next_free_node*DATA_WIDTH +: DATA_WIDTH] <= temp_data;
                                next_free_node <= next_free_node + 1; 
                                build_state <= INIT;
                            end else begin
                                // Traverse the tree to find the correct position
                                current_node <= root; 
                                build_state <= TRAVERSE;
                            end
                        end

                        TRAVERSE: begin      
                            if ((temp_data < keys[current_node*DATA_WIDTH +: DATA_WIDTH]) || (temp_data == keys[current_node*DATA_WIDTH +: DATA_WIDTH])) begin
                                if (left_child[current_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] == {($clog2(ARRAY_SIZE)+1){1'b1}}) begin 
                                    left_child[current_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= next_free_node; 
                                    keys[next_free_node*DATA_WIDTH +: DATA_WIDTH] <= temp_data;
                                    next_free_node <= next_free_node + 1;
                                    build_state <= INIT;
                                end else begin
                                    current_node <= left_child[current_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]; 
                                end
                            end else begin
                                if (right_child[current_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] == {($clog2(ARRAY_SIZE)+1){1'b1}}) begin 
                                    right_child[current_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= next_free_node; 
                                    keys[next_free_node*DATA_WIDTH +: DATA_WIDTH] <= temp_data; 
                                    next_free_node <= next_free_node + 1;
                                    build_state <= INIT;
                                end else begin
                                    current_node <= right_child[current_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]; 
                                end
                            end
                        end

                        COMPLETE: begin
                            // Tree construction complete
                            top_state <= SORT_TREE;
                            sort_state <= S_INIT;
                        end

                    endcase
                end

                SORT_TREE: begin
                    case (sort_state)
                        S_INIT: begin
                            // Start parallel sorting for left and right subtrees
                            left_output_index <= 0;
                            right_output_index <= 0;
                            sp_left <= 0;
                            sp_right <= 0;
                            left_done <= 0;
                            right_done <= 0;
                            current_left_node <= left_child[root*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            current_right_node <= right_child[root*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            sort_state <= S_SORT_LEFT_RIGHT;
                        end

                        S_SORT_LEFT_RIGHT: begin
                            // Sort left subtree in parallel
                            if (!left_done && current_left_node != {($clog2(ARRAY_SIZE)+1){1'b1}}) begin
                                left_stack[sp_left*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= current_left_node;
                                sp_left <= sp_left + 1;
                                current_left_node <= left_child[current_left_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            end else if (!left_done && sp_left > 0) begin
                                sp_left <= sp_left - 1;
                                current_left_node <= left_stack[(sp_left - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                                left_sorted[left_output_index*DATA_WIDTH +: DATA_WIDTH] <= keys[left_stack[(sp_left - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*DATA_WIDTH +: DATA_WIDTH];
                                left_output_index <= left_output_index + 1;
                                current_left_node <= right_child[left_stack[(sp_left - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            end else if (!left_done) begin
                                left_done <= 1;
                            end

                            // Sort right subtree in parallel
                            if (!right_done && current_right_node != {($clog2(ARRAY_SIZE)+1){1'b1}}) begin
                                right_stack[sp_right*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)] <= current_right_node;
                                sp_right <= sp_right + 1;
                                current_right_node <= left_child[current_right_node*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            end else if (!right_done && sp_right > 0) begin
                                sp_right <= sp_right - 1;
                                current_right_node <= right_stack[(sp_right - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                                right_sorted[right_output_index*DATA_WIDTH +: DATA_WIDTH] <= keys[right_stack[(sp_right - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*DATA_WIDTH +: DATA_WIDTH];
                                right_output_index <= right_output_index + 1;
                                current_right_node <= right_child[right_stack[(sp_right - 1)*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)]*($clog2(ARRAY_SIZE)+1) +: ($clog2(ARRAY_SIZE)+1)];
                            end else if (!right_done) begin
                                right_done <= 1;
                            end

                            // Transition to merging once both left and right sorting are done
                            if (left_done && right_done) begin
                                sort_state <= S_MERGE_RESULTS;
                            end
                        end

                        S_MERGE_RESULTS: begin

                            // Merge left_sorted, root, and right_sorted into final sorted output
                            for (i = 0; i < ARRAY_SIZE; i = i + 1) begin
                                if (i < left_output_index) begin
                                    sorted_out[i*DATA_WIDTH +: DATA_WIDTH] <= left_sorted[i*DATA_WIDTH +: DATA_WIDTH];
                                end
                            end

                            // Insert the root into `sorted_out`
                            sorted_out[left_output_index*DATA_WIDTH +: DATA_WIDTH] <= keys[root*DATA_WIDTH +: DATA_WIDTH];

                            // Copy `right_sorted` into `sorted_out`
                            for (j = 0; j < ARRAY_SIZE; j = j + 1) begin
                                if (j < right_output_index) begin
                                    sorted_out[(left_output_index + 1 + j)*DATA_WIDTH +: DATA_WIDTH] <= right_sorted[j*DATA_WIDTH +: DATA_WIDTH];
                                end
                            end

                            done <= 1; // Sorting complete
                            top_state <= IDLE;

                        end

                        default: begin
                            sort_state <= S_INIT; // Reset to initial sort state
                        end
                    endcase
                end

                default: begin
                    top_state <= IDLE; // Default behavior for top-level FSM
                end
                
            endcase
        end
    end
endmodule
