// harc_random_rt.h — Runtime randomization scaffold for HARC.
//
// Phase 5A only defines the ABI-shaped boundary that generated C++ will
// eventually call. Current codegen still emits inline Z3 solving in
// cpp_tb.rs; these entrypoints are intentionally inert until the typed
// runtime backend takes ownership of solving.

#pragma once

#include <initializer_list>
#include <cstddef>
#include <cstdint>
#include <vector>

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

template <size_t N>
inline bool harc_auto_cov_first_uncovered(
    const bool (&hit)[N],
    const bool (&blocked)[N],
    size_t& i) {
    for (size_t idx = 0; idx < N; ++idx) {
        if (!hit[idx] && !blocked[idx]) {
            i = idx;
            return true;
        }
    }
    return false;
}

template <size_t Rows, size_t Cols>
inline bool harc_auto_cov_first_uncovered_cross(
    const bool (&hit)[Rows][Cols],
    const bool (&blocked)[Rows][Cols],
    size_t& i,
    size_t& j) {
    for (size_t row = 0; row < Rows; ++row) {
        for (size_t col = 0; col < Cols; ++col) {
            if (!hit[row][col] && !blocked[row][col]) {
                i = row;
                j = col;
                return true;
            }
        }
    }
    return false;
}

template <size_t N>
inline uint64_t harc_auto_cov_count(const bool (&bins)[N]) {
    uint64_t count = 0;
    for (size_t i = 0; i < N; ++i) {
        if (bins[i]) ++count;
    }
    return count;
}

template <size_t Rows, size_t Cols>
inline uint64_t harc_auto_cov_count(const bool (&bins)[Rows][Cols]) {
    uint64_t count = 0;
    for (size_t i = 0; i < Rows; ++i) {
        for (size_t j = 0; j < Cols; ++j) {
            if (bins[i][j]) ++count;
        }
    }
    return count;
}

inline constexpr const char* harc_auto_cov_state(
    bool hit,
    bool blocked) {
    return hit ? "hit" : (blocked ? "*BLOCKED*" : "*NOT HIT*");
}

inline void harc_auto_cov_mark_blocked(bool& blocked) {
    blocked = true;
}

inline void harc_auto_cov_mark_hit(bool& hit, bool& blocked) {
    hit = true;
    blocked = false;
}

inline void harc_auto_cov_mark_selected_point_blocked(
    const HarcAutoCovSelection& selection,
    int group,
    bool& blocked) {
    if (harc_auto_cov_selected_point(selection, group)) {
        harc_auto_cov_mark_blocked(blocked);
    }
}

inline void harc_auto_cov_mark_selected_cross_blocked(
    const HarcAutoCovSelection& selection,
    int group,
    bool& blocked) {
    if (harc_auto_cov_selected_cross(selection, group)) {
        harc_auto_cov_mark_blocked(blocked);
    }
}

template <typename T, typename U>
inline void harc_auto_cov_mark_value_hit(
    const T& value,
    const U& expected,
    bool& hit,
    bool& blocked) {
    if (value == expected) {
        harc_auto_cov_mark_hit(hit, blocked);
    }
}

template <typename A, typename B, typename ExpectedA, typename ExpectedB>
inline void harc_auto_cov_mark_cross_hit(
    const A& a,
    const ExpectedA& expected_a,
    const B& b,
    const ExpectedB& expected_b,
    bool& hit,
    bool& blocked) {
    if (a == expected_a && b == expected_b) {
        harc_auto_cov_mark_hit(hit, blocked);
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
