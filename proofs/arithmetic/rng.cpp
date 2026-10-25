// Independent mathematical contract for the production harc_rng_range template.
extern uint64_t nondet_u64();
extern int64_t nondet_i64();
extern signed char nondet_i8();
extern unsigned char nondet_u8();
extern bool nondet_bool();
struct Draw {
    uint64_t value;
    unsigned *calls;
    uint64_t operator()() { ++*calls; return value; }
};
int main() {
    int64_t lo, hi;
    uint64_t draw = nondet_u64();
#if defined(PROOF_RNG_FULL)
    lo = -INT64_MAX - 1;
    hi = INT64_MAX;
#elif defined(PROOF_RNG_HALF)
    bool negative = nondet_bool();
    lo = negative ? -INT64_MAX - 1 : 0;
    hi = negative ? -1 : INT64_MAX;
#elif defined(PROOF_RNG_SMALL)
    lo = nondet_i8();
    hi = nondet_i8();
    draw = nondet_u8();
#elif defined(PROOF_RNG_DEGENERATE)
    lo = nondet_i64();
    hi = nondet_i64();
    __CPROVER_assume(hi <= lo);
#else
#error select a range contract
#endif
    unsigned calls = 0;
    Draw next;
    next.value = draw;
    next.calls = &calls;
    int64_t result = harc_rng_range(next, lo, hi);
    if (hi <= lo) {
        __CPROVER_assert(result == lo, "degenerate range returns lower bound");
        __CPROVER_assert(calls == 0, "degenerate range draws nothing");
    } else {
        // Signed 128-bit subtraction/addition cannot overflow for int64_t
        // bounds. Cardinality 2^64 stays representable in this reference.
        __int128 cardinality = (__int128)hi - (__int128)lo + 1;
        __int128 expected = (__int128)lo + (__int128)((_harc_u128)draw % (_harc_u128)cardinality);
        __CPROVER_assert((__int128)result == expected, "result equals mathematical inclusive modulo mapping");
        __CPROVER_assert(result >= lo && result <= hi, "result belongs to requested interval");
        __CPROVER_assert(calls == 1, "ordered interval draws exactly once");
    }
}
