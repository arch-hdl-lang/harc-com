#pragma once
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
using vluint64_t = uint64_t;
using vluint32_t = uint32_t;
using CData = uint8_t;
using SData = uint16_t;
using IData = uint32_t;
using QData = uint64_t;
template <int W> struct VlWide {
    uint32_t m_storage[W]{};
    uint32_t& operator[](int i) { return m_storage[i]; }
    const uint32_t& operator[](int i) const { return m_storage[i]; }
    uint32_t* data() { return m_storage; }
    const uint32_t* data() const { return m_storage; }
};
struct VerilatedCovContext { void write(const char*) {} };
struct VerilatedContext {
    VerilatedCovContext* coveragep() { static VerilatedCovContext c; return &c; }
    void timeInc(uint64_t) {}
    uint64_t time() const { return 0; }
};
struct Verilated {
    static void commandArgs(int, char**) {}
    static void traceEverOn(bool) {}
    static VerilatedContext* threadContextp() { static VerilatedContext c; return &c; }
    static void mkdir(const char*) {}
};
