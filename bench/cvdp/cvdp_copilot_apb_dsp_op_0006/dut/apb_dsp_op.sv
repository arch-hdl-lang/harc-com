// APB DSP Operation Module
module apb_dsp_op #(
    parameter ADDR_WIDTH = 'd8,
    parameter DATA_WIDTH = 'd32
) (
    input  logic                  clk_dsp,    // Faster clock to DSP operation
    input  logic                  en_clk_dsp, // Enable DSP operation with faster clock
    input  logic                  PCLK,       // APB clock
    input  logic                  PRESETn,    // Active low asynchronous APB Reset
    input  logic [ADDR_WIDTH-1:0] PADDR,      // APB address
    input  logic                  PWRITE,     // Write/Read enable
    input  logic [DATA_WIDTH-1:0] PWDATA,     // Write data
    input  logic                  PSEL,       // DSP selector
    input  logic                  PENABLE,    // APB enable
    output logic [DATA_WIDTH-1:0] PRDATA,     // Read data
    output logic                  PREADY,     // Ready signal
    output logic                  PSLVERR     // Error signal
);

    // Synchronize en_clk_dsp signal to clk_dsp domain to avoid CDC issues
    logic clk_dsp_sel;
    logic en_clk_dsp_ff;
    logic en_clk_dsp_sync_ff;

    always_ff @(posedge clk_dsp or negedge PRESETn) begin
        if (!PRESETn) begin
            en_clk_dsp_ff      <= 1'b0;
            en_clk_dsp_sync_ff <= 1'b0;
        end else begin
            en_clk_dsp_ff      <= en_clk_dsp;
            en_clk_dsp_sync_ff <= en_clk_dsp_ff;
        end
    end

    // Clock selection
    assign clk_dsp_sel = (en_clk_dsp_ff) ? clk_dsp : PCLK;

    // Internal registers address map
    localparam ADDRESS_A         = 32'h0;  // 0x00
    localparam ADDRESS_B         = 32'h4;  // 0x04
    localparam ADDRESS_C         = 32'h8;  // 0x08
    localparam ADDRESS_O         = 32'hC;  // 0x0C
    localparam ADDRESS_CONTROL   = 32'h10; // 0x10
    localparam ADDRESS_WDATA     = 32'h14; // 0x14
    localparam ADDRESS_SRAM_ADDR = 32'h18; // 0x18

    // Control modes
    localparam SRAM_WRITE     = 32'd1;
    localparam SRAM_READ      = 32'd2;
    localparam DSP_READ_OP_A  = 32'd3;
    localparam DSP_READ_OP_B  = 32'd4;
    localparam DSP_READ_OP_C  = 32'd5;
    localparam DSP_WRITE_OP_O = 32'd6;

    // Internal APB-side registers
    logic [DATA_WIDTH-1:0] reg_operand_a;
    logic [DATA_WIDTH-1:0] reg_operand_b;
    logic [DATA_WIDTH-1:0] reg_operand_c;
    logic [DATA_WIDTH-1:0] reg_operand_o;
    logic [DATA_WIDTH-1:0] reg_control;
    logic [DATA_WIDTH-1:0] reg_wdata_sram;
    logic [DATA_WIDTH-1:0] reg_addr_sram;

    // DSP-side pipeline regs and signals
    logic signed [DATA_WIDTH-1:0] dsp_op_a;
    logic signed [DATA_WIDTH-1:0] dsp_op_b;
    logic signed [DATA_WIDTH-1:0] dsp_op_c;
    logic signed [DATA_WIDTH-1:0] dsp_result;

    // SRAM signals
    logic [DATA_WIDTH-1:0] sram_data_in;
    logic                  sram_we;
    logic [DATA_WIDTH-1:0] sram_addr;
    logic [DATA_WIDTH-1:0] sram_data_out;
    logic [DATA_WIDTH-1:0] sram_data_out_sync_1;
    logic [DATA_WIDTH-1:0] sram_data_out_sync_2;
    
    // SRAM local memory
    logic [DATA_WIDTH-1:0] mem [63:0];

    // APB interface logic
    always_ff @(posedge PCLK or negedge PRESETn) begin
        if (!PRESETn) begin
            reg_operand_a  <= 'd0;
            reg_operand_b  <= 'd0;
            reg_operand_c  <= 'd0;
            reg_operand_o  <= 'd0;
            reg_control    <= 'd0;
            reg_wdata_sram <= 'd0;
            reg_addr_sram  <= 'd0;
            PRDATA         <= 'd0;
            PREADY         <= 1'b0;
            PSLVERR        <= 1'b0;
        end else if (PENABLE & PSEL) begin
            // Default value
            PREADY <= 1'b1;

            // Check addresses
            if (((reg_control == SRAM_WRITE) || (reg_control == DSP_WRITE_OP_O) || (reg_control == SRAM_READ)) && (sram_addr > 63)) begin
                PSLVERR <= 1'b1;
            end else begin
                case (PADDR)
                    ADDRESS_A         : PSLVERR <= 1'b0;
                    ADDRESS_B         : PSLVERR <= 1'b0;
                    ADDRESS_C         : PSLVERR <= 1'b0;
                    ADDRESS_O         : PSLVERR <= 1'b0;
                    ADDRESS_CONTROL   : PSLVERR <= 1'b0;
                    ADDRESS_WDATA     : PSLVERR <= 1'b0;
                    ADDRESS_SRAM_ADDR : PSLVERR <= 1'b0;
                    default           : PSLVERR <= 1'b1;
                endcase
            end

            // Internal register operation
            if (PWRITE) begin
                case (PADDR)
                    ADDRESS_A         : reg_operand_a  <= PWDATA;
                    ADDRESS_B         : reg_operand_b  <= PWDATA;
                    ADDRESS_C         : reg_operand_c  <= PWDATA;
                    ADDRESS_O         : reg_operand_o  <= PWDATA;
                    ADDRESS_CONTROL   : reg_control    <= PWDATA;
                    ADDRESS_WDATA     : reg_wdata_sram <= PWDATA;
                    ADDRESS_SRAM_ADDR : reg_addr_sram  <= PWDATA;
                endcase
            end else begin
                if (reg_control == SRAM_READ) begin
                    PRDATA <= sram_data_out_sync_2;
                end else begin
                    case (PADDR)
                        ADDRESS_A         : PRDATA  <= reg_operand_a;
                        ADDRESS_B         : PRDATA  <= reg_operand_b;
                        ADDRESS_C         : PRDATA  <= reg_operand_c;
                        ADDRESS_O         : PRDATA  <= reg_operand_o;
                        ADDRESS_CONTROL   : PRDATA  <= reg_control;
                        ADDRESS_WDATA     : PRDATA  <= reg_wdata_sram;
                        ADDRESS_SRAM_ADDR : PRDATA  <= reg_addr_sram;
                    endcase
                end               
            end
        end
    end

    // CDC synchronization for safe SRAM reads
    always_ff @(posedge clk_dsp_sel or negedge PRESETn) begin
        if (!PRESETn) begin
            sram_data_out_sync_1 <= 'd0;
            sram_data_out_sync_2 <= 'd0;
        end else begin
            sram_data_out_sync_1 <= sram_data_out;
            sram_data_out_sync_2 <= sram_data_out_sync_1;
        end        
    end

    // SRAM logic synchronization
    always_ff @(posedge clk_dsp_sel) begin
        sram_data_in <= (reg_control == SRAM_WRITE) ? reg_wdata_sram : dsp_result;

        if ((reg_control == SRAM_WRITE) || (reg_control == DSP_WRITE_OP_O)) begin
            sram_we <= 1'b1;
        end else begin
            sram_we <= 1'b0;
        end

        case (reg_control)
            DSP_READ_OP_A  : sram_addr <= reg_operand_a;
            DSP_READ_OP_B  : sram_addr <= reg_operand_b;
            DSP_READ_OP_C  : sram_addr <= reg_operand_c;
            DSP_WRITE_OP_O : sram_addr <= reg_operand_o;
            default        : sram_addr <= reg_addr_sram;
        endcase
    end

    // SRAM operation
    always_ff @(posedge clk_dsp_sel) begin
        if (sram_we) begin
            mem[sram_addr] <= sram_data_in;
        end else begin
            sram_data_out <= mem[sram_addr];
        end
    end
    
    // DSP operation
    always_ff @(posedge clk_dsp_sel) begin
        case (reg_control)
            DSP_READ_OP_A  : dsp_op_a <= sram_data_out;
            DSP_READ_OP_B  : dsp_op_b <= sram_data_out;
            DSP_READ_OP_C  : dsp_op_c <= sram_data_out;
        endcase
    end
    
    assign dsp_result = (dsp_op_a * dsp_op_b) + dsp_op_c;

endmodule