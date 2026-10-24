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
    // This immediate false wait follows a real timeout and must not read its
    // stale slot flag. The immediate true wait must still succeed.
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, []() { return false; }, 0) ? 1 : 0);
    results->push_back(co_await harc_rt::wait_until_timeout(
        slot, []() { return true; }, 0) ? 1 : 0);
    // A suspended predicate that fires clears the slot's timeout state; the
    // following immediate false wait must remain false after that success.
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
    scheduler.tick(); // timeout the first one-cycle false wait
    scheduler.tick(); // timeout the gate wait while gate is false
    gate = true;
    scheduler.tick(); // satisfy the gate wait

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
