#include <cstdint>

extern "C" int64_t ref_neg() { return -8; }

extern "C" __uint128_t ref_wide() {
    return ((__uint128_t)0x123456789abcdef0ULL << 64) |
           (__uint128_t)0x0fedcba987654321ULL;
}
