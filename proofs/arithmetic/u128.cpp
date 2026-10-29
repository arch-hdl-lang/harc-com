// Production functions are prepended verbatim by run.py.
int main() {
    _harc_u128 x, y;
    unsigned width;
    uint64_t shift;
    __CPROVER_assume(width <= 128);
    const _harc_u128 all = ~static_cast<_harc_u128>(0);
    const _harc_u128 mask = width == 0 ? static_cast<_harc_u128>(0) : all >> (128 - width);
#if defined(PROOF_MASK)
    __CPROVER_assert(harc_mask_u128(width) == mask, "mask equals low-width ones");
    __CPROVER_assert(harc_trunc_u128(x, width) == (x & mask), "truncation preserves precisely low bits");
    __CPROVER_assert(harc_negate_u128(x, width) == ((0 - x) & mask), "negation is modular subtraction");
#elif defined(PROOF_COMPARE)
    if (width == 0) {
        __CPROVER_assert(!harc_slt_u128(x, y, width), "empty vectors compare equal");
    } else {
        // Align the declared sign bit with bit 127, then bias signed order
        // into unsigned order. High storage padding is discarded by shift.
        const _harc_u128 sign = static_cast<_harc_u128>(1) << 127;
        const _harc_u128 a = (x << (128 - width)) ^ sign;
        const _harc_u128 b = (y << (128 - width)) ^ sign;
        __CPROVER_assert(harc_slt_u128(x, y, width) == (a < b), "signed order matches biased bitvectors");
    }
#elif defined(PROOF_SHIFT)
    const _harc_u128 value = x & mask;
    const bool negative = width != 0 && ((value >> (width - 1)) & 1);
    const _harc_u128 extended = value | (negative ? ~mask : static_cast<_harc_u128>(0));
    const _harc_u128 arithmetic = shift >= 128 ? (negative ? mask : static_cast<_harc_u128>(0))
        : static_cast<_harc_u128>(static_cast<__int128>(extended) >> shift) & mask;
    __CPROVER_assert(harc_ashr_u128(x, shift, width) == arithmetic, "arithmetic shift matches signed carrier");
    __CPROVER_assert(harc_shl_u128(x, shift, width) == (shift >= 128 ? static_cast<_harc_u128>(0) : (x << shift) & mask), "left shift truncates");
    __CPROVER_assert(harc_shr_u128(x, shift, width) == (shift >= 128 ? static_cast<_harc_u128>(0) : value >> shift), "logical shift zero fills");
#elif defined(PROOF_SEXT)
    unsigned dest_width, bit;
    __CPROVER_assume(dest_width <= 128);
    __CPROVER_assume(bit < 128);
    // Quantifying one arbitrary bit specifies the entire result, including
    // destination padding, without copying the production fill-mask formula.
    const bool data = ((x >> bit) & 1) != 0;
    const bool sign = width != 0 && ((x >> (width - 1)) & 1) != 0;
    const bool expected = width != 0 && bit < dest_width &&
        (bit < width ? data : sign);
    const _harc_u128 result = harc_sext_u128(x, width, dest_width);
    __CPROVER_assert((((result >> bit) & 1) != 0) == expected,
        "scalar sign extension preserves data, fills sign, and clears padding");
#elif defined(PROOF_SHIFT_COUNT)
    uint64_t limit;
    const uint64_t expected = x >= static_cast<_harc_u128>(limit)
        ? limit : static_cast<uint64_t>(x);
    __CPROVER_assert(harc_u128_shift_count(x, limit) == expected,
        "128-bit shift counts clamp without losing high-bit magnitude");
    const uint64_t low = static_cast<uint64_t>(x);
    __CPROVER_assert(harc_u64_shift_count(low, limit) == (low >= limit ? limit : low),
        "64-bit shift counts clamp at every limit including zero");
#elif defined(PROOF_DIVZERO)
    __CPROVER_assert(harc_sdiv_u128(x, 0, width) == 0, "zero-divisor quotient policy at every width");
    __CPROVER_assert(harc_smod_u128(x, 0, width) == (x & mask), "zero-divisor remainder policy at every width");
#elif defined(PROOF_DIVREM)
    // Fixed-width native signed division is an independent specification.
    typedef __CPROVER::unsignedbv<CHECK_WIDTH> U;
    typedef __CPROVER::signedbv<CHECK_WIDTH> S;
    const U a = static_cast<U>(x), b = static_cast<U>(y);
    const U min = static_cast<U>(1) << (CHECK_WIDTH - 1);
    const U minus_one = ~static_cast<U>(0);
    _harc_u128 quotient, remainder;
    if (b == 0) {
        quotient = 0; remainder = a;
    } else if (a == min && b == minus_one) {
        quotient = min; remainder = 0; // modular overflow policy
    } else {
        quotient = static_cast<U>(static_cast<S>(a) / static_cast<S>(b));
        remainder = static_cast<U>(static_cast<S>(a) % static_cast<S>(b));
    }
    __CPROVER_assert(harc_sdiv_u128(x, y, CHECK_WIDTH) == quotient, "signed division including zero and modular overflow");
    __CPROVER_assert(harc_smod_u128(x, y, CHECK_WIDTH) == remainder, "signed remainder follows dividend");
#else
#error Select a proof
#endif
}
