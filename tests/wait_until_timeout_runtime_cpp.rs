use std::path::PathBuf;
use std::process::Command;

fn cxx() -> Option<String> {
    for cc in [
        std::env::var("CXX").unwrap_or_default().as_str(),
        "g++",
        "clang++",
    ] {
        if !cc.is_empty()
            && Command::new(cc)
                .arg("--version")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        {
            return Some(cc.to_string());
        }
    }
    None
}

const PROBE: &str = r#"#include "harc_thread_rt.h"
#include <cstdio>
#include <vector>

// Every `wait until` takes at least one cycle (spec §7.9): a timed wait
// never completes at entry, even with a true predicate, and a runtime
// budget of zero is clamped to one cycle. Each wait reports its own result,
// never a stale `timed_out` flag left on the slot by an earlier wait.
harc_rt::HarcThread exercise(
    harc_rt::ThreadSlot* slot, bool* gate, std::vector<int>* results) {
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, []() { return false; }, 0) ? 1 : 0);
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, []() { return true; }, 0) ? 1 : 0);
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, []() { return false; }, 1) ? 1 : 0);
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, [gate]() { return *gate; }, 1) ? 1 : 0);
    // A false wait after a real timeout, then a true one: the true wait
    // must succeed despite the previous wait's timeout.
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, []() { return false; }, 0) ? 1 : 0);
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, []() { return true; }, 0) ? 1 : 0);
    // A predicate that fires clears the slot's timeout state; the following
    // false wait must still time out after that success.
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, [gate]() { return *gate; }, 1) ? 1 : 0);
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, []() { return false; }, 0) ? 1 : 0);
}

int main() {
    bool gate = false;
    std::vector<int> results;
    harc_rt::ThreadScheduler scheduler;
    harc_rt::ThreadSlot slot;
    scheduler.slots.push_back(&slot);
    slot.thread = exercise(&slot, &gate, &results);

    scheduler.bootstrap();
    if (!results.empty()) {
        std::fprintf(stderr, "a timed wait completed without taking a cycle\n");
        return 1;
    }
    // Each tick resolves exactly one wait: none completes at entry, and a
    // wait suspended during a tick is first observed on the next tick.
    for (int i = 0; i < 8; ++i) {
        if (i == 6) gate = true;  // the second gate wait fires
        scheduler.tick();
        if (results.size() != static_cast<size_t>(i + 1)) {
            std::fprintf(stderr, "tick %d resolved %zu waits\n", i, results.size());
            return 1;
        }
    }

    const std::vector<int> expected{0, 1, 0, 0, 0, 1, 1, 0};
    if (results != expected || !scheduler.all_done()) {
        std::fprintf(stderr, "unexpected runtime result or unfinished slot\n");
        return 1;
    }
    return 0;
}
"#;

#[test]
fn zero_budget_wait_uses_awaiter_result_not_stale_slot_timeout() {
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH; install one or set HARC_SKIP_CXX_PROBE=1"
        );
        return;
    };

    let dir = std::env::temp_dir().join(format!(
        "harc_wait_until_timeout_runtime_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create probe directory");
    let source = dir.join("probe.cpp");
    std::fs::write(&source, PROBE).expect("write probe");
    let binary = dir.join("probe");
    let build = Command::new(cxx)
        .arg("-std=gnu++20")
        .arg("-I")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime"))
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile probe");
    assert!(
        build.status.success(),
        "wait-until-timeout probe failed to compile:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = Command::new(binary).output().expect("run probe");
    assert!(
        run.status.success(),
        "wait-until-timeout probe exited non-zero:\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
}
