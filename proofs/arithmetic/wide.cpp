// Exact production word kernels are prepended by run.py.
// WORDS fixes storage capacity; every stored bit remains symbolic.
typedef __CPROVER::unsignedbv<PACK_BITS> Packed;
Packed pack(const uint32_t* words) {
    Packed result = 0;
    for (unsigned i = 0; i < WORDS; ++i)
        result |= static_cast<Packed>(words[i]) << (32 * i);
    return result;
}
int main() {
    uint32_t a[WORDS], b[WORDS], out[WORDS];
#if defined(PROOF_WIDE_ARITH)
    const Packed x = pack(a), y = pack(b);
    harc_word_add(out, a, b, WORDS);
    __CPROVER_assert(pack(out) == static_cast<Packed>(x + y), "word addition equals modular bitvector addition");
    harc_word_sub(out, a, b, WORDS);
    __CPROVER_assert(pack(out) == static_cast<Packed>(x - y), "word subtraction equals modular bitvector subtraction");
#elif defined(PROOF_WIDE_EXTEND)
    unsigned source_width, dest_width;
    __CPROVER_assume(source_width <= WORDS * 32);
    __CPROVER_assume(dest_width <= WORDS * 32);
    unsigned bit;
    __CPROVER_assume(bit < WORDS * 32);
    const bool original_a = ((a[bit / 32] >> (bit % 32)) & 1) != 0;
    const bool original_b = ((b[bit / 32] >> (bit % 32)) & 1) != 0;
    const bool sign = source_width != 0 &&
        ((a[(source_width - 1) / 32] >> ((source_width - 1) % 32)) & 1) != 0;
    // An arbitrary output bit is universally quantified. This checks the
    // entire vector without building a second 1024-bit barrel shifter.
    const bool expected = source_width != 0 && bit < dest_width &&
        (bit < source_width ? original_a : sign);
    harc_word_sext(a, WORDS, source_width, dest_width);
    __CPROVER_assert((((a[bit / 32] >> (bit % 32)) & 1) != 0) == expected,
        "every sign-extended output bit matches the mathematical definition");
    harc_word_mask(b, WORDS, dest_width);
    __CPROVER_assert((((b[bit / 32] >> (bit % 32)) & 1) != 0) == (bit < dest_width && original_b),
        "every masked output bit preserves data or clears padding");
#else
#error Select a proof
#endif
}
