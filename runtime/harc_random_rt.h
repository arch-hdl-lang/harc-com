// harc_random_rt.h — Runtime randomization scaffold for HARC.
//
// Phase 5A only defines the ABI-shaped boundary that generated C++ will
// eventually call. Current codegen still emits inline Z3 solving in
// cpp_tb.rs; these entrypoints are intentionally inert until the typed
// runtime backend takes ownership of solving.

#pragma once

#include <cstdio>
#include <functional>
#include <initializer_list>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <vector>
#include "harc_thread_rt.h"

namespace harc_rt {
namespace random {

using harc_problem_id = uint32_t;
using harc_call_site_id = uint32_t;
using harc_call_iteration = uint64_t;
using harc_seed = uint64_t;

struct HarcSolveStatus {
    bool ok = true;
    const char* message = nullptr;
    harc_problem_id problem_id = 0;
    harc_seed seed = 0;
};

struct HarcRuntimeProblemDescriptor {
    harc_problem_id id = 0;
    const char* origin = nullptr;
    const char* manifest = nullptr;
};

struct HarcRuntimeProblemTable {
    const HarcRuntimeProblemDescriptor* problems = nullptr;
    uint32_t len = 0;
};

struct HarcRuntimeCallSite {
    harc_call_site_id site_id = 0;
    harc_problem_id problem_id = 0;
    harc_call_iteration iteration = 0;
};

struct HarcRandomizeCall {
    const HarcRuntimeProblemDescriptor* problem = nullptr;
    harc_problem_id problem_id = 0;
    harc_seed seed = 0;
};

struct HarcDistBin {
    int64_t lo = 0;
    int64_t hi = 0;
    int64_t weight = 0;
};

struct HarcAutoCovSelection {
    uint8_t kind = 0;
    int group = -1;
    size_t i = 0;
    size_t j = 0;
};

struct HarcAutoCovPointMeta {
    const char* const* labels = nullptr;
    size_t len = 0;
};

struct HarcAutoCovCrossMeta {
    const char* const* labels = nullptr;
    size_t rows = 0;
    size_t cols = 0;
};

struct HarcAutoCovPlan {
    const char* type_name = nullptr;
    uint32_t span = 0;
    const HarcAutoCovPointMeta* points = nullptr;
    size_t point_count = 0;
    const HarcAutoCovCrossMeta* crosses = nullptr;
    size_t cross_count = 0;
};

struct HarcAutoCovState {
    bool initialized = false;
    std::vector<uint8_t> point_hit;
    std::vector<uint8_t> point_blocked;
    std::vector<uint8_t> cross_hit;
    std::vector<uint8_t> cross_blocked;
};

struct HarcSolverRetryPolicy {
    bool retried_without_preferences = false;
    bool retried_without_unique_history = false;
};

template <typename T>
using HarcUniqueHistory = std::vector<T>;

enum class HarcSolveMode : uint8_t {
    Inline,
    Queued,
    Blocking,
};

inline constexpr const HarcRuntimeProblemDescriptor* harc_find_problem(
    const HarcRuntimeProblemTable& table,
    harc_problem_id id) {
    for (uint32_t i = 0; i < table.len; ++i) {
        if (table.problems[i].id == id) return &table.problems[i];
    }
    return nullptr;
}

inline HarcRuntimeCallSite* harc_find_call_site(
    HarcRuntimeCallSite* sites,
    uint32_t len,
    harc_problem_id problem_id) {
    for (uint32_t i = 0; i < len; ++i) {
        if (sites[i].problem_id == problem_id) return &sites[i];
    }
    return nullptr;
}

inline constexpr uint64_t harc_splitmix64(uint64_t value) {
    value += 0x9E3779B97F4A7C15ull;
    value = (value ^ (value >> 30)) * 0xBF58476D1CE4E5B9ull;
    value = (value ^ (value >> 27)) * 0x94D049BB133111EBull;
    return value ^ (value >> 31);
}

inline uint64_t harc_rng_next_state(uint64_t& state) {
    state += 0x9E3779B97F4A7C15ull;
    uint64_t z = state;
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
    z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
    return z ^ (z >> 31);
}

inline void harc_rng_seed_from_env(uint64_t& state, const char* env_name = "HARC_SEED") {
    const char* s = std::getenv(env_name);
    state = (s && *s) ? std::strtoull(s, nullptr, 0) : 1ull;
}

struct HarcRng {
    uint64_t state = 0;

    void seed_from_env(const char* env_name = "HARC_SEED") {
        harc_rng_seed_from_env(state, env_name);
    }

    uint64_t next() {
        return harc_rng_next_state(state);
    }
};

inline constexpr harc_seed harc_seed_from(
    harc_seed global_seed,
    harc_call_site_id site_id,
    harc_call_iteration iteration) {
    uint64_t mixed = global_seed;
    mixed ^= uint64_t{site_id} * 0xD6E8FEB86659FD93ull;
    mixed ^= iteration * 0xA0761D6478BD642Full;
    return harc_splitmix64(mixed);
}

inline harc_seed harc_call_site_next_seed(
    HarcRuntimeCallSite& site,
    harc_seed global_seed) {
    return harc_seed_from(global_seed, site.site_id, site.iteration++);
}

inline HarcRandomizeCall harc_prepare_randomize_call(
    const HarcRuntimeProblemTable& table,
    HarcRuntimeCallSite* sites,
    uint32_t site_count,
    harc_problem_id problem_id,
    harc_seed global_seed,
    harc_seed fallback_seed) {
    const HarcRuntimeProblemDescriptor* problem = harc_find_problem(table, problem_id);
    HarcRuntimeCallSite* site = harc_find_call_site(sites, site_count, problem_id);
    harc_seed seed = site ? harc_call_site_next_seed(*site, global_seed) : fallback_seed;
    return HarcRandomizeCall{
        problem,
        problem ? problem->id : problem_id,
        seed,
    };
}

inline constexpr uint64_t harc_preference_draw(
    harc_seed seed,
    uint32_t salt) {
    return harc_splitmix64(seed ^ (uint64_t{salt} * 0x9E3779B97F4A7C15ull));
}

inline constexpr uint64_t harc_prefer_uint(
    harc_seed seed,
    uint32_t salt,
    unsigned width) {
    uint64_t draw = harc_preference_draw(seed, salt);
    if (width == 0) return 0;
    if (width >= 64) return draw;
    return draw & ((uint64_t{1} << width) - 1);
}

inline _harc_u128 harc_prefer_u128(
    harc_seed seed,
    uint32_t salt,
    unsigned width) {
    _harc_u128 value =
        (static_cast<_harc_u128>(harc_preference_draw(seed, salt)) |
         (static_cast<_harc_u128>(harc_preference_draw(seed, salt ^ 0x9E3779B9u)) << 64));
    if (width == 0) return 0;
    if (width >= 128) return value;
    return value & ((static_cast<_harc_u128>(1) << width) - 1);
}

template <size_t N>
inline harc_rt::HarcWide<N> harc_prefer_wide(
    harc_seed seed,
    uint32_t salt,
    unsigned width) {
    harc_rt::HarcWide<N> value;
    for (size_t i = 0; i < N; ++i) {
        uint64_t draw = harc_preference_draw(
            seed,
            salt ^ static_cast<uint32_t>(0x9E3779B9u * (i + 1)));
        value[i] = static_cast<uint32_t>(draw);
    }
    unsigned last_bits = width % 32;
    if (last_bits != 0 && N != 0) {
        value[N - 1] &= (uint32_t{1} << last_bits) - 1u;
    }
    return value;
}

inline constexpr int64_t harc_prefer_range(
    harc_seed seed,
    uint32_t salt,
    int64_t lo,
    int64_t hi) {
    if (hi <= lo) return lo;
    uint64_t span = static_cast<uint64_t>(hi - lo) + 1;
    return lo + static_cast<int64_t>(harc_preference_draw(seed, salt) % span);
}

inline constexpr int64_t harc_prefer_sint(
    harc_seed seed,
    uint32_t salt,
    unsigned width) {
    if (width == 0) return 0;
    if (width >= 63) {
        return static_cast<int64_t>(harc_preference_draw(seed, salt));
    }
    int64_t half = int64_t{1} << (width - 1);
    return harc_prefer_range(seed, salt, -half, half - 1);
}

inline int64_t harc_prefer_dist(
    harc_seed seed,
    uint32_t salt,
    std::initializer_list<HarcDistBin> bins) {
    int64_t total = 0;
    for (const auto& bin : bins) total += bin.weight;
    if (total <= 0) return 0;
    int64_t pick = static_cast<int64_t>(
        harc_preference_draw(seed, salt) % static_cast<uint64_t>(total));
    int64_t acc = 0;
    uint32_t bin_salt = 0;
    for (const auto& bin : bins) {
        acc += bin.weight;
        if (pick < acc) {
            return harc_prefer_range(seed, salt ^ (0xA5A5A5A5u + bin_salt), bin.lo, bin.hi);
        }
        ++bin_salt;
    }
    return bins.begin()->lo;
}

template <typename Next>
inline int64_t harc_rng_range(Next next, int64_t lo, int64_t hi) {
    if (hi <= lo) return lo;
    return lo + static_cast<int64_t>(next() % static_cast<uint64_t>(hi - lo + 1));
}

template <typename Next>
inline uint64_t harc_rng_uint(Next next, unsigned width) {
    if (width == 0) return 0;
    if (width >= 64) return next();
    return next() & ((uint64_t{1} << width) - 1);
}

template <typename Next>
inline _harc_u128 harc_rng_u128(Next next, unsigned width) {
    _harc_u128 value =
        (static_cast<_harc_u128>(next()) |
         (static_cast<_harc_u128>(next()) << 64));
    if (width == 0) return 0;
    if (width >= 128) return value;
    return value & ((static_cast<_harc_u128>(1) << width) - 1);
}

template <size_t N, typename Next>
inline harc_rt::HarcWide<N> harc_rng_wide(Next next, unsigned width) {
    harc_rt::HarcWide<N> value;
    for (size_t i = 0; i < N; ++i) {
        value[i] = static_cast<uint32_t>(next());
    }
    unsigned last_bits = width % 32;
    if (last_bits != 0 && N != 0) {
        value[N - 1] &= (uint32_t{1} << last_bits) - 1u;
    }
    return value;
}

template <typename Next>
inline int64_t harc_rng_dist(Next next, std::initializer_list<HarcDistBin> bins) {
    int64_t total = 0;
    for (const auto& bin : bins) total += bin.weight;
    if (total <= 0) return 0;
    int64_t pick = static_cast<int64_t>(next() % static_cast<uint64_t>(total));
    int64_t acc = 0;
    for (const auto& bin : bins) {
        acc += bin.weight;
        if (pick < acc) return harc_rng_range(next, bin.lo, bin.hi);
    }
    return bins.begin()->lo;
}

template <typename T>
inline const HarcUniqueHistory<T>& harc_unique_values(
    const HarcUniqueHistory<T>& history) {
    return history;
}

template <typename T>
inline void harc_unique_clear(HarcUniqueHistory<T>& history) {
    history.clear();
}

template <typename T>
inline void harc_unique_remember(
    HarcUniqueHistory<T>& history,
    const T& value) {
    history.push_back(value);
}

inline constexpr bool harc_auto_cov_has_preference(
    const HarcAutoCovSelection& selection) {
    return selection.kind != 0;
}

inline void harc_auto_cov_select_point(
    HarcAutoCovSelection& selection,
    int group,
    size_t i) {
    selection.kind = 1;
    selection.group = group;
    selection.i = i;
    selection.j = 0;
}

inline void harc_auto_cov_select_cross(
    HarcAutoCovSelection& selection,
    int group,
    size_t i,
    size_t j) {
    selection.kind = 2;
    selection.group = group;
    selection.i = i;
    selection.j = j;
}

inline constexpr bool harc_auto_cov_selected_point(
    const HarcAutoCovSelection& selection,
    int group) {
    return selection.kind == 1 && selection.group == group;
}

inline constexpr bool harc_auto_cov_selected_cross(
    const HarcAutoCovSelection& selection,
    int group) {
    return selection.kind == 2 && selection.group == group;
}

inline size_t harc_auto_cov_point_offset(
    const HarcAutoCovPlan& plan,
    size_t point) {
    size_t offset = 0;
    for (size_t i = 0; i < point && i < plan.point_count; ++i) {
        offset += plan.points[i].len;
    }
    return offset;
}

inline size_t harc_auto_cov_cross_offset(
    const HarcAutoCovPlan& plan,
    size_t cross) {
    size_t offset = 0;
    for (size_t i = 0; i < cross && i < plan.cross_count; ++i) {
        offset += plan.crosses[i].rows * plan.crosses[i].cols;
    }
    return offset;
}

inline size_t harc_auto_cov_point_total(const HarcAutoCovPlan& plan) {
    return harc_auto_cov_point_offset(plan, plan.point_count);
}

inline size_t harc_auto_cov_cross_total(const HarcAutoCovPlan& plan) {
    return harc_auto_cov_cross_offset(plan, plan.cross_count);
}

inline void harc_auto_cov_init(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state) {
    if (state.initialized) return;
    state.point_hit.assign(harc_auto_cov_point_total(plan), 0);
    state.point_blocked.assign(harc_auto_cov_point_total(plan), 0);
    state.cross_hit.assign(harc_auto_cov_cross_total(plan), 0);
    state.cross_blocked.assign(harc_auto_cov_cross_total(plan), 0);
    state.initialized = true;
}

inline uint8_t& harc_auto_cov_point_hit(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    size_t point,
    size_t i) {
    harc_auto_cov_init(plan, state);
    return state.point_hit[harc_auto_cov_point_offset(plan, point) + i];
}

inline uint8_t& harc_auto_cov_point_blocked(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    size_t point,
    size_t i) {
    harc_auto_cov_init(plan, state);
    return state.point_blocked[harc_auto_cov_point_offset(plan, point) + i];
}

inline uint8_t& harc_auto_cov_cross_hit(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    size_t cross,
    size_t i,
    size_t j) {
    harc_auto_cov_init(plan, state);
    const HarcAutoCovCrossMeta& meta = plan.crosses[cross];
    return state.cross_hit[harc_auto_cov_cross_offset(plan, cross) + i * meta.cols + j];
}

inline uint8_t& harc_auto_cov_cross_blocked(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    size_t cross,
    size_t i,
    size_t j) {
    harc_auto_cov_init(plan, state);
    const HarcAutoCovCrossMeta& meta = plan.crosses[cross];
    return state.cross_blocked[harc_auto_cov_cross_offset(plan, cross) + i * meta.cols + j];
}

template <typename T, size_t N>
inline bool harc_auto_cov_apply_point_preference(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    HarcAutoCovSelection& selection,
    int group,
    const T (&values)[N],
    T& preference) {
    if (harc_auto_cov_has_preference(selection)) return false;
    harc_auto_cov_init(plan, state);
    size_t base = harc_auto_cov_point_offset(plan, static_cast<size_t>(group));
    for (size_t i = 0; i < N; ++i) {
        if (!state.point_hit[base + i] && !state.point_blocked[base + i]) {
            preference = values[i];
            harc_auto_cov_select_point(selection, group, i);
            return true;
        }
    }
    return false;
}

template <typename A, typename B, size_t Rows, size_t Cols>
inline bool harc_auto_cov_apply_cross_preference(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    HarcAutoCovSelection& selection,
    int group,
    const A (&a_values)[Rows],
    const B (&b_values)[Cols],
    A& a_preference,
    B& b_preference) {
    if (harc_auto_cov_has_preference(selection)) return false;
    harc_auto_cov_init(plan, state);
    size_t base = harc_auto_cov_cross_offset(plan, static_cast<size_t>(group));
    const HarcAutoCovCrossMeta& meta = plan.crosses[group];
    for (size_t i = 0; i < Rows; ++i) {
        for (size_t j = 0; j < Cols; ++j) {
            size_t idx = base + i * meta.cols + j;
            if (!state.cross_hit[idx] && !state.cross_blocked[idx]) {
                a_preference = a_values[i];
                b_preference = b_values[j];
                harc_auto_cov_select_cross(selection, group, i, j);
                return true;
            }
        }
    }
    return false;
}

inline constexpr const char* harc_auto_cov_state(
    bool hit,
    bool blocked) {
    return hit ? "hit" : (blocked ? "*BLOCKED*" : "*NOT HIT*");
}

inline void harc_auto_cov_report_summary(
    const char* type_name,
    uint32_t span,
    uint64_t hit,
    uint64_t total,
    uint64_t blocked) {
    std::printf(
        "[auto_cov %s@%u] %llu/%llu hit (%.1f%%), blocked=%llu\n",
        type_name ? type_name : "",
        span,
        static_cast<unsigned long long>(hit),
        static_cast<unsigned long long>(total),
        total ? (100.0 * hit / total) : 0.0,
        static_cast<unsigned long long>(blocked));
}

inline void harc_auto_cov_report_bin(
    const char* label,
    bool hit,
    bool blocked) {
    std::printf(
        "  %s : %s\n",
        label ? label : "",
        harc_auto_cov_state(hit, blocked));
}

inline void harc_auto_cov_report(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state) {
    harc_auto_cov_init(plan, state);
    uint64_t hit = 0;
    uint64_t blocked = 0;
    for (uint8_t v : state.point_hit) if (v) ++hit;
    for (uint8_t v : state.cross_hit) if (v) ++hit;
    for (uint8_t v : state.point_blocked) if (v) ++blocked;
    for (uint8_t v : state.cross_blocked) if (v) ++blocked;
    uint64_t total = state.point_hit.size() + state.cross_hit.size();
    harc_auto_cov_report_summary(plan.type_name, plan.span, hit, total, blocked);

    for (size_t point = 0; point < plan.point_count; ++point) {
        size_t base = harc_auto_cov_point_offset(plan, point);
        for (size_t i = 0; i < plan.points[point].len; ++i) {
            harc_auto_cov_report_bin(
                plan.points[point].labels[i],
                state.point_hit[base + i],
                state.point_blocked[base + i]);
        }
    }
    for (size_t cross = 0; cross < plan.cross_count; ++cross) {
        size_t base = harc_auto_cov_cross_offset(plan, cross);
        size_t len = plan.crosses[cross].rows * plan.crosses[cross].cols;
        for (size_t i = 0; i < len; ++i) {
            harc_auto_cov_report_bin(
                plan.crosses[cross].labels[i],
                state.cross_hit[base + i],
                state.cross_blocked[base + i]);
        }
    }
}

template <typename ReportFn>
inline void harc_auto_cov_register_report(
    bool& registered,
    std::vector<std::function<void()>>& reports,
    ReportFn report_fn) {
    if (registered) return;
    reports.push_back(report_fn);
    registered = true;
}

template <typename Flag>
inline void harc_auto_cov_mark_blocked(Flag& blocked) {
    blocked = true;
}

template <typename HitFlag, typename BlockedFlag>
inline void harc_auto_cov_mark_hit(HitFlag& hit, BlockedFlag& blocked) {
    hit = true;
    blocked = false;
}

inline void harc_auto_cov_mark_selected_point_blocked(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    const HarcAutoCovSelection& selection,
    int group) {
    if (harc_auto_cov_selected_point(selection, group)) {
        harc_auto_cov_mark_blocked(
            harc_auto_cov_point_blocked(plan, state, static_cast<size_t>(group), selection.i));
    }
}

inline void harc_auto_cov_mark_selected_cross_blocked(
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    const HarcAutoCovSelection& selection,
    int group) {
    if (harc_auto_cov_selected_cross(selection, group)) {
        harc_auto_cov_mark_blocked(
            harc_auto_cov_cross_blocked(
                plan,
                state,
                static_cast<size_t>(group),
                selection.i,
                selection.j));
    }
}

template <typename T, typename U>
inline void harc_auto_cov_mark_value_hit(
    const T& value,
    const U& expected,
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    size_t point,
    size_t i) {
    if (value == expected) {
        harc_auto_cov_mark_hit(
            harc_auto_cov_point_hit(plan, state, point, i),
            harc_auto_cov_point_blocked(plan, state, point, i));
    }
}

template <typename A, typename B, typename ExpectedA, typename ExpectedB>
inline void harc_auto_cov_mark_cross_hit(
    const A& a,
    const ExpectedA& expected_a,
    const B& b,
    const ExpectedB& expected_b,
    const HarcAutoCovPlan& plan,
    HarcAutoCovState& state,
    size_t cross,
    size_t i,
    size_t j) {
    if (a == expected_a && b == expected_b) {
        harc_auto_cov_mark_hit(
            harc_auto_cov_cross_hit(plan, state, cross, i, j),
            harc_auto_cov_cross_blocked(plan, state, cross, i, j));
    }
}

inline bool harc_retry_without_preferences(
    HarcSolverRetryPolicy& policy,
    bool solver_sat) {
    if (solver_sat) return false;
    policy.retried_without_preferences = true;
    return true;
}

inline bool harc_retry_without_unique_history(
    HarcSolverRetryPolicy& policy,
    bool solver_sat) {
    if (solver_sat) return false;
    policy.retried_without_unique_history = true;
    return true;
}

inline constexpr HarcSolveStatus harc_solve_status_ok() {
    return HarcSolveStatus{true, nullptr, 0, 0};
}

inline constexpr HarcSolveStatus harc_solve_status_unsat(
    harc_problem_id problem_id,
    harc_seed seed) {
    return HarcSolveStatus{
        false,
        "randomize(t) with: constraint UNSAT",
        problem_id,
        seed,
    };
}

inline constexpr bool harc_handle_solve_status(const HarcSolveStatus& status) {
    // During the callback migration, generated solvers still emit any
    // user-facing diagnostics before returning a failed status. The runtime
    // boundary owns that policy so call sites do not need to open-code it.
    return status.ok;
}

template <typename T>
inline HarcSolveStatus harc_solve(
    T& target,
    harc_problem_id problem_id,
    harc_seed seed) {
    (void)target;
    (void)problem_id;
    (void)seed;
    return harc_solve_status_ok();
}

template <typename T, typename GeneratedSolveFn>
inline HarcSolveStatus harc_solve_constrained(
    T& target,
    harc_problem_id problem_id,
    harc_seed seed,
    HarcSolveMode mode,
    GeneratedSolveFn generated_solve_fn) {
    (void)target;
    (void)problem_id;
    (void)seed;
    (void)mode;
    return generated_solve_fn();
}

template <typename T>
inline HarcSolveStatus harc_solve_queued(
    T& target,
    harc_problem_id problem_id,
    harc_seed seed) {
    (void)target;
    (void)problem_id;
    (void)seed;
    return harc_solve_status_ok();
}

template <typename T, typename RandomizeFn>
inline HarcSolveStatus harc_solve_queued(
    T& target,
    harc_problem_id problem_id,
    harc_seed seed,
    RandomizeFn randomize_fn) {
    (void)problem_id;
    (void)seed;
    randomize_fn(&target);
    return harc_solve_status_ok();
}

template <typename T>
inline HarcSolveStatus harc_solve_blocking(
    T& target,
    harc_problem_id problem_id,
    harc_seed seed) {
    (void)target;
    (void)problem_id;
    (void)seed;
    return harc_solve_status_ok();
}

} // namespace random
} // namespace harc_rt
