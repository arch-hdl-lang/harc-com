// Reference model for tests/fixtures/extern_fn_ref_test.harc.
//
// Linked into the verilator-built TB via:
//     harc sim --ref-src tests/dut/extern_fn_ref.cpp ...
//
// HARC's codegen emits a matching `extern "C"` forward declaration at
// file scope (spec §9), so this symbol resolves at link time. The
// HARC source declares it as:
//     extern function ref_crc8_step(crc: uint<8>, byte: uint<8>) -> uint<8>
// which lowers to:
//     extern "C" uint64_t ref_crc8_step(uint64_t crc, uint64_t byte);
// HARC widens all narrow integer types to uint64_t at the FFI
// boundary, so this signature matches.

#include <cstdint>

extern "C" uint64_t ref_crc8_step(uint64_t crc, uint64_t byte) {
    // CRC-8/CCITT — polynomial 0x07, no reflection, no XOR-out.
    // Same algorithm the HARC-side `harc_crc8_step` implements,
    // so the two should produce identical outputs byte-for-byte.
    uint8_t x = static_cast<uint8_t>(crc) ^ static_cast<uint8_t>(byte);
    for (int i = 0; i < 8; ++i) {
        if (x & 0x80) {
            x = static_cast<uint8_t>((x << 1) ^ 0x07);
        } else {
            x = static_cast<uint8_t>(x << 1);
        }
    }
    return static_cast<uint64_t>(x);
}
