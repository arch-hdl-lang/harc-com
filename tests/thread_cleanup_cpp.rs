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
#include <type_traits>

struct FrameGuard {
    int* live;
    bool* dut_alive;
    int* late;
    FrameGuard(int* count, bool* alive, int* late_count)
        : live(count), dut_alive(alive), late(late_count) { ++*live; }
    ~FrameGuard() {
        if (!*dut_alive) ++*late;
        --*live;
    }
};

harc_rt::HarcThread suspended(
    int* live,
    bool* dut_alive,
    int* late,
    harc_rt::ThreadSlot* slot) {
    FrameGuard guard(live, dut_alive, late);
    co_await harc_rt::wait_until(slot, []() { return false; });
}

int main() {
    static_assert(!std::is_copy_constructible_v<harc_rt::HarcThread>);
    static_assert(!std::is_copy_assignable_v<harc_rt::HarcThread>);
    static_assert(std::is_move_constructible_v<harc_rt::HarcThread>);
    static_assert(std::is_move_assignable_v<harc_rt::HarcThread>);

    int live = 0;
    int late = 0;
    bool dut_alive = true;
    bool fatal = false;
    {
        harc_rt::ThreadScheduler scheduler;
        harc_rt::ThreadSlot run_slot;
        harc_rt::ThreadSlot actor_slot;
        scheduler.slots.push_back(&run_slot);
        scheduler.slots.push_back(&actor_slot);
        run_slot.thread = suspended(&live, &dut_alive, &late, &run_slot);
        actor_slot.thread = suspended(&live, &dut_alive, &late, &actor_slot);
        scheduler.bootstrap();
        std::printf("during=%d ", live);
        fatal = true;
        if (fatal) harc_rt::harc_destroy_scheduler_threads(scheduler);
        std::printf(
            "fatal=%d clean=%d slots=%zu preds=%d ",
            fatal ? 1 : 0,
            live,
            scheduler.slots.size(),
            (run_slot.pred ? 1 : 0) + (actor_slot.pred ? 1 : 0));
        dut_alive = false;
    }
    std::printf("after=%d late=%d\n", live, late);
    return 0;
}
"#;

#[test]
fn fatal_suspended_run_and_actor_frames_are_destroyed_before_dut_teardown() {
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH; install one or set HARC_SKIP_CXX_PROBE=1"
        );
        return;
    };

    let dir = std::env::temp_dir().join(format!("harc_thread_cleanup_{}", std::process::id()));
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
        "thread cleanup probe failed to compile:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = Command::new(binary).output().expect("run probe");
    assert!(run.status.success(), "thread cleanup probe exited non-zero");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout),
        "during=2 fatal=1 clean=0 slots=0 preds=0 after=0 late=0\n"
    );
}
