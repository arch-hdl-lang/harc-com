// harc_log_rt.h — Logging helpers for generated HARC testbenches.

#pragma once

#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <unordered_map>

#include "harc_trace_rt.h"

namespace harc_rt {
namespace log {

inline std::string harc_resolve_log_path(const char* path) {
    if (path && path[0] == '/') return std::string(path);
    const char* base = std::getenv("HARC_LOG_DIR");
    if (base && *base) return std::string(base) + "/" + (path ? path : "");
    return std::string(path ? path : "");
}

inline FILE* harc_open_sim_log(
    const char* env_name = "HARC_SIM_LOG",
    const char* fallback_path = "sim.log") {
    const char* path = std::getenv(env_name);
    if (!path || !*path) path = fallback_path;
    return std::fopen(path, "w");
}

inline void harc_close_file(FILE*& f) {
    if (!f) return;
    std::fclose(f);
    f = nullptr;
}

inline std::string harc_wave_output_path(const char* default_name) {
    const char* wave_path = std::getenv("HARC_WAVE_FILE");
    if (wave_path && *wave_path) return std::string(wave_path);
    return harc_resolve_log_path(default_name);
}

inline int harc_trace_depth(
    const char* env_name = "HARC_TRACE_DEPTH",
    int fallback_depth = 99) {
    const char* depth = std::getenv(env_name);
    if (depth && *depth) return std::atoi(depth);
    return fallback_depth;
}

inline void harc_log_wave_stderr(const std::string& wave_path) {
    std::fprintf(stderr, "[waves] writing %s\n", wave_path.c_str());
}

inline void harc_log_wave_file(FILE* f, const std::string& wave_path) {
    if (!f) return;
    std::fprintf(f, "[waves] writing %s\n", wave_path.c_str());
}

template <typename DutT, typename TraceT>
inline std::string harc_open_wave_trace(
    DutT* dut,
    TraceT* trace,
    const char* default_name) {
    if (dut && trace) dut->trace(trace, harc_trace_depth());
    std::string wave_path = harc_wave_output_path(default_name);
    if (trace) trace->open(wave_path.c_str());
    harc_log_wave_stderr(wave_path);
    return wave_path;
}

template <typename TraceT>
inline void harc_dump_wave_trace(TraceT* trace, uint64_t timestamp) {
    if (!trace) return;
    trace->dump(timestamp);
}

inline void harc_report_unknown_test(const char* test_name, const char* available_tests) {
    std::fprintf(
        stderr,
        "unknown test: %s (available: %s)\n",
        test_name ? test_name : "",
        available_tests ? available_tests : "");
}

inline const char* harc_select_test(
    int argc,
    char** argv,
    const char* env_name = "HARC_TEST") {
    const char* test_sel = std::getenv(env_name);
    for (int i = 1; i + 1 < argc; i++) {
        if (std::strcmp(argv[i], "--test") == 0) return argv[i + 1];
    }
    return test_sel;
}

inline int harc_report_test_result(int errors) {
    if (errors == 0) {
        std::printf("\nALL TESTS PASSED\n");
        return 0;
    }
    std::printf("\n%d TESTS FAILED\n", errors);
    return 1;
}

inline std::string harc_coverage_output_path() {
    return harc_resolve_log_path("coverage.dat");
}

template <typename CoverageT>
inline void harc_write_coverage(CoverageT* coverage) {
    if (!coverage) return;
    coverage->write(harc_coverage_output_path().c_str());
}

template <typename TraceT>
inline void harc_close_wave_trace(TraceT*& trace) {
    if (!trace) return;
    trace->close();
    delete trace;
    trace = nullptr;
}

inline double harc_percent(uint64_t hit, uint64_t total) {
    return total ? (100.0 * hit / total) : 0.0;
}

inline void harc_print_cover_summary(uint64_t hit, uint64_t total) {
    std::printf(
        "[cover] %llu/%llu hit (%.1f%%)\n",
        static_cast<unsigned long long>(hit),
        static_cast<unsigned long long>(total),
        harc_percent(hit, total));
}

inline void harc_print_cover_point(const char* label, uint64_t hits) {
    std::printf(
        "  [%s]: %llu hits%s\n",
        label ? label : "",
        static_cast<unsigned long long>(hits),
        hits ? "" : " *NOT HIT*");
}

inline void harc_print_covergroup_summary(const char* group, uint64_t hit, uint64_t total) {
    std::printf(
        "[%s] coverage: %llu/%llu hit (%.1f%%)\n",
        group ? group : "",
        static_cast<unsigned long long>(hit),
        static_cast<unsigned long long>(total),
        harc_percent(hit, total));
}

inline void harc_print_covergroup_bin(const char* point, const char* bin, uint64_t hits) {
    std::printf(
        "  %s (bin) [%s]: %llu hits%s\n",
        point ? point : "",
        bin ? bin : "",
        static_cast<unsigned long long>(hits),
        hits ? "" : " *NOT HIT*");
}

inline void harc_print_covergroup_cross_summary(
    const char* group,
    const char* kind,
    const char* label,
    uint64_t hit,
    uint64_t total) {
    std::printf(
        "[%s] %s %s: %llu/%llu hit (%.1f%%)\n",
        group ? group : "",
        kind ? kind : "",
        label ? label : "",
        static_cast<unsigned long long>(hit),
        static_cast<unsigned long long>(total),
        harc_percent(hit, total));
}

inline void harc_print_covergroup_missing_bin(const char* label) {
    std::printf("  %s: *NOT HIT*\n", label ? label : "");
}

inline void harc_print_covergroup_more_missing(
    uint64_t missing,
    uint64_t detail_limit,
    const char* kind) {
    if (missing > detail_limit) {
        std::printf(
            "  ... %llu more missing %s bins\n",
            static_cast<unsigned long long>(missing - detail_limit),
            kind ? kind : "");
    }
}

struct HarcLogFiles {
    std::unordered_map<std::string, FILE*> files;

    FILE* get(const char* path) {
        std::string resolved = harc_resolve_log_path(path);
        auto it = files.find(resolved);
        if (it != files.end()) return it->second;
        FILE* f = std::fopen(resolved.c_str(), "w");
        files[resolved] = f;
        return f;
    }

    void close_all() {
        for (auto& kv : files) {
            if (kv.second) std::fclose(kv.second);
            kv.second = nullptr;
        }
        files.clear();
    }
};

inline int harc_finish_sim_run(
    FILE*& sim_log,
    HarcLogFiles& log_files,
    harc_rt::trace::HarcTraceWriter& trace,
    int cycle,
    int errors) {
    harc_close_file(sim_log);
    log_files.close_all();
    trace.sim_end(cycle, errors);
    trace.close();
    return harc_report_test_result(errors);
}

inline void harc_log_stdout_line(int cycle, const char* sev, const char* msg) {
    std::printf("[cycle:%d %s] ", cycle, sev ? sev : "");
    std::printf("%s", msg ? msg : "");
    std::printf("\n");
}

inline void harc_log_file_line(FILE* f, int cycle, const char* sev, const char* msg) {
    if (!f) return;
    std::fprintf(f, "[cycle:%d %s] ", cycle, sev ? sev : "");
    std::fprintf(f, "%s", msg ? msg : "");
    std::fprintf(f, "\n");
    std::fflush(f);
}

inline void harc_log_line(
    FILE* sim_log,
    harc_rt::trace::HarcTraceWriter* trace,
    int cycle,
    const char* sev,
    const char* msg) {
    harc_log_stdout_line(cycle, sev, msg);
    harc_log_file_line(sim_log, cycle, sev, msg);
    if (trace) trace->log(cycle, sev, msg ? msg : "");
}

inline void harc_log_vline(
    FILE* sim_log,
    harc_rt::trace::HarcTraceWriter* trace,
    int cycle,
    const char* sev,
    const char* fmt,
    va_list ap) {
    char msg[4096];
    va_list msg_ap;
    va_copy(msg_ap, ap);
    std::vsnprintf(msg, sizeof(msg), fmt ? fmt : "", msg_ap);
    va_end(msg_ap);
    harc_log_line(sim_log, trace, cycle, sev, msg);
}

inline void harc_log_file_only_line(FILE* f, int cycle, const char* sev, const char* msg) {
    harc_log_stdout_line(cycle, sev, msg);
    harc_log_file_line(f, cycle, sev, msg);
}

inline void harc_log_file_only_vline(
    FILE* f,
    int cycle,
    const char* sev,
    const char* fmt,
    va_list ap) {
    std::printf("[cycle:%d %s] ", cycle, sev ? sev : "");
    va_list stdout_ap;
    va_copy(stdout_ap, ap);
    std::vprintf(fmt ? fmt : "", stdout_ap);
    va_end(stdout_ap);
    std::printf("\n");

    if (!f) return;
    std::fprintf(f, "[cycle:%d %s] ", cycle, sev ? sev : "");
    va_list file_ap;
    va_copy(file_ap, ap);
    std::vfprintf(f, fmt ? fmt : "", file_ap);
    va_end(file_ap);
    std::fprintf(f, "\n");
    std::fflush(f);
}

} // namespace log
} // namespace harc_rt

// These macros must run inside a variadic function or lambda; C++ cannot
// portably forward `...` through a normal helper without first materializing
// a va_list at the original call frame.
#define HARC_RT_LOG_PRINTF(sim_log, trace, cycle, sev, fmt) \
    do { \
        va_list _harc_log_ap; \
        va_start(_harc_log_ap, fmt); \
        harc_rt::log::harc_log_vline(sim_log, trace, cycle, sev, fmt, _harc_log_ap); \
        va_end(_harc_log_ap); \
    } while (0)

#define HARC_RT_LOG_FILE_ONLY_PRINTF(file, cycle, sev, fmt) \
    do { \
        va_list _harc_log_ap; \
        va_start(_harc_log_ap, fmt); \
        harc_rt::log::harc_log_file_only_vline(file, cycle, sev, fmt, _harc_log_ap); \
        va_end(_harc_log_ap); \
    } while (0)
