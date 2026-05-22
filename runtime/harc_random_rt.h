// harc_random_rt.h — Runtime randomization scaffold for HARC.
//
// Phase 5A only defines the ABI-shaped boundary that generated C++ will
// eventually call. Current codegen still emits inline Z3 solving in
// cpp_tb.rs; these entrypoints are intentionally inert until the typed
// runtime backend takes ownership of solving.

#pragma once

#include <cstdint>

namespace harc_rt {
namespace random {

using harc_problem_id = uint32_t;
using harc_call_site_id = uint32_t;
using harc_call_iteration = uint64_t;
using harc_seed = uint64_t;

struct HarcSolveStatus {
    bool ok = true;
    const char* message = nullptr;
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

inline constexpr HarcSolveStatus harc_solve_status_ok() {
    return HarcSolveStatus{true, nullptr};
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
