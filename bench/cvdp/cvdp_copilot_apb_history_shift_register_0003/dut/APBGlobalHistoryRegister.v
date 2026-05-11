module APBGlobalHistoryRegister (
    // APB clock & reset
    input  wire         pclk,
    input  wire         presetn,  // Active-low reset

    // APB signals
    input  wire [9:0]   paddr,
    input  wire         pselx,
    input  wire         penable,
    input  wire         pwrite,
    input  wire [7:0]   pwdata,
    input  wire         history_shift_valid,
    input  wire         clk_gate_en,
    output reg          pready,
    output reg  [7:0]   prdata,
    output reg          pslverr,
    output reg          history_full,
    output reg          history_empty,
    output reg          error_flag,
    output reg          interrupt_full,
    output reg          interrupt_error
);

    //---------------------------------------------
    // Parameter Definitions
    //---------------------------------------------
    // Register address map
    localparam ADDR_CTRL_REG     = 10'h0;  // 0x0
    localparam ADDR_TRAIN_HIS    = 10'h1;  // 0x1
    localparam ADDR_PREDICT_HIS  = 10'h2;  // 0x2

    localparam WIDTH             = 8;

    //---------------------------------------------
    // Internal Registers (CSR)
    //---------------------------------------------
    reg [WIDTH-1:0] control_register;
    reg [WIDTH-1:0] train_history;
    reg [WIDTH-1:0] predict_history;
    //---------------------------------------------
    // Internal wires
    //---------------------------------------------
    wire        predict_valid;
    wire        predict_taken;
    wire        train_mispredicted;
    wire        train_taken;
    //---------------------------------------------
    // APB Read/Write Logic
    //---------------------------------------------
    wire apb_valid;
    assign apb_valid = pselx & penable;    // Indicates active APB transaction
    assign pclk_gated = !clk_gate_en&pclk;
    // By spec, no wait states => PREADY always high after reset
    always @(posedge pclk_gated or negedge presetn) begin
      if (!presetn) begin
        pready   <= 1'b0;
        pslverr  <= 1'b0;
      end else begin
        // PREADY is always asserted (no wait states) once out of reset
        pready   <= 1'b1;
        // If transaction is valid, check address range
        if (apb_valid) begin
          // Check if address is valid (0x0 through 0x2 are used, everything else => PSLVERR)
          if (paddr > ADDR_PREDICT_HIS) begin
            pslverr <= 1'b1;
          end
          else begin
            pslverr  <= 1'b0;
          end
        end
      end
    end

    // Handle writes to CSR or memory
    // Note: The design writes immediately in the cycle when penable=1.
    always @(posedge pclk_gated or negedge presetn) begin
      if (!presetn) begin
        // Reset all registers
        control_register  <= 0;
        train_history     <= 0;
      end else begin
        if (apb_valid && pwrite) begin
          case (paddr)
              ADDR_CTRL_REG:    control_register[3:0]  <= pwdata[3:0];
              ADDR_TRAIN_HIS:   train_history[6:0]     <= pwdata[6:0];
              // If the address is outside defined range => PSLVERR is set, no write
          endcase
        end
      end
    end

    // Handle read from CSR or memory
    always @(posedge pclk_gated or negedge presetn) begin
      if (!presetn) begin
        prdata <= 0;
      end 
      else begin
        if (apb_valid) begin
          case (paddr)
            ADDR_CTRL_REG:    prdata <= {4'b0,control_register[3:0]};
            ADDR_TRAIN_HIS:   prdata <= {1'b0,train_history[6:0]};
            ADDR_PREDICT_HIS: prdata <= predict_history;
            default:          prdata <= 0; // Invalid => PSLVERR, but can set prdata to 0
          endcase
        end
        else begin
          // When no valid read, clear prdata
          prdata <= 0;
        end
      end
    end


    //---------------------------------------------
    // GHSR Behavior
    //---------------------------------------------

    assign  predict_valid       = control_register[0];     // valid branch prediction
    assign  predict_taken       = control_register[1];     // predicted direction (1=taken, 0=not taken)
    assign  train_mispredicted  = control_register[2];     // branch misprediction occurred
    assign  train_taken         = control_register[3];     // actual branch direction for mispredicted branch



    always @(posedge history_shift_valid or negedge presetn) begin
      if (!presetn) begin
        // 1) active low Asynchronous reset
        //    Clear the entire history register.
        predict_history <= 0;
      end
      else begin
        // 2) Misprediction Handling (highest priority)
        //    If a misprediction is flagged, restore the old history from train_history
        //    and incorporate the correct outcome (train_taken) as the newest bit.
        if (train_mispredicted) begin
          predict_history <= {train_history[WIDTH-2:0], train_taken};
        end
        // 3) Normal Prediction Update
        //    If the prediction is valid and there is no misprediction,
        //    shift in predict_taken at the LSB (bit[0] is the youngest branch).
        else if (predict_valid) begin
          // "Shifting in from the LSB" while keeping the newest branch in predict_history[0]
          // is typically done by moving predict_history[31:1] up one bit
          // and placing predict_taken in bit[0].
          predict_history <= {predict_history[WIDTH-2:0], predict_taken};
        end
      end
    end
    
    always @(*) begin
      error_flag=pslverr;
      interrupt_error=pslverr;
      if(predict_history==8'hff) begin
        history_full=1'b1;
        interrupt_full=1'b1;
        history_empty=1'b0;
      end
      else if (predict_history==8'h00) begin
        history_full=1'b0;
        interrupt_full=1'b0;
        history_empty=1'b1;
      end
      else begin
        history_full=1'b0;
        interrupt_full=1'b0;
        history_empty=1'b0;
      end
    end

endmodule
