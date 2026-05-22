// harc_random_rt.h — Runtime randomization scaffold for HARC.
//
// Phase 5A only defines the ABI-shaped boundary that generated C++ will
// eventually call. Current codegen still emits inline Z3 solving in
// cpp_tb.rs; these entrypoints are intentionally inert until the typed
// runtime backend takes ownership of solving.

#pragma once

#include <initializer_list>
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

struct HarcDistBin {
    int64_t lo = 0;
    int64_t hi = 0;
    int64_t weight = 0;
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
