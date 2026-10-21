// harc_trace_rt.h — Semantic trace writer for generated HARC testbenches.

#pragma once

#include <cstdio>
#include <cstdlib>
#include <cstdint>
#include <cstring>
#include <string>

namespace harc_rt {
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

    void open_env() {
        const char* p = std::getenv("HARC_TRACE");
        if (p && *p) {
            out = std::fopen(p, "w");
            enabled = (out != nullptr);
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
