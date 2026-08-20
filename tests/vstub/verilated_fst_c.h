#pragma once
#include "verilated.h"
struct VerilatedFstC {
    void open(const char*) {}
    void close() {}
    void dump(uint64_t) {}
    void flush() {}
    bool isOpen() const { return false; }
};
