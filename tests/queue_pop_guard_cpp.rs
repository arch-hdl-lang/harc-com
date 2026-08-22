//! Runtime gate for `harc_rt::HarcQueue<T>::pop()` on an empty queue.
//!
//! `pop()` used to call `std::deque::front()` with no emptiness check
//! (#644). `front()` on an empty deque is undefined behaviour, so a
//! testbench that popped an empty scoreboard or component queue read
//! garbage — or died somewhere unrelated to the actual mistake —
//! instead of reporting it. Not every pop site is guarded upstream: the
//! direct-actor thread pop waits on `!queue.empty()` first, but the
//! scoreboard / component-queue seam (`Stmt::ComponentQueuePop`) emits
//! a bare `<recv>.<queue>.pop()`.
//!
//! `tests/fixtures/queue_empty_pop_test.harc` covers the same guard
//! end-to-end through `harc sim`, but that path needs Verilator and only
//! runs under `tests/run_negative_fixtures.sh`. This test compiles the
//! shipped runtime header with a host compiler, so a regression fails in
//! `cargo test` rather than in a job nobody runs locally.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Whether a usable host C++ compiler is on PATH. Mirrors
/// `tests/wide_cast_cpp.rs`.
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

/// `argv[1] == "empty"` pops an empty queue and must never return;
/// anything else exercises the ordinary push/pop path so a guard that
/// fires on a NON-empty queue fails here too.
const PROBE: &str = r#"#include "harc_queue_rt.h"
#include <cstdio>
#include <cstring>

int main(int argc, char** argv) {
    harc_rt::HarcQueue<unsigned long long> q;
    if (argc > 1 && std::strcmp(argv[1], "empty") == 0) {
        (void)q.pop();
        std::printf("UNREACHABLE\n");
        return 0;
    }
    q.push(7);
    q.push(9);
    unsigned long long a = q.pop();
    unsigned long long b = q.pop();
    std::printf("%llu %llu %d %zu\n", a, b, (int)q.empty(), q.size());
    return 0;
}
"#;

#[test]
fn empty_queue_pop_aborts_with_a_harc_error() {
    // A silent skip would let the UB this test exists for ship on any
    // machine without a compiler, so an absent toolchain fails unless
    // the operator opts out explicitly (same contract as
    // `tests/wide_cast_cpp.rs`).
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH (tried $CXX, g++, clang++). This test compiles \
             harc_queue_rt.h and pops an empty queue — without it the empty-pop guard is \
             untested outside the Verilator fixture job. Install a compiler, or set \
             HARC_SKIP_CXX_PROBE=1 to skip deliberately."
        );
        eprintln!("SKIP empty_queue_pop_aborts_with_a_harc_error: HARC_SKIP_CXX_PROBE set.");
        return;
    };

    let dir = std::env::temp_dir().join(format!("harc_queue_pop_guard_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    let probe = dir.join("probe.cpp");
    std::fs::write(&probe, PROBE).expect("write probe");

    let bin = dir.join("probe");
    let build = Command::new(&cxx)
        // Match the real build: `run_verilator` (`src/main.rs`) sets
        // `CFG_CXXFLAGS_STD=-std=gnu++20`.
        .arg("-std=gnu++20")
        .arg("-I")
        .arg(repo_root().join("runtime"))
        .arg(&probe)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("run C++ compiler");
    assert!(
        build.status.success(),
        "queue runtime probe did not compile:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );

    // Ordinary pops keep working — FIFO order, and the queue drains.
    let ok = Command::new(&bin).output().expect("run probe");
    assert!(
        ok.status.success(),
        "non-empty pop path exited non-zero:\n{}",
        String::from_utf8_lossy(&ok.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&ok.stdout).trim(),
        "7 9 1 0",
        "push/pop no longer round-trips in FIFO order",
    );

    // Popping empty aborts with the diagnostic instead of reading a
    // dangling `front()`.
    let boom = Command::new(&bin).arg("empty").output().expect("run probe");
    let stdout = String::from_utf8_lossy(&boom.stdout);
    let stderr = String::from_utf8_lossy(&boom.stderr);
    assert!(
        !stdout.contains("UNREACHABLE"),
        "pop() on an empty queue returned instead of aborting; stdout:\n{stdout}",
    );
    assert!(
        !boom.status.success(),
        "pop() on an empty queue exited 0; stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stderr.contains("HARC-ERROR: pop() on an empty queue"),
        "empty pop did not report the HARC-ERROR diagnostic; stderr:\n{stderr}",
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            boom.status.signal(),
            Some(libc_sigabrt()),
            "empty pop did not abort(); status: {:?}, stderr:\n{stderr}",
            boom.status,
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// SIGABRT is 6 on every platform this repo builds on (Linux, macOS).
#[cfg(unix)]
fn libc_sigabrt() -> i32 {
    6
}
