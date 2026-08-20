// harc_thread_rt.h — Coroutine runtime for HARC test simulation.
//
// Slim port of arch-com's `arch_thread_rt.h`. The two runtimes are
// independent (HARC owns its build artifacts and doesn't link against
// arch's runtime), but the cooperative-scheduler model is identical so
// HARC tests can interoperate with arch DUTs that drop in their own
// thread-sim later without two competing schedulers in one process.
//
// Phase 1 (this file):
//   - Single OS thread, single clock domain.
//   - Each `run` block (and later: each `driver` / `agent` / `monitor`)
//     compiles to a C++20 coroutine returning `HarcThread`.
//   - One `ThreadScheduler` per test instance owns all coroutines.
//   - Awaiters:
//       * `wait_until(slot, pred)`  — suspend; resume on next posedge
//                                      where pred() returns true.
//       * `wait_cycles(slot, N)`    — suspend; resume after N posedges
//                                      (N>=1).
//
// Out of Phase 1 scope: fork/join, resource locks, multi-thread
// scheduling on multiple OS threads, cross-domain clocks. Those land
// in subsequent phases under the same surface so test source doesn't
// shift when the runtime grows up.
//
// Why match arch's runtime structure: HARC is the sister verification
// language. When HARC drivers/monitors bind to ARCH `bus` types they
// will eventually run alongside ARCH-emitted threads in the same
// process. Keeping the two runtimes structurally identical (same slot
// states, same tick-pass semantics) means later merging them into a
// shared runtime is a structural rename, not a semantic redesign.

#pragma once

#include <coroutine>
#include <functional>
#include <cstdint>
#include <initializer_list>
#include <type_traits>
#include <vector>
#include <atomic>
#include <thread>
#include <array>

// 65–128-bit integer type for whole-signal arithmetic and >64-bit
// hex literals. File-scope (not inside `harc_rt::`) so emitted code
// can reference it bare. Mirrors arch-com's `_arch_u128` (see
// arch-com/src/sim_codegen/mod.rs:767).
typedef unsigned __int128 _harc_u128;

namespace harc_rt {

// ── Wide-vector interop ─────────────────────────────────────────────────────
//
// Verilator lowers SystemVerilog ports by bit-width:
//   1..32 bits   → uint8_t / uint16_t / uint32_t
//   33..64 bits  → uint64_t
//   >64 bits     → VlWide<N>  (array of N uint32_t words, N=ceil(W/32))
//
// HARC's testbench source language doesn't have separate "narrow"/"wide"
// types — every integer expression flows through the same shape. Narrow
// values use native integers / `_harc_u128`; wider vector values use
// `HarcWide<N>`, a small LSB-first 32-bit word array. This is deliberately
// sized by the compiler from the HARC type and supports the language-level
// vector target of up to 1024 bits (N <= 32).
//
// Mirrors arch-com's `_arch_u128` design (see arch-com src/sim_codegen/
// mod.rs:767 for the equivalent typedef and conversion helpers). The
// `_harc_u128` typedef is at file scope above; the helpers below use
// it for the value type so codegen can emit `_harc_u128` literals
// without namespace qualification.

template<std::size_t N>
struct HarcWide {
    std::array<uint32_t, N> words{};

    HarcWide() = default;

    template<typename T, typename = std::enable_if_t<std::is_integral_v<T> || std::is_enum_v<T>>>
    HarcWide(T v) {
        _harc_u128 u = static_cast<_harc_u128>(v);
        for (std::size_t i = 0; i < N; ++i) {
            words[i] = (i < 4) ? static_cast<uint32_t>(u >> (32 * i)) : 0u;
        }
    }

    HarcWide(std::initializer_list<uint32_t> init) {
        std::size_t i = 0;
        for (uint32_t w : init) {
            if (i < N) words[i] = w;
            ++i;
        }
    }

    // Widening conversion between word counts: a value is sized by its own
    // width, its destination slot by the declared width of the local it
    // lands in, and the two legitimately differ (`let b : uint<256> =
    // a.zext<200>()`, or a plain `uint<130>` value assigned into a
    // `uint<256>` local). Copy the source words and zero-fill the rest.
    // Narrowing stays a compile error: dropping words is only correct
    // behind an explicit `harc_wide_trunc`, which masks to a stated width.
    template<std::size_t M, typename = std::enable_if_t<(M < N)>>
    HarcWide(const HarcWide<M>& other) {
        for (std::size_t i = 0; i < M; ++i) words[i] = other.words[i];
    }

    uint32_t operator[](std::size_t i) const { return i < N ? words[i] : 0u; }
    uint32_t& operator[](std::size_t i) { return words[i]; }

    operator uint64_t() const {
        uint64_t v = 0;
        if constexpr (N >= 1) v |= static_cast<uint64_t>(words[0]);
        if constexpr (N >= 2) v |= static_cast<uint64_t>(words[1]) << 32;
        return v;
    }

    operator _harc_u128() const {
        _harc_u128 v = 0;
        if constexpr (N >= 1) v |= static_cast<_harc_u128>(words[0]);
        if constexpr (N >= 2) v |= static_cast<_harc_u128>(words[1]) << 32;
        if constexpr (N >= 3) v |= static_cast<_harc_u128>(words[2]) << 64;
        if constexpr (N >= 4) v |= static_cast<_harc_u128>(words[3]) << 96;
        return v;
    }
};

template<std::size_t N>
inline HarcWide<N> harc_wide_from_binary(const char* bits) {
    HarcWide<N> v;
    if (!bits) return v;
    std::size_t len = 0;
    while (bits[len] != '\0') ++len;
    for (std::size_t src = 0; src < len; ++src) {
        const char ch = bits[len - 1 - src];
        if (ch != '1') continue;
        const std::size_t word = src / 32;
        const std::size_t bit = src % 32;
        if (word < N) v.words[word] |= (uint32_t{1} << bit);
    }
    return v;
}

template<typename T> struct is_harc_wide : std::false_type {};
template<std::size_t N> struct is_harc_wide<HarcWide<N>> : std::true_type {};
template<typename T>
inline constexpr bool is_harc_wide_v = is_harc_wide<std::remove_cv_t<std::remove_reference_t<T>>>::value;
template<typename T> struct harc_wide_words;
template<std::size_t N> struct harc_wide_words<HarcWide<N>> { static constexpr std::size_t value = N; };

// Marker trait for co-sim accessor proxies (harc_cosim_rt.h
// specializes it). Used instead of `is_convertible_v<Sig, uint64_t>`
// in the signal helpers: convertibility is broader than "proxy" —
// HarcWide itself converts to uint64_t, and keying the scalar-read
// path on convertibility would silently truncate a future wide call
// site.
template<typename T> struct harc_is_accessor_proxy : std::false_type {};
template<typename T>
inline constexpr bool harc_is_accessor_proxy_v =
    harc_is_accessor_proxy<std::remove_cv_t<std::remove_reference_t<T>>>::value;


template<typename T>
inline long long harc_printf_ll(const T& v) {
    if constexpr (is_harc_wide_v<T>) {
        return static_cast<long long>(static_cast<uint64_t>(v));
    } else {
        return static_cast<long long>(v);
    }
}

template<std::size_t A, std::size_t B>
inline bool operator==(const HarcWide<A>& lhs, const HarcWide<B>& rhs) {
    constexpr std::size_t M = (A > B) ? A : B;
    for (std::size_t i = 0; i < M; ++i) {
        const uint32_t l = (i < A) ? lhs.words[i] : 0u;
        const uint32_t r = (i < B) ? rhs.words[i] : 0u;
        if (l != r) return false;
    }
    return true;
}

template<std::size_t A, std::size_t B>
inline bool operator!=(const HarcWide<A>& lhs, const HarcWide<B>& rhs) {
    return !(lhs == rhs);
}

template<std::size_t N, typename T, typename = std::enable_if_t<std::is_integral_v<T> || std::is_enum_v<T>>>
inline bool operator==(const HarcWide<N>& lhs, T rhs) {
    return lhs == HarcWide<N>(rhs);
}

template<std::size_t N, typename T, typename = std::enable_if_t<std::is_integral_v<T> || std::is_enum_v<T>>>
inline bool operator==(T lhs, const HarcWide<N>& rhs) {
    return HarcWide<N>(lhs) == rhs;
}

template<std::size_t N, typename T, typename = std::enable_if_t<std::is_integral_v<T> || std::is_enum_v<T>>>
inline bool operator!=(const HarcWide<N>& lhs, T rhs) {
    return !(lhs == rhs);
}

template<std::size_t N, typename T, typename = std::enable_if_t<std::is_integral_v<T> || std::is_enum_v<T>>>
inline bool operator!=(T lhs, const HarcWide<N>& rhs) {
    return !(lhs == rhs);
}

template<std::size_t N>
inline HarcWide<N> harc_wide_mask_bits(HarcWide<N> value, unsigned width) {
    const unsigned total = static_cast<unsigned>(N * 32);
    if (width >= total) return value;
    const unsigned keep_words = width / 32;
    const unsigned keep_bits = width % 32;
    for (std::size_t i = keep_words + (keep_bits ? 1 : 0); i < N; ++i) value.words[i] = 0;
    if (keep_words < N) {
        if (keep_bits == 0) {
            for (std::size_t i = keep_words; i < N; ++i) value.words[i] = 0;
        } else {
            value.words[keep_words] &= (uint32_t{1} << keep_bits) - 1u;
        }
    }
    return value;
}

template<std::size_t N, typename T>
inline HarcWide<N> harc_wide_zext(T value) {
    return HarcWide<N>(value);
}

template<std::size_t N, std::size_t M>
inline HarcWide<N> harc_wide_zext(const HarcWide<M>& value) {
    HarcWide<N> out;
    constexpr std::size_t C = (N < M) ? N : M;
    for (std::size_t i = 0; i < C; ++i) out.words[i] = value.words[i];
    return out;
}

template<std::size_t N, typename T>
inline HarcWide<N> harc_wide_zext(T value, unsigned source_width) {
    return harc_wide_mask_bits(harc_wide_zext<N>(value), source_width);
}

template<std::size_t N, typename T>
inline HarcWide<N> harc_wide_trunc(T value, unsigned width) {
    return harc_wide_mask_bits(HarcWide<N>(value), width);
}

template<std::size_t N, std::size_t M>
inline HarcWide<N> harc_wide_trunc(const HarcWide<M>& value, unsigned width) {
    return harc_wide_mask_bits(harc_wide_zext<N>(value), width);
}

template<std::size_t N, typename T>
inline HarcWide<N> harc_wide_sext(T value, unsigned source_width, unsigned dest_width) {
    HarcWide<N> out = harc_wide_zext<N>(value);
    out = harc_wide_mask_bits(out, source_width);
    if (source_width == 0 || dest_width == 0) return HarcWide<N>();
    const unsigned sign_bit = source_width - 1;
    const std::size_t sign_word = sign_bit / 32;
    const unsigned sign_off = sign_bit % 32;
    const bool neg = sign_word < N && ((out.words[sign_word] >> sign_off) & 1u);
    if (!neg) return harc_wide_mask_bits(out, dest_width);
    for (unsigned bit = source_width; bit < dest_width && bit < N * 32; ++bit) {
        out.words[bit / 32] |= uint32_t{1} << (bit % 32);
    }
    return harc_wide_mask_bits(out, dest_width);
}

template<std::size_t N>
inline bool harc_wide_get_bit(const HarcWide<N>& v, unsigned bit) {
    return bit < N * 32 && ((v.words[bit / 32] >> (bit % 32)) & 1u);
}

template<std::size_t N>
inline void harc_wide_set_bit(HarcWide<N>& v, unsigned bit) {
    if (bit < N * 32) v.words[bit / 32] |= uint32_t{1} << (bit % 32);
}

template<std::size_t N>
inline void harc_wide_clear_bit(HarcWide<N>& v, unsigned bit) {
    if (bit < N * 32) v.words[bit / 32] &= ~(uint32_t{1} << (bit % 32));
}

template<std::size_t N, typename Val>
inline void harc_wide_write_bits(HarcWide<N>& dst, unsigned lo, unsigned width, const Val& value) {
    for (unsigned b = 0; b < width && lo + b < N * 32; ++b) {
        bool bit = false;
        if constexpr (is_harc_wide_v<std::remove_cv_t<std::remove_reference_t<Val>>>) {
            bit = harc_wide_get_bit(value, b);
        } else {
            const _harc_u128 raw = static_cast<_harc_u128>(value);
            bit = b < 128 && ((raw >> b) & 1u);
        }
        if (bit) {
            harc_wide_set_bit(dst, lo + b);
        } else {
            harc_wide_clear_bit(dst, lo + b);
        }
    }
}

// Extract an arbitrary bit range into a width-sized wide carrier. Unlike
// `harc_bits`, this never truncates the result to 64 bits. Bits outside the
// requested range and storage padding in the final word are always zero.
template<std::size_t OutWords, std::size_t InWords>
inline HarcWide<OutWords> harc_wide_extract_bits(
    const HarcWide<InWords>& value, unsigned lo, unsigned width) {
    HarcWide<OutWords> out;
    const unsigned capped = width < OutWords * 32 ? width : OutWords * 32;
    for (unsigned b = 0; b < capped; ++b) {
        if (harc_wide_get_bit(value, lo + b)) harc_wide_set_bit(out, b);
    }
    return harc_wide_mask_bits(out, capped);
}

template<std::size_t OutWords>
inline HarcWide<OutWords> harc_wide_extract_bits(
    _harc_u128 value, unsigned lo, unsigned width) {
    return harc_wide_extract_bits<OutWords>(harc_wide_zext<4>(value), lo, width);
}

template<std::size_t N>
inline HarcWide<N> operator+(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    HarcWide<N> out;
    uint64_t carry = 0;
    for (std::size_t i = 0; i < N; ++i) {
        const uint64_t sum = static_cast<uint64_t>(lhs.words[i]) + rhs.words[i] + carry;
        out.words[i] = static_cast<uint32_t>(sum);
        carry = sum >> 32;
    }
    return out;
}

template<std::size_t N>
inline HarcWide<N> operator-(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    HarcWide<N> out;
    uint64_t borrow = 0;
    for (std::size_t i = 0; i < N; ++i) {
        const uint64_t l = lhs.words[i];
        const uint64_t r = static_cast<uint64_t>(rhs.words[i]) + borrow;
        out.words[i] = static_cast<uint32_t>(l - r);
        borrow = l < r ? 1 : 0;
    }
    return out;
}

template<std::size_t N>
inline HarcWide<N> operator*(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    HarcWide<N> out;
    for (std::size_t i = 0; i < N; ++i) {
        uint64_t carry = 0;
        for (std::size_t j = 0; j + i < N; ++j) {
            const uint64_t cur = static_cast<uint64_t>(out.words[i + j])
                + static_cast<uint64_t>(lhs.words[i]) * rhs.words[j]
                + carry;
            out.words[i + j] = static_cast<uint32_t>(cur);
            carry = cur >> 32;
        }
    }
    return out;
}

template<std::size_t N>
inline HarcWide<N> operator&(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    HarcWide<N> out;
    for (std::size_t i = 0; i < N; ++i) out.words[i] = lhs.words[i] & rhs.words[i];
    return out;
}

template<std::size_t N>
inline HarcWide<N> operator|(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    HarcWide<N> out;
    for (std::size_t i = 0; i < N; ++i) out.words[i] = lhs.words[i] | rhs.words[i];
    return out;
}

template<std::size_t N>
inline HarcWide<N> operator^(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    HarcWide<N> out;
    for (std::size_t i = 0; i < N; ++i) out.words[i] = lhs.words[i] ^ rhs.words[i];
    return out;
}

template<std::size_t N>
inline HarcWide<N> operator~(const HarcWide<N>& value) {
    HarcWide<N> out;
    for (std::size_t i = 0; i < N; ++i) out.words[i] = ~value.words[i];
    return out;
}

template<std::size_t N, typename S, typename = std::enable_if_t<std::is_integral_v<S>>>
inline HarcWide<N> operator<<(const HarcWide<N>& value, S shift_raw) {
    HarcWide<N> out;
    if constexpr (std::is_signed_v<S>) {
        if (shift_raw < 0) return out;
    }
    if (static_cast<_harc_u128>(shift_raw) >= N * 32) return out;
    const unsigned shift = static_cast<unsigned>(shift_raw);
    const unsigned word_shift = shift / 32;
    const unsigned bit_shift = shift % 32;
    for (std::size_t i = word_shift; i < N; ++i) {
        uint64_t part = static_cast<uint64_t>(value.words[i - word_shift]) << bit_shift;
        out.words[i] |= static_cast<uint32_t>(part);
        if (bit_shift && i + 1 < N) out.words[i + 1] |= static_cast<uint32_t>(part >> 32);
    }
    return out;
}

template<std::size_t N, typename S, typename = std::enable_if_t<std::is_integral_v<S>>>
inline HarcWide<N> operator>>(const HarcWide<N>& value, S shift_raw) {
    HarcWide<N> out;
    if constexpr (std::is_signed_v<S>) {
        if (shift_raw < 0) return out;
    }
    if (static_cast<_harc_u128>(shift_raw) >= N * 32) return out;
    const unsigned shift = static_cast<unsigned>(shift_raw);
    const unsigned word_shift = shift / 32;
    const unsigned bit_shift = shift % 32;
    for (std::size_t i = 0; i + word_shift < N; ++i) {
        uint64_t part = value.words[i + word_shift];
        if (bit_shift && i + word_shift + 1 < N) part |= static_cast<uint64_t>(value.words[i + word_shift + 1]) << 32;
        out.words[i] = static_cast<uint32_t>(part >> bit_shift);
    }
    return out;
}

template<std::size_t N>
inline bool operator<(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    for (std::size_t i = N; i > 0; --i) {
        if (lhs.words[i - 1] != rhs.words[i - 1]) return lhs.words[i - 1] < rhs.words[i - 1];
    }
    return false;
}

template<std::size_t N>
inline bool operator>(const HarcWide<N>& lhs, const HarcWide<N>& rhs) { return rhs < lhs; }

template<std::size_t N>
inline bool operator<=(const HarcWide<N>& lhs, const HarcWide<N>& rhs) { return !(rhs < lhs); }

template<std::size_t N>
inline bool operator>=(const HarcWide<N>& lhs, const HarcWide<N>& rhs) { return !(lhs < rhs); }

template<std::size_t N>
inline bool harc_wide_is_zero(const HarcWide<N>& value) {
    for (uint32_t word : value.words) if (word != 0) return false;
    return true;
}

inline uint64_t harc_u64_shift_count(uint64_t value, uint64_t limit) {
    return value >= limit ? limit : value;
}

inline uint64_t harc_u128_shift_count(_harc_u128 value, uint64_t limit) {
    if ((value >> 64) != 0) return limit;
    return harc_u64_shift_count(static_cast<uint64_t>(value), limit);
}

template<std::size_t N>
inline uint64_t harc_wide_shift_count(const HarcWide<N>& value, uint64_t limit) {
    for (std::size_t i = 2; i < N; ++i) {
        if (value.words[i] != 0) return limit;
    }
    uint64_t low = value.words[0];
    if constexpr (N > 1) low |= static_cast<uint64_t>(value.words[1]) << 32;
    return harc_u64_shift_count(low, limit);
}

template<std::size_t N>
inline bool harc_wide_slt(HarcWide<N> lhs, HarcWide<N> rhs, unsigned width) {
    lhs = harc_wide_mask_bits(lhs, width);
    rhs = harc_wide_mask_bits(rhs, width);
    if (width == 0) return false;
    const bool lhs_negative = harc_wide_get_bit(lhs, width - 1);
    const bool rhs_negative = harc_wide_get_bit(rhs, width - 1);
    if (lhs_negative != rhs_negative) return lhs_negative;
    return lhs < rhs;
}

template<std::size_t N>
inline HarcWide<N> harc_wide_divmod(const HarcWide<N>& lhs, const HarcWide<N>& rhs, HarcWide<N>* rem_out) {
    HarcWide<N> q;
    HarcWide<N> r;
    if (harc_wide_is_zero(rhs)) {
        if (rem_out) *rem_out = lhs;
        return q;
    }
    for (unsigned bit = static_cast<unsigned>(N * 32); bit > 0; --bit) {
        r = r << 1;
        if (harc_wide_get_bit(lhs, bit - 1)) r.words[0] |= 1u;
        if (r >= rhs) {
            r = r - rhs;
            harc_wide_set_bit(q, bit - 1);
        }
    }
    if (rem_out) *rem_out = r;
    return q;
}

template<std::size_t N>
inline HarcWide<N> operator/(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    return harc_wide_divmod(lhs, rhs, static_cast<HarcWide<N>*>(nullptr));
}

template<std::size_t N>
inline HarcWide<N> operator%(const HarcWide<N>& lhs, const HarcWide<N>& rhs) {
    HarcWide<N> r;
    (void)harc_wide_divmod(lhs, rhs, &r);
    return r;
}

template<std::size_t N>
inline HarcWide<N> harc_wide_negate(HarcWide<N> value, unsigned width) {
    value = harc_wide_mask_bits(value, width);
    return harc_wide_mask_bits((~value) + HarcWide<N>(1), width);
}

template<std::size_t N>
inline HarcWide<N> harc_wide_sdiv(HarcWide<N> lhs, HarcWide<N> rhs, unsigned width) {
    lhs = harc_wide_mask_bits(lhs, width);
    rhs = harc_wide_mask_bits(rhs, width);
    if (harc_wide_is_zero(rhs) || width == 0) return HarcWide<N>();
    const bool lhs_negative = harc_wide_get_bit(lhs, width - 1);
    const bool rhs_negative = harc_wide_get_bit(rhs, width - 1);
    if (lhs_negative) lhs = harc_wide_negate(lhs, width);
    if (rhs_negative) rhs = harc_wide_negate(rhs, width);
    HarcWide<N> quotient = lhs / rhs;
    return lhs_negative != rhs_negative ? harc_wide_negate(quotient, width) : quotient;
}

template<std::size_t N>
inline HarcWide<N> harc_wide_smod(HarcWide<N> lhs, HarcWide<N> rhs, unsigned width) {
    lhs = harc_wide_mask_bits(lhs, width);
    rhs = harc_wide_mask_bits(rhs, width);
    if (harc_wide_is_zero(rhs) || width == 0) return lhs;
    const bool lhs_negative = harc_wide_get_bit(lhs, width - 1);
    const bool rhs_negative = harc_wide_get_bit(rhs, width - 1);
    if (lhs_negative) lhs = harc_wide_negate(lhs, width);
    if (rhs_negative) rhs = harc_wide_negate(rhs, width);
    HarcWide<N> remainder = lhs % rhs;
    return lhs_negative ? harc_wide_negate(remainder, width) : remainder;
}

template<typename Sig, typename Val>
inline void harc_assign(Sig& sig, Val val) {
    if constexpr (std::is_assignable_v<Sig&, Val>) {
        sig = val;
    } else if constexpr (std::is_arithmetic_v<Sig>) {
        if constexpr (is_harc_wide_v<Val>) {
            sig = static_cast<Sig>(static_cast<uint64_t>(val));
        } else {
            sig = static_cast<Sig>(val);
        }
    } else {
        // VlWide<N>: write low words and zero anything beyond.
        constexpr std::size_t N = sizeof(Sig) / sizeof(uint32_t);
        if constexpr (is_harc_wide_v<Val>) {
            constexpr std::size_t M = harc_wide_words<std::remove_cv_t<std::remove_reference_t<Val>>>::value;
            for (std::size_t i = 0; i < N; ++i) sig[i] = (i < M) ? val.words[i] : 0u;
        } else {
            const _harc_u128 v = static_cast<_harc_u128>(val);
            for (std::size_t i = 0; i < N; ++i) {
                sig[i] = (i < 4) ? static_cast<uint32_t>(v >> (32 * i)) : 0u;
            }
        }
    }
}

template<typename Sig>
inline auto harc_read(const Sig& sig) {
    if constexpr (std::is_arithmetic_v<Sig>) {
        return static_cast<_harc_u128>(sig);
    } else if constexpr (harc_is_accessor_proxy_v<Sig>) {
        // Co-sim accessor proxy (harc_cosim_rt.h SigProxy): a <= 64-bit
        // scalar behind a DPI accessor, not a word-array wide.
        return static_cast<_harc_u128>(static_cast<uint64_t>(sig));
    } else {
        constexpr std::size_t N = sizeof(Sig) / sizeof(uint32_t);
        HarcWide<N> v;
        for (std::size_t i = 0; i < N; ++i) v.words[i] = static_cast<uint32_t>(sig[i]);
        return v;
    }
}

inline uint64_t harc_bits(_harc_u128 value, uint32_t hi, uint32_t lo) {
    if (hi < lo) return 0;
    if (lo >= 128) return 0;
    const uint32_t width = hi - lo + 1;
    const _harc_u128 shifted = value >> lo;
    if (width >= 64) return static_cast<uint64_t>(shifted);
    const _harc_u128 mask = (static_cast<_harc_u128>(1) << width) - 1;
    return static_cast<uint64_t>(shifted & mask);
}

inline _harc_u128 harc_mask_u128(unsigned width) {
    if (width >= 128) return ~static_cast<_harc_u128>(0);
    if (width == 0) return 0;
    return (static_cast<_harc_u128>(1) << width) - 1;
}

inline _harc_u128 harc_negate_u128(_harc_u128 value, unsigned width) {
    return (~value + 1) & harc_mask_u128(width);
}

inline _harc_u128 harc_sdiv_u128(_harc_u128 lhs, _harc_u128 rhs, unsigned width) {
    const _harc_u128 mask = harc_mask_u128(width);
    lhs &= mask;
    rhs &= mask;
    if (rhs == 0 || width == 0) return 0;
    const bool lhs_negative = ((lhs >> (width - 1)) & 1u) != 0;
    const bool rhs_negative = ((rhs >> (width - 1)) & 1u) != 0;
    if (lhs_negative) lhs = harc_negate_u128(lhs, width);
    if (rhs_negative) rhs = harc_negate_u128(rhs, width);
    const _harc_u128 quotient = lhs / rhs;
    return lhs_negative != rhs_negative ? harc_negate_u128(quotient, width) : quotient;
}

inline _harc_u128 harc_smod_u128(_harc_u128 lhs, _harc_u128 rhs, unsigned width) {
    const _harc_u128 mask = harc_mask_u128(width);
    lhs &= mask;
    rhs &= mask;
    if (rhs == 0 || width == 0) return lhs;
    const bool lhs_negative = ((lhs >> (width - 1)) & 1u) != 0;
    const bool rhs_negative = ((rhs >> (width - 1)) & 1u) != 0;
    if (lhs_negative) lhs = harc_negate_u128(lhs, width);
    if (rhs_negative) rhs = harc_negate_u128(rhs, width);
    const _harc_u128 remainder = lhs % rhs;
    return lhs_negative ? harc_negate_u128(remainder, width) : remainder;
}

inline bool harc_slt_u128(_harc_u128 lhs, _harc_u128 rhs, unsigned width) {
    lhs &= harc_mask_u128(width);
    rhs &= harc_mask_u128(width);
    if (width == 0) return false;
    const bool lhs_negative = ((lhs >> (width - 1)) & 1u) != 0;
    const bool rhs_negative = ((rhs >> (width - 1)) & 1u) != 0;
    if (lhs_negative != rhs_negative) return lhs_negative;
    return lhs < rhs;
}

inline _harc_u128 harc_trunc_u128(_harc_u128 value, unsigned width) {
    return value & harc_mask_u128(width);
}

inline _harc_u128 harc_shl_u128(_harc_u128 value, uint64_t shift, unsigned width) {
    if (shift >= 128) return 0;
    return harc_trunc_u128(value << shift, width);
}

inline _harc_u128 harc_shr_u128(_harc_u128 value, uint64_t shift, unsigned width) {
    if (shift >= width || shift >= 128) return 0;
    return harc_trunc_u128(value, width) >> shift;
}

inline _harc_u128 harc_ashr_u128(_harc_u128 value, uint64_t shift, unsigned width) {
    value = harc_trunc_u128(value, width);
    if (width == 0) return 0;
    const bool negative = ((value >> (width - 1)) & 1u) != 0;
    if (shift >= width) return negative ? harc_mask_u128(width) : 0;
    _harc_u128 out = value >> shift;
    if (negative && shift != 0) {
        out |= harc_mask_u128(width) & ~harc_mask_u128(width - shift);
    }
    return harc_trunc_u128(out, width);
}

template<std::size_t N>
inline HarcWide<N> harc_wide_ashr(HarcWide<N> value, uint64_t shift, unsigned width) {
    value = harc_wide_mask_bits(value, width);
    if (width == 0) return HarcWide<N>();
    const bool negative = harc_wide_get_bit(value, width - 1);
    if (shift >= width) {
        HarcWide<N> out;
        if (negative) {
            for (unsigned bit = 0; bit < width; ++bit) harc_wide_set_bit(out, bit);
        }
        return out;
    }
    HarcWide<N> out = value >> shift;
    if (negative) {
        for (unsigned bit = width - static_cast<unsigned>(shift); bit < width; ++bit) {
            harc_wide_set_bit(out, bit);
        }
    }
    return harc_wide_mask_bits(out, width);
}

inline _harc_u128 harc_sext_u128(_harc_u128 value, unsigned source_width, unsigned dest_width) {
    value &= harc_mask_u128(source_width);
    if (source_width == 0 || dest_width == 0) return 0;
    if (source_width >= dest_width) return value & harc_mask_u128(dest_width);
    const _harc_u128 sign = static_cast<_harc_u128>(1) << (source_width - 1);
    if ((value & sign) == 0) return value & harc_mask_u128(dest_width);
    const _harc_u128 fill = harc_mask_u128(dest_width) & ~harc_mask_u128(source_width);
    return (value | fill) & harc_mask_u128(dest_width);
}

template<std::size_t N>
inline uint64_t harc_bits(const HarcWide<N>& value, uint32_t hi, uint32_t lo) {
    if (hi < lo || hi >= N * 32 || lo >= N * 32) return 0;
    const uint32_t width = hi - lo + 1;
    uint64_t out = 0;
    const uint32_t capped = width > 64 ? 64 : width;
    for (uint32_t b = 0; b < capped; ++b) {
        const uint32_t src = lo + b;
        const uint32_t word = src / 32;
        const uint32_t bit = src % 32;
        if ((value.words[word] >> bit) & 1u) out |= (uint64_t{1} << b);
    }
    return out;
}

// ── Flattened `Vec<Bus, N>` / multi-lane port lane access ────────────
// A `Vec<Bus, N>` port (or any multi-lane bus port) flattens in
// `arch build`'s SystemVerilog to a PACKED vector that Verilator
// exposes as a single packed C++ scalar (`CData`/`SData`/`IData`/…):
//
//   input logic [2:0]                 m_ar_valid   → uint8_t  m_ar_valid;   (W=1)
//   input logic [2:0][MASTER_ID_W-1:0] m_ar_id     → uint16_t m_ar_id;      (W=3)
//
// Lane `i` lives at bits `[i*W +: W]`. The ARCH native sim, by contrast,
// exposes the very same port as a true C++ array (`uint8_t m_ar_id[3]`),
// where lane `i` is just `m_ar_id[i]`. The HARC source writes
// `dut.m_ar_id[i]` for both; these helpers make that lower correctly
// against either backend from the *same* generated C++ by branching on
// whether the port type is array-indexable at compile time.
//
// `W` is the per-lane bit-width, supplied by the TB codegen from the SV
// port shape (defaults to 1 for single-dimension lanes). The packed
// branch read-extracts / read-modify-writes the `W`-bit field; the
// array branch ignores `W` and indexes directly (so genuine unpacked
// `Vec` ports — which Verilator keeps as C++ arrays — are untouched).

// A packed port whose total width exceeds 64 bits is exposed by
// Verilator as a `VlWide<NW>` (array of 32-bit words), which is neither
// a C array nor an arithmetic scalar. `harc_vec_lane_is_word_array`
// detects that case so the lane helpers extract/deposit word-by-word
// instead of going through `_harc_u128` (which can't hold the whole
// signal). Detection: not an array, not arithmetic, and indexable with
// `[]` yielding a word.
template<typename Sig, typename = void>
struct harc_vec_lane_is_word_array : std::false_type {};
template<typename Sig>
struct harc_vec_lane_is_word_array<
    Sig,
    std::void_t<decltype(std::declval<const Sig&>()[0])>>
    : std::bool_constant<!std::is_array_v<Sig> && !std::is_arithmetic_v<Sig>> {};

template<unsigned W, typename Sig>
inline auto harc_vec_lane_read(const Sig& sig, std::size_t lane) {
    if constexpr (std::is_array_v<Sig>) {
        // Native-sim array port (and Verilator `unpacked Vec` ports):
        // lane is a real element.
        return harc_read(sig[lane]);
    } else if constexpr (harc_vec_lane_is_word_array<Sig>::value) {
        // Verilator packed-vector port wider than 64b (`VlWide<NW>`):
        // extract the `W`-bit lane field word-by-word. `W` for such
        // ports is ≤64 (per-lane payload), so a uint64 result suffices.
        const unsigned lo = static_cast<unsigned>(lane) * W;
        uint64_t out = 0;
        const unsigned capped = W > 64 ? 64 : W;
        for (unsigned b = 0; b < capped; ++b) {
            const unsigned src = lo + b;
            const unsigned word = src / 32;
            const unsigned bit = src % 32;
            if ((static_cast<uint32_t>(sig[word]) >> bit) & 1u) {
                out |= (uint64_t{1} << b);
            }
        }
        return out;
    } else {
        // Verilator packed-vector port ≤64b: lane is a `W`-bit field.
        const _harc_u128 v = static_cast<_harc_u128>(sig);
        const _harc_u128 shifted = v >> (static_cast<_harc_u128>(lane) * W);
        return static_cast<_harc_u128>(shifted & harc_mask_u128(W));
    }
}

template<unsigned W, typename Sig, typename Val>
inline void harc_vec_lane_write(Sig& sig, std::size_t lane, Val val) {
    if constexpr (std::is_array_v<Sig>) {
        harc_assign(sig[lane], val);
    } else if constexpr (harc_vec_lane_is_word_array<Sig>::value) {
        // Read-modify-write the `W`-bit lane field of a `VlWide<NW>`
        // port, word-by-word.
        const unsigned lo = static_cast<unsigned>(lane) * W;
        const uint64_t v = static_cast<uint64_t>(val);
        const unsigned capped = W > 64 ? 64 : W;
        for (unsigned b = 0; b < capped; ++b) {
            const unsigned src = lo + b;
            const unsigned word = src / 32;
            const unsigned bit = src % 32;
            const uint32_t bitval = static_cast<uint32_t>((v >> b) & 1u);
            uint32_t w = static_cast<uint32_t>(sig[word]);
            w = (w & ~(uint32_t{1} << bit)) | (bitval << bit);
            sig[word] = w;
        }
    } else {
        // Read-modify-write the `W`-bit lane field in the packed scalar.
        using Bare = std::remove_cv_t<std::remove_reference_t<Sig>>;
        const unsigned shift = static_cast<unsigned>(lane) * W;
        const _harc_u128 field_mask = harc_mask_u128(W) << shift;
        const _harc_u128 ins =
            (static_cast<_harc_u128>(static_cast<uint64_t>(val)) & harc_mask_u128(W)) << shift;
        const _harc_u128 cur = static_cast<_harc_u128>(sig);
        if constexpr (harc_is_accessor_proxy_v<Bare>) {
            // Co-sim accessor proxy: assign through its uint64_t path —
            // scalar proxies only exist for <=64-bit ports, so the
            // truncation is exact.
            sig = static_cast<uint64_t>((cur & ~field_mask) | ins);
        } else {
            sig = static_cast<Bare>((cur & ~field_mask) | ins);
        }
    }
}

// ── Wider-than-128-bit support ───────────────────────────────────────
// 65–128b values flow through `_harc_u128` (above). For wider signals
// (256, 512, 1024, … up to arbitrary `VlWide<N>`), the natural surface
// is a word-array literal: `dut.wdata = 0x<N hex digits>` lowers in
// the codegen to a word-array initializer-list and routes through
// these helpers. Words are LSB-first to match Verilator's `VlWide`
// layout (word 0 = bits[31:0], word 1 = bits[63:32], …).
//
// `harc_assign_words` writes the literal into the signal, padding any
// extra signal words with zero. `harc_eq_words` compares word-by-word,
// treating any unspecified words as zero on both sides.

template<typename Sig>
inline void harc_assign_words(Sig& sig, std::initializer_list<uint32_t> words) {
    if constexpr (std::is_arithmetic_v<Sig>) {
        // Narrow signal — pack the low 1–2 words into a uint64.
        auto it = words.begin();
        uint64_t v = 0;
        if (it != words.end()) v  = static_cast<uint64_t>(*it++);
        if (it != words.end()) v |= static_cast<uint64_t>(*it++) << 32;
        // Extra words beyond uint64 capacity are dropped (would set
        // bits the signal can't hold anyway).
        sig = static_cast<Sig>(v);
    } else {
        constexpr std::size_t N = sizeof(Sig) / sizeof(uint32_t);
        std::size_t i = 0;
        for (auto w : words) {
            if (i < N) sig[i] = w;
            ++i;
        }
        for (std::size_t j = i; j < N; ++j) sig[j] = 0;
    }
}

// Number of 32-bit words a signal can physically hold. For Verilator
// `VlWide<N>` ports this is `N`; for plain arithmetic signals it is the
// `sizeof`-derived word count (1 for ≤32b, 2 for ≤64b).
template<typename Sig>
inline constexpr std::size_t harc_sig_word_capacity() {
    if constexpr (std::is_arithmetic_v<Sig>) {
        return sizeof(Sig) / sizeof(uint32_t) == 0 ? 1
             : sizeof(Sig) / sizeof(uint32_t);
    } else {
        return sizeof(Sig) / sizeof(uint32_t);
    }
}

// Over-width guard for word-list assignment. `ReqWords` is the number of
// 32-bit words the *value* of the literal actually needs (i.e. the index
// of its highest set bit, divided into words and rounded up). The codegen
// computes it from the literal's value — leading-zero words do NOT count,
// so a literal written wider than necessary but whose value fits is fine.
//
// When `ReqWords` exceeds the signal's physical word capacity the
// `static_assert` fails at C++ compile time, turning a previously-silent
// truncation (high words dropped / data misaligned) into a hard,
// named build error. See `docs` and the HARC-side diagnostic that
// precomputes `ReqWords` in `cpp_tb.rs`.
template<std::size_t ReqWords, typename Sig>
inline void harc_assign_words_checked(Sig& sig,
                                      std::initializer_list<uint32_t> words) {
    static_assert(ReqWords <= harc_sig_word_capacity<Sig>(),
                  "HARC: literal value is too wide for the target port/signal "
                  "(its required bit width exceeds the port width); the high "
                  "bits would be silently dropped. Narrow the literal so its "
                  "value fits, or widen the port.");
    harc_assign_words(sig, words);
}

// Inline hex formatter for 65–128-bit values used by the printf-style
// interpolation lowering. Constructed as a temporary in the printf
// arg list:
//
//   sim_log_line("INFO", "ct=0x%s",
//                (const char*)HarcHexBuf128(harc_read(dut->x), 32, false));
//
// The temporary's lifetime extends through the full surrounding
// expression (the printf call), so the returned `const char*` is
// valid for the duration of the printf. Each call site instantiates
// its own temporary, so multiple wide-hex args in one printf don't
// clobber each other (each has its own on-stack buffer).
//
// `_harc_u128` is up to 32 hex digits; `width` is clamped to that
// range. Narrower than 1 is also clamped (to 1) so no zero-length
// formatting.
struct HarcHexBuf128 {
    char buf[40];
    HarcHexBuf128(_harc_u128 v, int width, bool upper) {
        const char* hex = upper ? "0123456789ABCDEF" : "0123456789abcdef";
        if (width < 1) width = 1;
        if (width > 32) width = 32;
        for (int i = width - 1; i >= 0; --i) {
            buf[i] = hex[(uint32_t)(v & 0xf)];
            v >>= 4;
        }
        buf[width] = '\0';
    }
    operator const char*() const { return buf; }
};

struct HarcHexBufWide {
    char buf[260];

    template<typename T>
    HarcHexBufWide(const T& v, int width, bool upper) {
        const char* hex = upper ? "0123456789ABCDEF" : "0123456789abcdef";
        if (width < 1) width = 1;
        if (width > 256) width = 256;
        for (int i = 0; i < width; ++i) buf[i] = '0';
        if constexpr (is_harc_wide_v<T>) {
            constexpr std::size_t N = harc_wide_words<std::remove_cv_t<std::remove_reference_t<T>>>::value;
            for (std::size_t wi = 0; wi < N; ++wi) {
                uint32_t word = v.words[wi];
                for (int nib = 0; nib < 8; ++nib) {
                    int pos = width - 1 - static_cast<int>(wi * 8 + nib);
                    if (pos >= 0) {
                        buf[pos] = hex[word & 0xfu];
                    }
                    word >>= 4;
                }
            }
        } else {
            _harc_u128 tmp = static_cast<_harc_u128>(v);
            for (int i = width - 1; i >= 0; --i) {
                buf[i] = hex[static_cast<uint32_t>(tmp & 0xf)];
                tmp >>= 4;
            }
        }
        buf[width] = '\0';
    }

    operator const char*() const { return buf; }
};

template<typename Sig>
inline bool harc_eq_words(const Sig& sig, std::initializer_list<uint32_t> words) {
    if constexpr (std::is_arithmetic_v<Sig>) {
        // Pack the literal's low 64 bits.
        auto it = words.begin();
        uint64_t expected = 0;
        if (it != words.end()) expected  = static_cast<uint64_t>(*it++);
        if (it != words.end()) expected |= static_cast<uint64_t>(*it++) << 32;
        // Any further literal words must be zero for equality.
        while (it != words.end()) { if (*it++ != 0) return false; }
        return static_cast<uint64_t>(sig) == expected;
    } else {
        constexpr std::size_t N = sizeof(Sig) / sizeof(uint32_t);
        static_assert(N > 0, "harc_eq_words: signal type has no word representation");
        std::size_t i = 0;
        for (auto w : words) {
            uint32_t s = (i < N) ? static_cast<uint32_t>(sig[i]) : 0u;
            if (s != w) return false;
            ++i;
        }
        // Any remaining signal words must be zero for equality.
        for (std::size_t j = i; j < N; ++j) {
            if (sig[j] != 0) return false;
        }
        return true;
    }
}

struct ThreadScheduler;

// Promise type for HARC coroutines. Returns void (the coroutine's
// effect is on the test's signal state, not a return value); the
// handle is owned by the scheduler via a ThreadSlot.
struct HarcThread {
    struct promise_type {
        HarcThread get_return_object() {
            return HarcThread{ std::coroutine_handle<promise_type>::from_promise(*this) };
        }
        std::suspend_always initial_suspend() noexcept { return {}; }
        std::suspend_always final_suspend()   noexcept { return {}; }
        void return_void() noexcept {}
        void unhandled_exception() { std::terminate(); }
    };
    std::coroutine_handle<promise_type> h;
    bool done() const { return !h || h.done(); }
    void resume() { if (h && !h.done()) h.resume(); }
    void destroy() { if (h) { h.destroy(); h = nullptr; } }
};

// State a suspended slot is parked on.
//   Ready            — not currently suspended; will run on next tick().
//   WaitUntil        — resume when pred() returns true at a posedge.
//   WaitUntilTimeout — same as WaitUntil but with a per-slot cycle
//                      countdown; resume when EITHER pred() becomes
//                      true OR cycles_remaining hits 0. The `timed_out`
//                      flag distinguishes the two resume reasons for
//                      the awaiter's return value.
//   WaitCycles       — resume after `cycles_remaining` posedges.
//   Done             — coroutine finished.
enum class WaitKind : uint8_t { Ready, WaitUntil, WaitUntilTimeout, WaitCycles, Done };

// One scheduled coroutine. The scheduler owns these; awaiters mutate
// the fields when a coroutine suspends.
struct ThreadSlot {
    HarcThread thread;
    WaitKind   kind = WaitKind::Ready;
    uint32_t   cycles_remaining = 0;
    std::function<bool()> pred;  // for WaitUntil + WaitUntilTimeout
    /// Set by the scheduler when a `WaitUntilTimeout` slot resumes
    /// because the cycle budget hit 0 (predicate never became true).
    /// `false` when the slot resumes because `pred()` fired first.
    /// Read by `WaitUntilTimeoutAwaiter::await_resume` to return the
    /// "satisfied?" boolean to the coroutine.
    bool timed_out = false;
};

// Awaiter: `co_await wait_until(slot, pred)`. The predicate is captured
// by value into the slot; the lambda's own captures must outlive the
// suspend, which is trivially true since the caller is the same
// coroutine.
struct WaitUntilAwaiter {
    std::function<bool()> pred;
    ThreadSlot* slot;
    bool await_ready() noexcept {
        // Don't short-circuit even when pred is true: the semantics is
        // "resumes at the *next* posedge where pred is true", matching
        // arch's lowered-fsm wait-state behavior. Same-cycle resume
        // would skip the implicit one-cycle quantum.
        return false;
    }
    void await_suspend(std::coroutine_handle<>) noexcept {
        slot->kind = WaitKind::WaitUntil;
        slot->pred = std::move(pred);
    }
    void await_resume() noexcept {}
};

// Awaiter: `co_await wait_cycles(slot, N)`. N must be >= 1; passing
// N == 0 is treated as "no wait" (await_ready returns true).
struct WaitCyclesAwaiter {
    uint32_t n;
    ThreadSlot* slot;
    bool await_ready() noexcept { return n == 0; }
    void await_suspend(std::coroutine_handle<>) noexcept {
        slot->kind = WaitKind::WaitCycles;
        slot->cycles_remaining = n;
    }
    void await_resume() noexcept {}
};

// Awaiter: `co_await wait_until_timeout(slot, pred, max_cycles)`.
//
// Replaces the codegen's previous polling-loop shape
// (`while (!cond) { co_await wait_cycles(_slot, 1); }`) for timed
// `wait until <expr> timeout N cycles fail("…")` (spec §7.9). One
// scheduler round-trip instead of N: the scheduler evaluates `pred`
// each tick (same path as plain WaitUntil) and additionally decrements
// `cycles_remaining`. Resumes when EITHER pred fires OR the countdown
// hits zero; `await_resume()` returns `true` for the former,
// `false` for the latter.
//
// Short-circuit when pred is already true at entry: `await_ready`
// returns true and the coroutine never suspends — matches the
// existing polling-loop semantics where `pred-true-at-entry` doesn't
// wait. The `ready_satisfied` flag is per-awaiter (lives in the
// coroutine frame), not on the slot, so multiple in-flight
// `wait_until_timeout` calls in the same coroutine don't interfere.
struct WaitUntilTimeoutAwaiter {
    std::function<bool()> pred;
    ThreadSlot* slot;
    uint32_t max_cycles;
    bool ready_satisfied = false;

    bool await_ready() noexcept {
        if (pred && pred()) { ready_satisfied = true; return true; }
        // N == 0 + pred false: also short-circuit (immediate timeout).
        // Same semantics as the existing polling-loop's
        // `(cycle_count - start) < 0` guard skipping the loop body.
        if (max_cycles == 0) { ready_satisfied = false; return true; }
        return false;
    }
    void await_suspend(std::coroutine_handle<>) noexcept {
        slot->kind = WaitKind::WaitUntilTimeout;
        slot->pred = std::move(pred);
        slot->cycles_remaining = max_cycles;
        slot->timed_out = false;
    }
    bool await_resume() noexcept {
        // Short-circuit path: ready_satisfied was set by await_ready.
        // Suspend path: scheduler set slot->timed_out.
        return ready_satisfied || !slot->timed_out;
    }
};

// Scheduler: one per test. Owns slot pointers; the slot lifetime is
// the surrounding test class / main scope.
struct ThreadScheduler {
    std::vector<ThreadSlot*> slots;

    // Run all initially-Ready slots until they hit their first wait.
    // Called once after coroutines are constructed and before the
    // first posedge — so test setup statements (driving rst, default
    // signal values) run before the DUT samples anything.
    void bootstrap() {
        for (auto* s : slots) {
            if (s->kind == WaitKind::Ready) {
                s->thread.resume();
                if (s->thread.done()) s->kind = WaitKind::Done;
            }
        }
    }

    // Called by the test loop after the posedge eval / checker pass.
    // Semantics:
    //   - WaitCycles slots: decrement; if hits 0, mark Ready.
    //   - WaitUntil  slots: evaluate pred; if true, mark Ready.
    //   - Ready      slots: resume the coroutine until it suspends or
    //                       finishes. Iterated to a fixed point so
    //                       multi-stage cascades (slot A's resume makes
    //                       slot B's predicate true) settle in one
    //                       tick — matching arch's behavior for
    //                       fork/join unconditional transitions.
    //
    // The resumed[] guard prevents a freshly suspended slot from
    // re-firing in the same tick: `wait_cycles(N)` and `wait_until`
    // both promise at-least-one-cycle quantum from the suspend.
    void tick() {
        std::vector<bool> resumed(slots.size(), false);

        // Pass 1: advance counters / preds based on prior-tick state.
        for (auto* s : slots) {
            if (s->kind == WaitKind::WaitCycles) {
                if (s->cycles_remaining > 0) --s->cycles_remaining;
                if (s->cycles_remaining == 0) s->kind = WaitKind::Ready;
            } else if (s->kind == WaitKind::WaitUntil) {
                if (s->pred && s->pred()) s->kind = WaitKind::Ready;
            } else if (s->kind == WaitKind::WaitUntilTimeout) {
                // Pred-first: if it fires this cycle, that takes
                // priority over a coincident countdown hit (the user
                // asked "did the condition hold within N cycles?";
                // having it hold at cycle N exactly counts as yes).
                if (s->pred && s->pred()) {
                    s->kind = WaitKind::Ready;
                    s->timed_out = false;
                } else if (s->cycles_remaining > 0) {
                    --s->cycles_remaining;
                    if (s->cycles_remaining == 0) {
                        s->kind = WaitKind::Ready;
                        s->timed_out = true;
                    }
                } else {
                    // cycles_remaining was 0 at entry and pred didn't
                    // fire — defensive; the awaiter's await_ready
                    // short-circuits this case so we shouldn't reach
                    // here in practice.
                    s->kind = WaitKind::Ready;
                    s->timed_out = true;
                }
            }
        }
        // Pass 2 (fixed point): resume Ready slots, then re-check
        // remaining WaitUntil/WaitUntilTimeout preds (a resumed slot
        // may have changed signal state another slot is waiting on).
        bool changed = true;
        while (changed) {
            changed = false;
            for (size_t i = 0; i < slots.size(); ++i) {
                if (slots[i]->kind == WaitKind::Ready && !resumed[i]) {
                    resumed[i] = true;
                    slots[i]->thread.resume();
                    if (slots[i]->thread.done()) slots[i]->kind = WaitKind::Done;
                    changed = true;
                }
            }
            for (size_t i = 0; i < slots.size(); ++i) {
                if (!resumed[i] && slots[i]->kind == WaitKind::WaitUntil) {
                    if (slots[i]->pred && slots[i]->pred()) {
                        slots[i]->kind = WaitKind::Ready;
                        changed = true;
                    }
                }
                if (!resumed[i] && slots[i]->kind == WaitKind::WaitUntilTimeout) {
                    // Same priority rule as Pass 1: pred-true wins
                    // over a coincident timeout.
                    if (slots[i]->pred && slots[i]->pred()) {
                        slots[i]->kind = WaitKind::Ready;
                        slots[i]->timed_out = false;
                        changed = true;
                    }
                }
            }
        }
    }

    bool all_done() const {
        for (auto* s : slots) if (s->kind != WaitKind::Done) return false;
        return true;
    }
};

// Convenience constructors. Pass the slot the caller is parked in so
// the awaiter can write its suspend state.
inline WaitUntilAwaiter  wait_until (ThreadSlot* s, std::function<bool()> p) { return {std::move(p), s}; }
inline WaitCyclesAwaiter wait_cycles(ThreadSlot* s, uint32_t n)              { return {n, s}; }
inline WaitUntilTimeoutAwaiter wait_until_timeout(
    ThreadSlot* s, std::function<bool()> p, uint32_t n
) { return {std::move(p), s, n, /*ready_satisfied=*/false}; }

// ─── Multi-OS-thread support (Phase 3a) ───────────────────────────────
//
// Atomic spin-wait barrier. ~10–30 ns per round-trip vs ~µs for
// std::condition_variable. Used by emit_main's per-cycle barrier
// dance to synchronize the main thread with N worker threads (one
// per bound-driver/bound-monitor coroutine actor).
//
// Mirrors arch-com's `arch_rt::Barrier` exactly so the two runtimes
// can later be merged. Dual cache-line padding avoids false sharing
// with neighbouring fields — Apple Silicon's strong cache-line
// bouncing penalty makes alignment matter much more than on x86.
//
// Construct with `target` = number of participating threads. Each
// thread calls `wait()` at the synchronization point; all participants
// advance together.
//
// Caveat (matches arch-com's ThreadSimPerf measurements): per-cycle
// barrier sync on Apple Silicon costs ~10s of µs round-trip due to
// P/E core scheduling jitter. With ~µs of per-cycle actor work,
// multi-thread mode is *slower* than single-thread cooperative mode.
// Cycle batching (`run_cycles(K)` API, Phase 3b) is required for
// wall-clock wins. Phase 3a delivers the runtime topology and
// validates correctness against the cooperative baseline; perf comes
// next.
struct alignas(64) Barrier {
    alignas(64) std::atomic<uint32_t> count{0};
    alignas(64) std::atomic<uint32_t> generation{0};
    uint32_t target;
    explicit Barrier(uint32_t target) : target(target) {}
    void wait() {
        uint32_t gen = generation.load(std::memory_order_acquire);
        if (count.fetch_add(1, std::memory_order_acq_rel) + 1 == target) {
            count.store(0, std::memory_order_release);
            generation.fetch_add(1, std::memory_order_release);
        } else {
            // Spin briefly, then yield to avoid pegging a core when
            // other participants are slow (oversubscribed). Long spin
            // window: per-cycle sim work is often sub-µs so a low
            // budget would trigger OS context switches every cycle.
            // ~100k iters ≈ 30–100 µs on modern CPUs — well over
            // typical per-cycle work.
            uint32_t spins = 0;
            while (generation.load(std::memory_order_acquire) == gen) {
                if (++spins > 100000) {
                    std::this_thread::yield();
                    spins = 0;
                }
            }
        }
    }
};

} // namespace harc_rt
