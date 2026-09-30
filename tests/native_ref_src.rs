//! Native reference linkage regression. Set ARCH_BIN to run the compiler pair.
use std::{fs, process::Command};

#[test]
fn native_arch_links_multiple_reference_sources() {
    let Ok(arch) = std::env::var("ARCH_BIN") else {
        eprintln!("set ARCH_BIN to exercise native reference linkage");
        return;
    };
    let dir = std::env::temp_dir().join(format!("harc_native_refs_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let tb = dir.join("ref_test.harc");
    fs::write(
        &tb,
        r#"
extern function native_one() -> uint<32>
extern function native_two(x: uint<32>) -> uint<32>
test NativeReferences
    let dut : Top
    run
        assert native_one() == 17 else fail("first reference")
        assert native_two(17) == 42 else fail("second reference")
    end run
end test NativeReferences
"#,
    )
    .unwrap();
    let one = dir.join("one.cpp");
    let two = dir.join("two.cpp");
    fs::write(
        &one,
        "#include <cstdint>\nextern \"C\" uint64_t native_one(){return 17;}\n",
    )
    .unwrap();
    fs::write(
        &two,
        "#include <cstdint>\nextern \"C\" uint64_t native_two(uint64_t x){return x+25;}\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--dut"])
        .arg(root.join("tests/dut/top_counter.arch"))
        .args(["--top", "Top", "--arch-bin"])
        .arg(arch)
        .arg("--outdir")
        .arg(dir.join("build"))
        .arg("--ref-src")
        .arg("one.cpp")
        .arg("--ref-src")
        .arg("two.cpp")
        .arg(&tb)
        .current_dir(&dir)
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
