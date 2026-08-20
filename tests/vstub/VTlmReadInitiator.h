#pragma once
#include "verilated.h"
struct VTlmReadInitiator {
    CData clk = 0;
    CData rst = 0;
    CData mem_read_req_valid = 0;
    CData mem_read_req_ready = 0;
    IData mem_read_addr = 0;
    CData mem_read_rsp_valid = 0;
    CData mem_read_rsp_ready = 0;
    IData mem_read_rsp_data = 0;
    void eval() {}
    void final() {}
    template <typename T> void trace(T*, int) {}
    template <typename T> void trace(T*, int, int) {}
};
