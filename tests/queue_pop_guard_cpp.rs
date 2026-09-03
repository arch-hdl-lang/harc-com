//! Runtime gate for guarded `harc_rt::HarcQueue<T>` value reads.
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
//! Both `pop()` and the non-destructive `front()` have two paths, and this
//! test pins them because each
//! covers what the other cannot:
//!
//!   * With a reporter installed — what the generated `run_<Test>()`
//!     prologue does — the empty pop reports through the sim's FATAL
//!     path and hands back a value-initialised `T`. Nothing aborts, so
//!     the run can unwind and still write its log and trace.
//!   * With no reporter — a unit test, or any caller outside a
//!     generated run function — there is no test context to fail
//!     cleanly through, so it aborts rather than silently returning a
//!     default that a caller would mistake for real data.
//!
//! `tests/fixtures/queue_empty_pop_test.harc` covers the reporter path
//! end-to-end through `harc sim` under both codegens, but that needs
//! Verilator and only runs under `tests/run_negative_fixtures.sh` and
//! the equivalence harness. This test compiles the shipped runtime
//! header with a host compiler, so a regression fails in `cargo test`
//! rather than in a job nobody runs locally.

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

/// `argv[1]` selects the path under test:
///   * `report` / `front-report` — reporter installed: the empty read must
///     name its operation, return a zero-filled `T`, and NOT abort.
///   * `abort` / `front-abort` — no reporter: the empty read must abort.
///   * anything else — the ordinary push/pop path, so a guard that
///     fired on a NON-empty queue fails here too.
const PROBE: &str = r#"#include "harc_queue_rt.h"
#include <cstdio>
#include <cstring>

int main(int argc, char** argv) {
    const char* mode = argc > 1 ? argv[1] : "";
    harc_rt::HarcQueue<unsigned long long> q;

    if (std::strcmp(mode, "report") == 0) {
        int reported = 0;
        harc_rt::HarcQueueFatalScope scope([&]() { reported++; });
        unsigned long long v = q.pop();
        // Reported exactly once, poisoned with a deterministic zero,
        // and the queue is still coherent afterwards.
        std::printf("reported=%d v=%llu size=%zu\n", reported, v, q.size());
        // The scope must uninstall on the way out, or a later read would
        // call into a dead test context.
        return 0;
    }

    if (std::strcmp(mode, "front-report") == 0) {
        int reported = 0;
        harc_rt::HarcQueueFatalScope scope([&]() { reported++; });
        unsigned long long v = q.front();
        std::printf("reported=%d v=%llu size=%zu\n", reported, v, q.size());
        // The scope must uninstall on the way out, or a later pop would
        // call into a dead test context.
        return 0;
    }

    if (std::strcmp(mode, "uninstalled") == 0) {
        {
            harc_rt::HarcQueueFatalScope scope([&]() {});
        }
        std::printf("installed=%d\n", harc_rt::harc_queue_empty_pop_reporter ? 1 : 0);
        return 0;
    }

    if (std::strcmp(mode, "nested") == 0) {
        int outer = 0;
        int inner = 0;
        harc_rt::HarcQueueFatalScope outer_scope([&]() { outer++; });
        {
            harc_rt::HarcQueueFatalScope inner_scope([&]() { inner++; });
            (void)q.pop();
        }
        (void)q.pop();
        std::printf("outer=%d inner=%d\n", outer, inner);
        return 0;
    }

    if (std::strcmp(mode, "abort") == 0) {
        (void)q.pop();
        std::printf("UNREACHABLE\n");
        return 0;
    }

    if (std::strcmp(mode, "front-abort") == 0) {
        (void)q.front();
        std::printf("UNREACHABLE\n");
        return 0;
    }

    q.push(7);
    q.push(9);
    unsigned long long front = q.front();
    unsigned long long a = q.pop();
    unsigned long long b = q.pop();
    std::printf("%llu %llu %llu %d %zu\n", front, a, b, (int)q.empty(), q.size());
    return 0;
}
"#;

#[test]
fn empty_queue_value_reads_report_fatal_and_abort_only_without_a_reporter() {
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
        eprintln!(
            "SKIP empty_queue_pop_reports_fatal_and_aborts_only_without_a_reporter: \
             HARC_SKIP_CXX_PROBE set."
        );
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

    let run = |mode: &str| {
        let out = Command::new(&bin).arg(mode).output().expect("run probe");
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        (out.status, stdout, stderr)
    };

    // Ordinary pops keep working — FIFO order, and the queue drains.
    let (status, stdout, stderr) = run("ok");
    assert!(
        status.success(),
        "non-empty pop path exited non-zero:\n{stderr}",
    );
    assert_eq!(
        stdout, "7 7 9 1 0",
        "front must not remove the value and push/pop must retain FIFO order",
    );

    // Reporter installed — the generated `run_<Test>()` shape. The
    // failure goes to the sim's FATAL path and the pop returns a
    // deterministic zero, so the run can unwind and write its trace.
    let (status, stdout, stderr) = run("report");
    assert!(
        status.success(),
        "empty pop aborted even though a reporter was installed; this is the path the \
         generated prologue takes, so aborting here means no log and no trace:\n{stderr}",
    );
    assert_eq!(
        stdout, "reported=1 v=0 size=0",
        "empty pop with a reporter must report exactly once and return a zero-filled \
         value; stderr:\n{stderr}",
    );

    let (status, stdout, stderr) = run("front-report");
    assert!(
        status.success(),
        "empty front aborted even though a reporter was installed:\n{stderr}",
    );
    assert_eq!(
        stdout, "reported=1 v=0 size=0",
        "empty front must report exactly once, preserve the queue, and return a deterministic zero; stderr:\n{stderr}",
    );

    // The scope uninstalls on the way out — otherwise a later pop would
    // call into a test context that no longer exists.
    let (status, stdout, _) = run("uninstalled");
    assert!(status.success(), "uninstall probe exited non-zero");
    assert_eq!(
        stdout, "installed=0",
        "HarcQueueFatalScope left its reporter installed after destruction",
    );

    // A generated run can be nested under a caller-owned reporting scope.
    // Leaving the inner scope must restore, rather than erase, that caller.
    let (status, stdout, stderr) = run("nested");
    assert!(status.success(), "nested reporter probe failed:\n{stderr}");
    assert_eq!(
        stdout, "outer=1 inner=1",
        "the inner queue-fatal scope did not restore the previous reporter",
    );

    // No reporter — no test context to fail cleanly through, so this
    // aborts rather than silently handing back a default.
    let (status, stdout, stderr) = run("abort");
    assert!(
        !stdout.contains("UNREACHABLE"),
        "pop() on an empty queue returned instead of aborting with no reporter installed; \
         stdout:\n{stdout}",
    );
    assert!(
        !status.success(),
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
            status.signal(),
            Some(SIGABRT),
            "empty pop did not abort(); status: {status:?}, stderr:\n{stderr}",
        );
    }

    let (status, stdout, stderr) = run("front-abort");
    assert!(
        !stdout.contains("UNREACHABLE"),
        "front() on an empty queue returned instead of aborting with no reporter installed; stdout:\n{stdout}",
    );
    assert!(!status.success(), "empty front exited 0; stderr:\n{stderr}");
    assert!(
        stderr.contains("HARC-ERROR: front() on an empty queue"),
        "empty front did not name the operation; stderr:\n{stderr}",
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(SIGABRT),
            "empty front did not abort(); status: {status:?}, stderr:\n{stderr}",
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// SIGABRT is 6 on every platform this repo builds on (Linux, macOS).
#[cfg(unix)]
const SIGABRT: i32 = 6;
