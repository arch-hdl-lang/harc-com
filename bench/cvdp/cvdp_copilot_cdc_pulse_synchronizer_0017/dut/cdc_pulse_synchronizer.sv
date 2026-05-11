module cdc_pulse_synchronizer (
    input  logic src_clock,   // Source Clock Domain
    input  logic des_clock,   // Destination Clock Domain
    input  logic rst_in,      // Reset
    input  logic src_pulse,   // Source Pulse
    output logic des_pulse    // Destination Pulse
);

    logic pls_toggle;      
    logic pls_toggle_synca;
    logic pls_toggle_syncb;
    logic pls_toggle_syncc;

    //--------------------------------------------------
    //   Toggle Flop Circuit
    //---------------------------------------------------

    always_ff @(posedge src_clock or posedge rst_in) begin
        if (rst_in) begin
            pls_toggle <= 1'b0;
        end else if (src_pulse) begin
            pls_toggle <= ~pls_toggle;
        end else begin
            pls_toggle <= pls_toggle;
        end
    end

    //--------------------------------------------------
    //   Double Flop Bit Synchronizer
    //---------------------------------------------------

    always_ff @(posedge des_clock or posedge rst_in) begin
        if (rst_in) begin
            pls_toggle_synca <= 1'b0;
            pls_toggle_syncb <= 1'b0;
        end else begin
            pls_toggle_synca <= pls_toggle;
            pls_toggle_syncb <= pls_toggle_synca;
        end
    end

    //--------------------------------------------------
    //   Delay Logic of Output signal
    //---------------------------------------------------

    always_ff @(posedge des_clock or posedge rst_in) begin
        if (rst_in) begin
            pls_toggle_syncc <= 1'b0;
        end else begin
            pls_toggle_syncc <= pls_toggle_syncb;
        end
    end

    //--------------------------------------------------
    //   Assign Statement for posedge and negedge detection
    //---------------------------------------------------

    assign des_pulse = pls_toggle_syncc ^ pls_toggle_syncb;

endmodule
