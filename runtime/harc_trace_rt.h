// harc_trace_rt.h — Semantic trace writer for generated HARC testbenches.

#pragma once

#include <cstdio>
#include <cstdlib>
#include <cstdint>
#include <cstring>
#include <cerrno>
#include <mutex>
#include <set>
#include <string>

namespace harc_rt {

// Suite mode: with no `--test` / `HARC_TEST`, the generated `main()` runs
// every test of the suite in order (`harc_run_suite`). Each test still
// gets a fresh context; the per-test banner is replaced by one aggregate
// banner for the whole suite. Output files are shared across the tests of
// one process through `harc_output_open_mode`.
struct HarcSuiteRun {
    bool active = false;
    size_t index = 0;  // 1-based index of the running test while active
    const char* name = nullptr;  // running test's name while active
};

inline HarcSuiteRun& harc_suite_run() {
    static HarcSuiteRun state;
    return state;
}

// fopen mode for an output file (sim.log, the semantic trace, `logf`
// files, the coverage JSONL). During a whole-suite run, the first open of a
// path in this process truncates it ("w") and every later open appends
// ("a"): the suite accumulates every test's output, while a file first
// written by a later test still starts empty instead of appending to a
// previous invocation's. Outside a suite run (a selected test, or a
// harness driving runs itself) every run truncates, as before.
inline const char* harc_output_open_mode(const char* path) {
    if (!harc_suite_run().active) return "w";
    static std::mutex lock;
    static std::set<std::string> opened;
    std::lock_guard<std::mutex> guard(lock);
    return opened.insert(path ? path : "").second ? "w" : "a";
}

namespace trace {


inline std::string harc_trace_u128(unsigned __int128 value) {
    char digits[40];
    char* end = digits + sizeof(digits);
    char* p = end;
    do {
        *--p = static_cast<char>('0' + value % 10);
        value /= 10;
    } while (value != 0);
    return std::string(p, end);
}

inline uint64_t harc_wave_time(unsigned __int128 value) {
    // Verilator's waveform ABI is 64-bit. Saturate at its ceiling so a
    // long-running simulation never sends time backwards; semantic traces
    // retain the exact 128-bit decimal timestamp for merge and analysis.
    constexpr unsigned __int128 max = static_cast<unsigned __int128>(UINT64_MAX);
    return value > max ? UINT64_MAX : static_cast<uint64_t>(value);
}

inline std::string harc_trace_escape(const std::string& s) {
    std::string out;
    out.reserve(s.size() + 8);
    for (unsigned char c : s) {
        switch (c) {
            case '"': out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (c < 0x20) {
                    char buf[7];
                    std::snprintf(buf, sizeof(buf), "\\u%04x", c);
                    out += buf;
                } else {
                    out.push_back(static_cast<char>(c));
                }
        }
    }
    return out;
}

struct HarcTraceWriter {
    FILE* out = nullptr;
    uint64_t seq = 0;
    unsigned __int128 vcd_time = 0;
    uint64_t clock_cycle = 0;
    std::string clock;
    bool enabled = false;
    bool open_failed = false;

    void open_env() {
        const char* p = std::getenv("HARC_TRACE");
        if (p && *p) {
            out = std::fopen(p, harc_output_open_mode(p));
            enabled = (out != nullptr);
            if (!enabled) {
                open_failed = true;
                const int error = errno;
                std::fprintf(
                    stderr,
                    "harc: failed to open semantic trace '%s': %s\n",
                    p,
                    std::strerror(error));
            }
        }
    }

    void close() {
        if (out) {
            std::fflush(out);
            std::fclose(out);
            out = nullptr;
        }
        enabled = false;
    }

    uint64_t next_seq() { return seq++; }

    void set_timing(unsigned __int128 new_vcd_time, const char* new_clock, uint64_t new_clock_cycle) {
        vcd_time = new_vcd_time;
        clock = new_clock ? new_clock : "";
        clock_cycle = new_clock_cycle;
    }

    void meta(uint64_t seed, const char* backend, const char* top, const char* test) {
        if (!enabled) return;
        std::fprintf(
            out,
            "{\"type\":\"meta\",\"schema_version\":1,\"tool\":\"harc\",\"seed\":%llu,\"dut_backend\":\"%s\",\"top\":\"%s\",\"test\":\"%s\"}\n",
            static_cast<unsigned long long>(seed),
            backend ? backend : "unknown",
            top ? top : "",
            test ? test : "");
        std::fflush(out);
    }

    void meta_env(uint64_t seed, const char* top, const char* test) {
        meta(seed, std::getenv("HARC_DUT_BACKEND"), top, test);
    }

    void raw(const char* type, uint64_t cycle, const std::string& payload) {
        if (!enabled) return;
        const std::string vcd_time_decimal = harc_trace_u128(vcd_time);
        std::fprintf(
            out,
            "{\"type\":\"%s\",\"cycle\":%llu,\"seq\":%llu,\"vcd_time\":%s,\"clock\":\"%s\",\"clock_cycle\":%llu%s%s}\n",
            type,
            static_cast<unsigned long long>(cycle),
            static_cast<unsigned long long>(next_seq()),
            vcd_time_decimal.c_str(),
            harc_trace_escape(clock).c_str(),
            static_cast<unsigned long long>(clock_cycle),
            payload.empty() ? "" : ",",
            payload.c_str());
        std::fflush(out);
    }

    void sim_start(uint64_t cycle) {
        raw("sim_start", cycle, "");
    }

    void sim_end(uint64_t cycle, int errors) {
        raw("sim_end", cycle, "\"errors\":" + std::to_string(errors));
    }

    void randomize(uint64_t cycle, const std::string& fields_payload) {
        raw("randomize", cycle, fields_payload);
    }

    void tlm_call(
        uint64_t cycle,
        const char* component,
        const char* bus,
        const char* method,
        const char* phase,
        const char* direction,
        int64_t tag = -1) {
        std::string payload =
            "\"component\":\"" + harc_trace_escape(component ? component : "") +
            "\",\"bus\":\"" + harc_trace_escape(bus ? bus : "") +
            "\",\"method\":\"" + harc_trace_escape(method ? method : "") +
            "\",\"phase\":\"" + harc_trace_escape(phase ? phase : "") +
            "\",\"direction\":\"" + harc_trace_escape(direction ? direction : "") + "\"";
        if (tag >= 0) {
            payload += ",\"tag\":" + std::to_string(tag);
        }
        raw("tlm_call", cycle, payload);
    }

    void log(uint64_t cycle, const char* sev, const std::string& msg) {
        std::string payload =
            "\"severity\":\"" + harc_trace_escape(sev ? sev : "") +
            "\",\"message\":\"" + harc_trace_escape(msg) + "\"";
        raw("log", cycle, payload);
        if (sev && std::strcmp(sev, "FAIL") == 0) {
            std::string fail_payload =
                "\"failure_id\":\"fail\",\"message\":\"" + harc_trace_escape(msg) + "\"";
            raw("assertion_failure", cycle, fail_payload);
        }
    }
};

inline void harc_start_trace(
    HarcTraceWriter& trace,
    uint64_t seed,
    const char* top,
    const char* test,
    uint64_t cycle) {
    trace.open_env();
    trace.meta_env(seed, top, test);
    trace.sim_start(cycle);
}

inline void harc_finish_trace(HarcTraceWriter& trace, uint64_t cycle, int errors) {
    trace.sim_end(cycle, errors);
    trace.close();
}

} // namespace trace
} // namespace harc_rt
