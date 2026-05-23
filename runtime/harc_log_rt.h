// harc_log_rt.h — Logging helpers for generated HARC testbenches.

#pragma once

#include <cstdarg>
#include <cstdio>
#include <cstdlib>
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

inline std::string harc_wave_output_path(const char* default_name) {
    const char* wave_path = std::getenv("HARC_WAVE_FILE");
    if (wave_path && *wave_path) return std::string(wave_path);
    return harc_resolve_log_path(default_name);
}

inline std::string harc_coverage_output_path() {
    return harc_resolve_log_path("coverage.dat");
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
