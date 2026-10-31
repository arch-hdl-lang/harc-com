//! CI-runnable (C++ compiler only, no Verilator) coverage of the shipped
//! log runtime: the `--verbosity` / `--verbosity-of` / per-message
//! `verbosity=` print filter (`harc_log_enabled`), the whole-suite runner
//! (`harc_run_suite`) and its banner, and the per-process output-file open
//! mode a suite run shares across tests (`harc_output_open_mode`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Same lookup and skip contract as `tests/queue_pop_guard_cpp.rs`.
fn cxx() -> Option<String> {
    for cc in [
        std::env::var("CXX").unwrap_or_default().as_str(),
        "g++",
        "clang++",
    ] {
        if cc.is_empty() {
            continue;
        }
        if Command::new(cc)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Some(cc.to_string());
        }
    }
    None
}

const PROBE: &str = r#"#include "harc_log_rt.h"
#include <cstdio>
#include <cstring>

using namespace harc_rt::log;

static void enabled(const char* label, const char* sev, int verbosity, const char* id) {
    HarcLogAttrScope scope(verbosity, id);
    std::printf("%s=%d\n", label, harc_log_enabled(sev) ? 1 : 0);
}

// Two fake tests: A passes and writes nothing to b.log; B writes one line
// to b.log through the logf file table and fails when argv asks it to.
static bool fail_b = false;
static int run_a(int, char**) {
    HarcLogContext ctx;
    ctx.close_all();
    return harc_report_test_result(0);
}
static int run_b(int, char**) {
    HarcLogContext ctx;
    FILE* f = ctx.file("b.log");
    if (f) std::fprintf(f, "B\n");
    ctx.close_all();
    return harc_report_test_result(fail_b ? 1 : 0);
}

int main(int argc, char** argv) {
    const char* mode = argc > 1 ? argv[1] : "";
    if (std::strcmp(mode, "filter") == 0) {
        enabled("debug", "DEBUG", -1, nullptr);
        enabled("info", "INFO", -1, nullptr);
        enabled("warn", "WARN", -1, nullptr);
        enabled("error", "ERROR", -1, nullptr);
        enabled("fatal", "FATAL", -1, nullptr);
        enabled("fail", "FAIL", -1, nullptr);
        enabled("info_high", "INFO", HARC_V_HIGH, nullptr);
        enabled("debug_low", "DEBUG", HARC_V_LOW, nullptr);
        enabled("error_full", "ERROR", HARC_V_FULL, nullptr);
        enabled("drv_debug", "DEBUG", -1, "AXI_DRV");
        enabled("drv_info", "INFO", -1, "AXI_DRV");
        enabled("mon_debug", "DEBUG", -1, "MON");
        // Scope restores the outer attributes.
        {
            HarcLogAttrScope outer(HARC_V_LOW, nullptr);
            { HarcLogAttrScope inner(HARC_V_FULL, nullptr); }
            std::printf("restored=%d\n", harc_current_log_attrs().verbosity);
        }
        std::printf("cleared=%d\n", harc_current_log_attrs().verbosity);
        return 0;
    }
    if (std::strcmp(mode, "suite") == 0) {
        fail_b = argc > 2 && std::strcmp(argv[2], "fail") == 0;
        static const char* const names[] = {"A", "B"};
        static int (*const fns[])(int, char**) = {&run_a, &run_b};
        return harc_run_suite(argc, argv, 2,
            [](size_t i) { return names[i]; },
            [](size_t i, int c, char** v) { return fns[i](c, v); });
    }
    return 2;
}
"#;

struct Probe {
    bin: PathBuf,
    dir: PathBuf,
}

fn build_probe(tag: &str) -> Option<Probe> {
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH (tried $CXX, g++, clang++); install one or set \
             HARC_SKIP_CXX_PROBE=1 to skip deliberately"
        );
        eprintln!("SKIP {tag}: HARC_SKIP_CXX_PROBE set");
        return None;
    };
    let dir = std::env::temp_dir().join(format!("harc_log_suite_rt_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let src = dir.join("probe.cpp");
    std::fs::write(&src, PROBE).expect("write probe");
    let bin = dir.join("probe");
    let build = Command::new(&cxx)
        .arg("-std=gnu++20")
        .arg("-I")
        .arg(repo_root().join("runtime"))
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("run C++ compiler");
    assert!(
        build.status.success(),
        "log runtime probe did not compile:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    Some(Probe { bin, dir })
}

fn run(probe: &Probe, cwd: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(&probe.bin);
    cmd.args(args)
        .current_dir(cwd)
        .env_remove("HARC_VERBOSITY")
        .env_remove("HARC_VERBOSITY_OF")
        .env_remove("HARC_SIM_LOG")
        .env_remove("HARC_LOG_DIR")
        .env_remove("HARC_TRACE")
        .env_remove("HARC_COVERAGE_JSONL")
        .env_remove("HARC_COVERAGE_JSON");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run probe")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn log_filter_honours_verbosity_verbosity_of_and_per_message_overrides() {
    let Some(probe) = build_probe("filter") else {
        return;
    };
    let filter = |env: &[(&str, &str)]| {
        let out = run(&probe, &probe.dir, &["filter"], env);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        stdout(&out)
    };
    let has = |text: &str, line: &str| text.lines().any(|l| l == line);

    // Default MEDIUM.
    let d = filter(&[]);
    for line in [
        "debug=0", "info=1", "warn=1", "error=1", "fatal=1", "fail=1", "info_high=0",
        "debug_low=1", "error_full=1", "drv_debug=0", "drv_info=1", "mon_debug=0",
        "restored=0", "cleared=-1",
    ] {
        assert!(has(&d, line), "default: missing {line}:\n{d}");
    }
    // --verbosity LOW hides info/warn but never error/fatal.
    let low = filter(&[("HARC_VERBOSITY", "low")]);
    for line in ["info=0", "warn=0", "error=1", "fatal=1", "debug_low=1"] {
        assert!(has(&low, line), "LOW: missing {line}:\n{low}");
    }
    // --verbosity-of raises only the matching id ...
    let of = filter(&[("HARC_VERBOSITY_OF", "AXI_DRV=HIGH")]);
    for line in ["drv_debug=1", "mon_debug=0", "debug=0"] {
        assert!(has(&of, line), "verbosity-of: missing {line}:\n{of}");
    }
    // ... and replaces --verbosity for it in either direction.
    let both = filter(&[("HARC_VERBOSITY", "HIGH"), ("HARC_VERBOSITY_OF", "MON=LOW,AXI_DRV=low")]);
    for line in ["debug=1", "drv_debug=0", "drv_info=0", "mon_debug=0", "info_high=1"] {
        assert!(has(&both, line), "both: missing {line}:\n{both}");
    }
}

#[test]
fn run_suite_runs_every_test_and_prints_one_aggregate_banner() {
    let Some(probe) = build_probe("suite") else {
        return;
    };
    let cwd = probe.dir.join("pass");
    std::fs::create_dir_all(&cwd).unwrap();
    let out = run(&probe, &cwd, &["suite"], &[]);
    let text = stdout(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("[suite] running test A (1/2)"), "{text}");
    assert!(text.contains("[suite] running test B (2/2)"), "{text}");
    assert!(text.contains("2 test(s) run: 2 passed, 0 failed"), "{text}");
    assert_eq!(text.matches("ALL TESTS PASSED").count(), 1, "{text}");

    let cwd = probe.dir.join("fail");
    std::fs::create_dir_all(&cwd).unwrap();
    let out = run(&probe, &cwd, &["suite", "fail"], &[]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("1 passed, 1 failed (B)"), "{text}");
    assert!(text.contains("\n1 TESTS FAILED"), "{text}");
    assert!(!text.contains("ALL TESTS PASSED"), "{text}");
    // sim.log holds both tests, each marked.
    let log = std::fs::read_to_string(cwd.join("sim.log")).expect("sim.log");
    assert!(log.contains("[suite] test A") && log.contains("[suite] test B"), "{log}");
}

#[test]
fn file_first_written_by_a_later_suite_test_starts_empty_each_run() {
    let Some(probe) = build_probe("logf") else {
        return;
    };
    let cwd = probe.dir.join("logf");
    std::fs::create_dir_all(&cwd).unwrap();
    // Two invocations of the same suite; only test B writes b.log.
    for _ in 0..2 {
        let out = run(&probe, &cwd, &["suite"], &[]);
        assert!(out.status.success(), "{}", stdout(&out));
    }
    let b = std::fs::read_to_string(cwd.join("b.log")).expect("b.log");
    assert_eq!(b, "B\n", "b.log appended to the previous invocation's file");
    // sim.log is truncated per invocation too, then shared by both tests.
    let log = std::fs::read_to_string(cwd.join("sim.log")).expect("sim.log");
    assert_eq!(log.matches("[suite] test A").count(), 1, "{log}");
}
