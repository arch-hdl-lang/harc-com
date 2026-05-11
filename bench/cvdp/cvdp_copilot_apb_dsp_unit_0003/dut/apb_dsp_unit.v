module apb_dsp_unit (
    // APB clock & reset
    input  wire         pclk,
    input  wire         presetn,  // Active-low reset

    // APB signals
    input  wire [9:0]   paddr,
    input  wire         pselx,
    input  wire         penable,
    input  wire         pwrite,
    input  wire [7:0]   pwdata,
    input  wire         sram_valid,
    output reg          pready,
    output reg  [7:0]   prdata,
    output reg          pslverr
);

    //---------------------------------------------
    // Parameter Definitions
    //---------------------------------------------
    // Register address map
    localparam ADDR_OPERAND1     = 10'h0;  // 0x0
    localparam ADDR_OPERAND2     = 10'h1;  // 0x1
    localparam ADDR_ENABLE       = 10'h2;  // 0x2
    localparam ADDR_WRITE_ADDR   = 10'h3;  // 0x3
    localparam ADDR_WRITE_DATA   = 10'h4;  // 0x4
    localparam ADDR_RESULT       = 10'h5;  // 0x5 (used for storing DSP results)

    // DSP Modes
    localparam MODE_DISABLED     = 8'h0;   // DSP disabled
    localparam MODE_ADD          = 8'h1;   // Addition mode
    localparam MODE_MULT         = 8'h2;   // Multiplication mode
    localparam MODE_WRITE        = 8'h3;   // Data Writing mode

    // Memory size (1KB = 1024 bytes)
    localparam MEM_SIZE          = 1024;

    //---------------------------------------------
    // Internal Registers (CSR)
    //---------------------------------------------
    reg [7:0] r_operand_1;
    reg [7:0] r_operand_2;
    reg [7:0] r_enable;
    reg [7:0] r_write_address;
    reg [7:0] r_write_data;

    //---------------------------------------------
    // Memory (1 KB SRAM)
    //---------------------------------------------
    reg [7:0] sram_mem [0:MEM_SIZE-1];

    //---------------------------------------------
    // APB Read/Write Logic
    //---------------------------------------------
    wire apb_valid;
    assign apb_valid = pselx & penable;    // Indicates active APB transaction

    // By spec, no wait states => PREADY always high after reset
    always @(posedge pclk or negedge presetn) begin
        if (!presetn) begin
            pready   <= 1'b0;
            pslverr  <= 1'b0;
        end else begin
            // PREADY is always asserted (no wait states) once out of reset
            pready   <= 1'b1;
            pslverr  <= 1'b0;

            // If transaction is valid, check address range
            if (apb_valid) begin
                if(r_enable==MODE_WRITE) begin
                    if (paddr > MEM_SIZE) begin
                        pslverr <= 1'b1;
                    end
                end
                else begin
                    // Check if address is valid (0x0 through 0x5 are used, everything else => PSLVERR)
                    if (paddr > ADDR_RESULT) begin
                        pslverr <= 1'b1;
                    end
                end
            end
        end
    end

    // Handle writes to CSR or memory
    // Note: The design writes immediately in the cycle when penable=1.
    always @(posedge pclk or negedge presetn) begin
        if (!presetn) begin
            // Reset all registers
            r_operand_1    <= 8'h00;
            r_operand_2    <= 8'h00;
            r_enable       <= 8'h00;
            r_write_address<= 8'h00;
            r_write_data   <= 8'h00;
        end else begin
            if (apb_valid && pwrite) begin
                case (paddr)
                    ADDR_OPERAND1:     r_operand_1     <= pwdata;
                    ADDR_OPERAND2:     r_operand_2     <= pwdata;
                    ADDR_ENABLE:       r_enable        <= pwdata;
                    ADDR_WRITE_ADDR:   r_write_address <= pwdata;
                    ADDR_WRITE_DATA:   r_write_data    <= pwdata;
                    // If the address is outside defined range => PSLVERR is set, no write
                endcase
            end
        end
    end

    // Handle read from CSR or memory
    always @(posedge pclk or negedge presetn) begin
        if (!presetn) begin
            prdata <= 8'h00;
        end else begin
            if (apb_valid && !pwrite) begin
                if(r_enable==MODE_WRITE) begin
                    prdata <= sram_mem[paddr];
                end
                else begin
                    case (paddr)
                        ADDR_OPERAND1:     prdata <= r_operand_1;
                        ADDR_OPERAND2:     prdata <= r_operand_2;
                        ADDR_ENABLE:       prdata <= r_enable;
                        ADDR_WRITE_ADDR:   prdata <= r_write_address;
                        ADDR_WRITE_DATA:   prdata <= r_write_data;
                        ADDR_RESULT:       prdata <= sram_mem[ADDR_RESULT];  // Read the result from memory[0x5]
                        default:           prdata <= 8'h00; // Invalid => PSLVERR, but can set prdata to 0
                    endcase
                end
            end else if (!apb_valid) begin
                // When no valid read, clear prdata (optional behavior)
                prdata <= 8'h00;
            end
        end
    end

    //---------------------------------------------
    // SRAM write Behavior
    //---------------------------------------------
    always @(posedge sram_valid or negedge presetn) begin
        if (!presetn) begin
            // Initialize memory location for result
            sram_mem[ADDR_RESULT] <= 8'h00;
        end else begin
            // If r_Enable = WRITE (0x3), write r_write_data into memory[r_write_address]
            if (r_enable == MODE_WRITE) begin
                // Write data to memory at r_write_address
                sram_mem[r_write_address] <= r_write_data;
            end
        end
    end

    //---------------------------------------------
    // DSP Functional Behavior
    //---------------------------------------------
    always @(posedge pclk or negedge presetn) begin
        if (!presetn) begin
            // Initialize memory location for result
            sram_mem[ADDR_RESULT] <= 8'h00;
        end else begin
            // If r_Enable = ADD (0x1), add contents of memory[r_operand_1] and memory[r_operand_2]
            if (r_enable == MODE_ADD) begin
                sram_mem[ADDR_RESULT] <= sram_mem[r_operand_1] + sram_mem[r_operand_2];
            end
            // If r_Enable = MULT (0x2), multiply contents of memory[r_operand_1] and memory[r_operand_2]
            else if (r_enable == MODE_MULT) begin
                sram_mem[ADDR_RESULT] <= sram_mem[r_operand_1] * sram_mem[r_operand_2];
            end
            // If r_Enable = DISABLED (0x0), no operation
            // else do nothing
        end
    end

endmodule
