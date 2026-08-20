#pragma once
#include "verilated.h"
struct VAxiLiteRegs {
    CData clk=0, rst=0;
    CData axil_ar_valid=0, axil_aw_valid=0, axil_r_ready=0, axil_r_valid=0;
    CData axil_w_ready=0, axil_w_valid=0, axil_w_strb=0, axil_r_resp=0;
    IData axil_ar_addr=0, axil_aw_addr=0, axil_w_data=0;
    IData axil_r_data=0;
    CData axil_b_valid=0, axil_b_ready=0, axil_b_resp=0, axil_aw_ready=0, axil_ar_ready=0;
    void eval() {}
    void final() {}
};
