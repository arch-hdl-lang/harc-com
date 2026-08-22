#pragma once
#include <cstdint>
struct VTop {
    uint8_t clk = 0;
    uint8_t rst = 0;
    uint8_t en = 0;
    uint32_t count_out = 0;
    void eval() {}
    void final() {}
};
