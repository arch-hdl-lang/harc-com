// harc_z3_rt.h — Z3 helper routines for generated HARC solver code.
//
// This header is included only by generated testbenches that lower
// randomization constraints through Z3.

#pragma once

#include "harc_thread_rt.h"
#include <cstdint>
#include <cstddef>
#include <z3++.h>

inline z3::expr harc_z3_bv_words(
    z3::context& ctx,
    const uint32_t* words,
    size_t word_count,
    unsigned width) {
    z3::expr out = ctx.bv_val((uint64_t)0, width);
    for (size_t i = 0; i < word_count; ++i) {
        if (words[i] == 0) continue;
        z3::expr part = ctx.bv_val((uint64_t)words[i], width);
        if (i != 0) part = z3::shl(part, ctx.bv_val((uint64_t)(i * 32), width));
        out = out | part;
    }
    return out;
}

inline z3::expr harc_z3_bv_value(z3::context& ctx, uint64_t v, unsigned width) {
    uint32_t words[2] = {static_cast<uint32_t>(v), static_cast<uint32_t>(v >> 32)};
    return harc_z3_bv_words(ctx, words, 2, width);
}

inline z3::expr harc_z3_bv_signed_value(z3::context& ctx, int64_t v, unsigned width) {
    uint32_t words[32] = {};
    size_t word_count = (width + 31) / 32;
    if (word_count > 32) word_count = 32;
    uint32_t fill = v < 0 ? 0xffffffffu : 0u;
    for (size_t i = 0; i < word_count; ++i) words[i] = fill;
    uint64_t raw = static_cast<uint64_t>(v);
    if (word_count > 0) words[0] = static_cast<uint32_t>(raw);
    if (word_count > 1) words[1] = static_cast<uint32_t>(raw >> 32);
    unsigned rem = width % 32;
    if (rem != 0 && word_count > 0) words[word_count - 1] &= ((1u << rem) - 1u);
    return harc_z3_bv_words(ctx, words, word_count, width);
}

inline z3::expr harc_z3_bv_value(z3::context& ctx, int64_t v, unsigned width) {
    return harc_z3_bv_signed_value(ctx, v, width);
}

inline z3::expr harc_z3_bv_value(z3::context& ctx, _harc_u128 v, unsigned width) {
    uint32_t words[4] = {
        static_cast<uint32_t>(v),
        static_cast<uint32_t>(v >> 32),
        static_cast<uint32_t>(v >> 64),
        static_cast<uint32_t>(v >> 96),
    };
    return harc_z3_bv_words(ctx, words, 4, width);
}

template<size_t N>
inline z3::expr harc_z3_bv_value(
    z3::context& ctx,
    const harc_rt::HarcWide<N>& v,
    unsigned width) {
    return harc_z3_bv_words(ctx, v.words.data(), v.words.size(), width);
}

inline z3::expr harc_z3_bv_signed_extend(
    z3::context& ctx,
    const z3::expr& value,
    unsigned value_width,
    unsigned solver_width) {
    if (solver_width <= value_width) return value;
    return z3::to_expr(ctx, Z3_mk_sign_ext(ctx, solver_width - value_width, value));
}

template<typename T>
inline z3::expr harc_z3_bv_signed_value(
    z3::context& ctx,
    const T& v,
    unsigned value_width,
    unsigned solver_width) {
    return harc_z3_bv_signed_extend(
        ctx,
        harc_z3_bv_value(ctx, v, value_width),
        value_width,
        solver_width);
}

inline uint64_t harc_z3_bv_low_u64(z3::context& ctx, const z3::expr& value) {
    uint64_t raw = 0;
    z3::expr simplified = value.simplify();
    if (simplified.is_numeral_u64(raw)) return raw;
    const char* bits = Z3_get_numeral_binary_string(ctx, simplified);
    if (!bits) return 0;
    size_t len = 0;
    while (bits[len] != '\0') ++len;
    for (size_t src = 0; src < len && src < 64; ++src) {
        if (bits[len - 1 - src] == '1') raw |= (uint64_t{1} << src);
    }
    return raw;
}
