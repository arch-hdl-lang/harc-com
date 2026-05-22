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

enum class HarcSolveMode : uint8_t {
    Inline,
    Queued,
    Blocking,
};

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

