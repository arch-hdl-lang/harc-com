// harc_trace_rt.h — Semantic trace writer for generated HARC testbenches.

#pragma once

#include <cstdio>
#include <cstdlib>
#include <cstdint>
#include <cstring>
#include <string>

namespace harc_rt {
namespace trace {

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

    void raw(const char* type, int cycle, const std::string& payload) {
        if (!enabled) return;
        std::fprintf(
            out,
            "{\"type\":\"%s\",\"cycle\":%d,\"seq\":%llu%s%s}\n",
            type,
            cycle,
            static_cast<unsigned long long>(next_seq()),
            payload.empty() ? "" : ",",
            payload.c_str());
        std::fflush(out);
    }

    void sim_start(int cycle) {
        raw("sim_start", cycle, "");
    }

    void sim_end(int cycle, int errors) {
        raw("sim_end", cycle, "\"errors\":" + std::to_string(errors));
    }

    void log(int cycle, const char* sev, const std::string& msg) {
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

} // namespace trace
} // namespace harc_rt
