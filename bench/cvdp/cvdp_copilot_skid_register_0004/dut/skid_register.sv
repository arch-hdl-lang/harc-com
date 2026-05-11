module skid_register
  #(parameter DATA_WIDTH = 32)
(
    input  wire                     clk,
    input  wire                     rst,

    // Upstream (source) side
    input  wire [DATA_WIDTH-1:0]    up_bus,
    input  wire                     up_val,
    output reg                      up_rdy,

    // Downstream (sink) side
    output reg  [DATA_WIDTH-1:0]    dn_bus,
    output reg                      dn_val,
    input  wire                     dn_rdy
);

    // Internal regs/wires
    reg  [DATA_WIDTH-1:0]   skid_bus;   // Skid‐buffered data
    reg                     skid_val;   // True if skid_bus holds valid data
    wire                    dn_active;  // True if we can move data into dn_bus
    wire                    dn_val_i;   // The "next" valid going into dn_val
    wire [DATA_WIDTH-1:0]   dn_bus_i;   // The "next" data going into dn_bus

    // Downstream "active" logic
    assign dn_active = ~dn_val | dn_rdy;

    // Upstream ready logic
    always @(posedge clk) begin
        if (rst) begin
            up_rdy <= 1'b1; 
        end
        else begin
            up_rdy <= dn_active;
        end
    end

    // Skid‐bus and skid‐val tracking
    always @(posedge clk) begin
        if (rst) begin
            skid_val <= 1'b0;
        end 
        else begin
            skid_val <= dn_val_i & ~dn_active;
        end
    end

    always @(posedge clk) begin
        if (rst) begin
            skid_bus <= {DATA_WIDTH{1'b0}};
        end 
        else if (~dn_active & dn_val_i) begin
            skid_bus <= dn_bus_i;
        end
    end

    // Mux logic: direct or skid‐buffered
    assign dn_bus_i = up_rdy ? up_bus : skid_bus;
    assign dn_val_i = up_rdy ? up_val : skid_val;

    // Downstream valid/data
    always @(posedge clk) begin
        if (rst) begin
            dn_val <= 1'b0;
        end
        else if (dn_active) begin
            dn_val <= dn_val_i;
        end
    end

    always @(posedge clk) begin
        if (rst) begin
            dn_bus <= {DATA_WIDTH{1'b0}};
        end
        else if (dn_active) begin
            dn_bus <= dn_bus_i;
        end
    end

endmodule
