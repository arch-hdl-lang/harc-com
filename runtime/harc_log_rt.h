// harc_log_rt.h — Logging helpers for generated HARC testbenches.

#pragma once

#include <cctype>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#include "harc_trace_rt.h"

namespace harc_rt {
namespace log {

template <typename DutT, typename ContextT>
inline DutT* harc_make_dut(ContextT* context) {
    if constexpr (requires { new DutT(context); }) {
        return new DutT(context);
    } else {
        return new DutT;
    }
}

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
    FILE* f = std::fopen(path, harc_output_open_mode(path));
    const HarcSuiteRun& suite = harc_suite_run();
    if (f && suite.active && suite.name) {
        std::fprintf(f, "[suite] test %s\n", suite.name);
    }
    return f;
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

inline const char* harc_wave_default_name() {
#if defined(HARC_TRACE_VCD)
    return "waves.vcd";
#elif defined(HARC_TRACE_FST)
    return "waves.fst";
#else
    return "waves.vcd";
#endif
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
    if (harc_suite_run().active) {
        // Suite mode: one line per test; `harc_run_suite` prints the
        // aggregate banner once every test has run.
        if (errors == 0) {
            std::printf("[suite] test PASSED\n");
        } else {
            std::printf("[suite] test FAILED (%d error(s))\n", errors);
        }
        return errors == 0 ? 0 : 1;
    }
    if (errors == 0) {
        std::printf("\nALL TESTS PASSED\n");
        return 0;
    }
    std::printf("\n%d TESTS FAILED\n", errors);
    return 1;
}

// Run every test in order (no `--test` / `HARC_TEST` given) and print one
// banner covering all of them: `ALL TESTS PASSED` only when every test
// passed, otherwise `<N> TESTS FAILED` with N the number of failing tests.
template <typename NameFn, typename RunFn>
inline int harc_run_suite(
    int argc,
    char** argv,
    size_t count,
    NameFn name_at,
    RunFn run_at) {
    HarcSuiteRun& s = harc_suite_run();
    s.active = true;
    std::vector<std::string> failed;
    for (size_t i = 0; i < count; ++i) {
        s.index = i + 1;
        const char* name = name_at(i);
        s.name = name;
        std::printf("[suite] running test %s (%zu/%zu)\n", name, i + 1, count);
        std::fflush(stdout);
        if (run_at(i, argc, argv) != 0) failed.push_back(name);
    }
    s.active = false;
    s.index = 0;
    s.name = nullptr;
    std::printf("\n[suite] %zu test(s) run: %zu passed, %zu failed", count,
                count - failed.size(), failed.size());
    for (size_t i = 0; i < failed.size(); ++i) {
        std::printf("%s%s", i == 0 ? " (" : ", ", failed[i].c_str());
    }
    std::printf("%s\n", failed.empty() ? "" : ")");
    if (failed.empty()) {
        std::printf("\nALL TESTS PASSED\n");
        return 0;
    }
    std::printf("\n%zu TESTS FAILED\n", failed.size());
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

inline FILE* harc_open_coverage_json() {
    const char* path = std::getenv("HARC_COVERAGE_JSONL");
    if (!path || !*path) path = std::getenv("HARC_COVERAGE_JSON");
    if (!path || !*path) return nullptr;
    return std::fopen(path, harc_output_open_mode(path));
}

inline void harc_json_string(FILE* f, const char* value) {
    std::fputc('"', f);
    const unsigned char* p = reinterpret_cast<const unsigned char*>(value ? value : "");
    while (*p) {
        unsigned char c = *p++;
        switch (c) {
            case '"': std::fputs("\\\"", f); break;
            case '\\': std::fputs("\\\\", f); break;
            case '\b': std::fputs("\\b", f); break;
            case '\f': std::fputs("\\f", f); break;
            case '\n': std::fputs("\\n", f); break;
            case '\r': std::fputs("\\r", f); break;
            case '\t': std::fputs("\\t", f); break;
            default:
                if (c < 0x20) {
                    std::fprintf(f, "\\u%04x", static_cast<unsigned>(c));
                } else {
                    std::fputc(c, f);
                }
                break;
        }
    }
    std::fputc('"', f);
}

inline void harc_cov_json_cover_summary(
    FILE* f,
    uint64_t hit,
    uint64_t total) {
    if (!f) return;
    std::fprintf(
        f,
        "{\"type\":\"cover\",\"hit\":%llu,\"total\":%llu}\n",
        static_cast<unsigned long long>(hit),
        static_cast<unsigned long long>(total));
    std::fflush(f);
}

inline void harc_cov_json_cover_point(FILE* f, const char* label, uint64_t hits) {
    if (!f) return;
    std::fputs("{\"type\":\"cover_point\",\"label\":", f);
    harc_json_string(f, label);
    std::fprintf(f, ",\"hits\":%llu}\n", static_cast<unsigned long long>(hits));
    std::fflush(f);
}

inline void harc_cov_json_auto_summary(
    FILE* f,
    const char* type_name,
    uint64_t span,
    uint64_t hit,
    uint64_t total,
    uint64_t blocked) {
    if (!f) return;
    std::fputs("{\"type\":\"auto_cover\",\"record\":", f);
    harc_json_string(f, type_name);
    std::fprintf(
        f,
        ",\"span\":%llu,\"hit\":%llu,\"total\":%llu,\"blocked\":%llu}\n",
        static_cast<unsigned long long>(span),
        static_cast<unsigned long long>(hit),
        static_cast<unsigned long long>(total),
        static_cast<unsigned long long>(blocked));
    std::fflush(f);
}

inline void harc_cov_json_auto_bin(
    FILE* f,
    const char* type_name,
    uint64_t span,
    const char* label,
    bool hit,
    bool blocked) {
    if (!f) return;
    std::fputs("{\"type\":\"auto_cover_bin\",\"record\":", f);
    harc_json_string(f, type_name);
    std::fprintf(
        f, ",\"span\":%llu,\"label\":", static_cast<unsigned long long>(span));
    harc_json_string(f, label);
    std::fprintf(
        f,
        ",\"hit\":%s,\"blocked\":%s}\n",
        hit ? "true" : "false",
        blocked ? "true" : "false");
    std::fflush(f);
}

inline void harc_cov_json_summary(
    FILE* f,
    const char* group,
    uint64_t hit,
    uint64_t total) {
    if (!f) return;
    std::fputs("{\"type\":\"covergroup\",\"group\":", f);
    harc_json_string(f, group);
    std::fprintf(
        f,
        ",\"hit\":%llu,\"total\":%llu}\n",
        static_cast<unsigned long long>(hit),
        static_cast<unsigned long long>(total));
    std::fflush(f);
}

inline void harc_cov_json_bin(
    FILE* f,
    const char* group,
    const char* point,
    const char* bin,
    uint64_t hits) {
    if (!f) return;
    std::fputs("{\"type\":\"coverpoint_bin\",\"group\":", f);
    harc_json_string(f, group);
    std::fputs(",\"point\":", f);
    harc_json_string(f, point);
    std::fputs(",\"bin\":", f);
    harc_json_string(f, bin);
    std::fprintf(f, ",\"hits\":%llu}\n", static_cast<unsigned long long>(hits));
    std::fflush(f);
}

inline void harc_cov_json_cross_summary(
    FILE* f,
    const char* group,
    const char* kind,
    const char* label,
    uint64_t hit,
    uint64_t total) {
    if (!f) return;
    std::fputs("{\"type\":\"cross\",\"group\":", f);
    harc_json_string(f, group);
    std::fputs(",\"kind\":", f);
    harc_json_string(f, kind);
    std::fputs(",\"label\":", f);
    harc_json_string(f, label);
    std::fprintf(
        f,
        ",\"hit\":%llu,\"total\":%llu}\n",
        static_cast<unsigned long long>(hit),
        static_cast<unsigned long long>(total));
    std::fflush(f);
}

inline void harc_cov_json_cross_bin(
    FILE* f,
    const char* group,
    const char* kind,
    const char* label,
    const char* bin,
    uint64_t hits) {
    if (!f) return;
    std::fputs("{\"type\":\"cross_bin\",\"group\":", f);
    harc_json_string(f, group);
    std::fputs(",\"kind\":", f);
    harc_json_string(f, kind);
    std::fputs(",\"label\":", f);
    harc_json_string(f, label);
    std::fputs(",\"bin\":", f);
    harc_json_string(f, bin);
    std::fprintf(f, ",\"hits\":%llu}\n", static_cast<unsigned long long>(hits));
    std::fflush(f);
}

struct HarcLogFiles {
    std::unordered_map<std::string, FILE*> files;

    FILE* get(const char* path) {
        std::string resolved = harc_resolve_log_path(path);
        auto it = files.find(resolved);
        if (it != files.end()) return it->second;
        FILE* f = std::fopen(resolved.c_str(), harc_output_open_mode(resolved.c_str()));
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

struct HarcLogContext {
    FILE* sim_log = harc_open_sim_log();
    FILE* coverage_json = harc_open_coverage_json();
    HarcLogFiles files;

    FILE* file(const char* path) {
        return files.get(path);
    }

    void close_all() {
        harc_close_file(sim_log);
        harc_close_file(coverage_json);
        files.close_all();
    }
};

inline int harc_finish_sim_run(
    HarcLogContext& log,
    harc_rt::trace::HarcTraceWriter& trace,
    uint64_t cycle,
    int errors) {
    log.close_all();
    harc_rt::trace::harc_finish_trace(trace, cycle, errors);
    if (trace.open_failed && errors == 0) errors = 1;
    return harc_report_test_result(errors);
}

// Verbosity (spec §7.7). Levels rank LOW < MEDIUM < HIGH < DEBUG < FULL.
// A message prints when its verbosity is at or below its threshold. The
// verbosity is the call's `verbosity=` when given, else the severity's
// default (warn/info MEDIUM, debug HIGH). The threshold is the
// `--verbosity-of ID=LEVEL` entry matching the call's `id=` exactly
// (`HARC_VERBOSITY_OF`, comma-separated), else `--verbosity`
// (`HARC_VERBOSITY`), else MEDIUM. error and fatal (and any other tag, e.g.
// assertion FAIL lines) always print.
enum HarcVerbosity : int { HARC_V_LOW = 0, HARC_V_MEDIUM, HARC_V_HIGH, HARC_V_DEBUG, HARC_V_FULL };

// Level name (case-insensitive) in [begin, end) -> rank, or -1.
inline int harc_verbosity_rank(const char* begin, const char* end) {
    static const char* const names[] = {"LOW", "MEDIUM", "HIGH", "DEBUG", "FULL"};
    for (int i = 0; i < 5; ++i) {
        const char* a = begin;
        const char* b = names[i];
        while (a < end && *b && std::toupper(static_cast<unsigned char>(*a)) == *b) {
            ++a;
            ++b;
        }
        if (a == end && !*b) return i;
    }
    return -1;
}

inline int harc_verbosity_threshold() {
    static const int threshold = [] {
        const char* v = std::getenv("HARC_VERBOSITY");
        if (!v || !*v) return static_cast<int>(HARC_V_MEDIUM);
        int rank = harc_verbosity_rank(v, v + std::strlen(v));
        if (rank >= 0) return rank;
        std::fprintf(
            stderr,
            "harc: unknown HARC_VERBOSITY '%s' (expected LOW, MEDIUM, HIGH, DEBUG or FULL); using MEDIUM\n",
            v);
        return static_cast<int>(HARC_V_MEDIUM);
    }();
    return threshold;
}

// `HARC_VERBOSITY_OF=id1=LEVEL,id2=LEVEL` parsed once; a malformed entry
// warns and is ignored.
inline const std::vector<std::pair<std::string, int>>& harc_verbosity_of() {
    static const std::vector<std::pair<std::string, int>> entries = [] {
        std::vector<std::pair<std::string, int>> out;
        const char* v = std::getenv("HARC_VERBOSITY_OF");
        if (!v) return out;
        std::string all(v);
        size_t start = 0;
        while (start <= all.size()) {
            size_t comma = all.find(',', start);
            if (comma == std::string::npos) comma = all.size();
            std::string entry = all.substr(start, comma - start);
            if (!entry.empty()) {
                size_t eq = entry.rfind('=');
                int rank = eq == std::string::npos
                    ? -1
                    : harc_verbosity_rank(entry.c_str() + eq + 1, entry.c_str() + entry.size());
                if (eq == 0 || rank < 0) {
                    std::fprintf(
                        stderr,
                        "harc: ignoring malformed HARC_VERBOSITY_OF entry '%s' (expected ID=LEVEL)\n",
                        entry.c_str());
                } else {
                    out.emplace_back(entry.substr(0, eq), rank);
                }
            }
            start = comma + 1;
        }
        return out;
    }();
    return entries;
}

// Per-message attributes of the log call being executed (set by generated
// code through `HarcLogAttrScope` around a `log(..., verbosity=, id=)`).
struct HarcLogAttrs {
    int verbosity = -1;        // -1: the severity's default
    const char* id = nullptr;  // nullptr: no `id=`
};

inline HarcLogAttrs& harc_current_log_attrs() {
    thread_local HarcLogAttrs attrs;
    return attrs;
}

struct HarcLogAttrScope {
    HarcLogAttrs saved;
    HarcLogAttrScope(int verbosity, const char* id) : saved(harc_current_log_attrs()) {
        harc_current_log_attrs() = HarcLogAttrs{verbosity, id};
    }
    ~HarcLogAttrScope() { harc_current_log_attrs() = saved; }
    HarcLogAttrScope(const HarcLogAttrScope&) = delete;
    HarcLogAttrScope& operator=(const HarcLogAttrScope&) = delete;
};

inline bool harc_log_enabled(const char* sev) {
    if (!sev) return true;
    int level;
    if (std::strcmp(sev, "DEBUG") == 0) {
        level = HARC_V_HIGH;
    } else if (std::strcmp(sev, "INFO") == 0 || std::strcmp(sev, "WARN") == 0) {
        level = HARC_V_MEDIUM;
    } else {
        return true;
    }
    const HarcLogAttrs& attrs = harc_current_log_attrs();
    if (attrs.verbosity >= 0) level = attrs.verbosity;
    int threshold = harc_verbosity_threshold();
    if (attrs.id) {
        for (const auto& entry : harc_verbosity_of()) {
            if (entry.first == attrs.id) threshold = entry.second;
        }
    }
    return level <= threshold;
}

inline void harc_log_stdout_line(uint64_t cycle, const char* sev, const char* msg) {
    std::printf("[cycle:%llu %s] ", static_cast<unsigned long long>(cycle), sev ? sev : "");
    std::printf("%s", msg ? msg : "");
    std::printf("\n");
}

inline void harc_log_file_line(FILE* f, uint64_t cycle, const char* sev, const char* msg) {
    if (!f) return;
    std::fprintf(f, "[cycle:%llu %s] ", static_cast<unsigned long long>(cycle), sev ? sev : "");
    std::fprintf(f, "%s", msg ? msg : "");
    std::fprintf(f, "\n");
    std::fflush(f);
}

inline void harc_log_line(
    FILE* sim_log,
    harc_rt::trace::HarcTraceWriter* trace,
    uint64_t cycle,
    const char* sev,
    const char* msg) {
    // Below-threshold messages are not printed, but the semantic trace
    // keeps every log event so trace diffs do not depend on verbosity.
    if (harc_log_enabled(sev)) {
        harc_log_stdout_line(cycle, sev, msg);
        harc_log_file_line(sim_log, cycle, sev, msg);
    }
    if (trace) trace->log(cycle, sev, msg ? msg : "");
}

inline void harc_log_vline(
    FILE* sim_log,
    harc_rt::trace::HarcTraceWriter* trace,
    uint64_t cycle,
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

inline void harc_log_file_only_line(FILE* f, uint64_t cycle, const char* sev, const char* msg) {
    if (!harc_log_enabled(sev)) return;
    harc_log_stdout_line(cycle, sev, msg);
    harc_log_file_line(f, cycle, sev, msg);
}

inline void harc_log_file_only_vline(
    FILE* f,
    uint64_t cycle,
    const char* sev,
    const char* fmt,
    va_list ap) {
    if (!harc_log_enabled(sev)) return;
    std::printf("[cycle:%llu %s] ", static_cast<unsigned long long>(cycle), sev ? sev : "");
    va_list stdout_ap;
    va_copy(stdout_ap, ap);
    std::vprintf(fmt ? fmt : "", stdout_ap);
    va_end(stdout_ap);
    std::printf("\n");

    if (!f) return;
    std::fprintf(f, "[cycle:%llu %s] ", static_cast<unsigned long long>(cycle), sev ? sev : "");
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

#if VM_COVERAGE
#define HARC_RT_WRITE_COVERAGE(coverage) \
    do { \
        harc_rt::log::harc_write_coverage(coverage); \
    } while (0)
#else
#define HARC_RT_WRITE_COVERAGE(coverage) \
    do { \
    } while (0)
#endif

#if HARC_TRACE_ENABLED
#define HARC_RT_LOG_WAVE_FILE(file, path) \
    do { \
        harc_rt::log::harc_log_wave_file(file, path); \
    } while (0)
#define HARC_RT_DUMP_WAVE_TRACE(trace, timestamp) \
    do { \
        harc_rt::log::harc_dump_wave_trace(trace, timestamp); \
    } while (0)
#define HARC_RT_CLOSE_WAVE_TRACE(trace) \
    do { \
        harc_rt::log::harc_close_wave_trace(trace); \
    } while (0)
#else
#define HARC_RT_LOG_WAVE_FILE(file, path) \
    do { \
    } while (0)
#define HARC_RT_DUMP_WAVE_TRACE(trace, timestamp) \
    do { \
    } while (0)
#define HARC_RT_CLOSE_WAVE_TRACE(trace) \
    do { \
    } while (0)
#endif
