//! Direct scheduler regression for issue #936.

use std::{fs, process::Command};

#[test]
fn scheduler_slot_growth_is_memory_safe() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = std::env::temp_dir().join(format!("harc_scheduler_growth_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("scheduler_growth");
    let result = Command::new("c++")
        .args([
            "-std=c++20",
            "-g",
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-fno-omit-frame-pointer",
        ])
        .arg("-I")
        .arg(root.join("runtime"))
        .arg(root.join("tests/scheduler_growth.cpp"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("C++ compiler required");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = Command::new(&binary).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::remove_dir_all(dir).unwrap();
}
