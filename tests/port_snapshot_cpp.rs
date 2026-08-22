use std::path::PathBuf;
use std::process::Command;

fn cxx() -> Option<String> {
    for cc in [
        std::env::var("CXX").unwrap_or_default().as_str(),
        "c++",
        "clang++",
        "g++",
    ] {
        if !cc.is_empty()
            && Command::new(cc)
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        {
            return Some(cc.to_string());
        }
    }
    None
}

#[test]
fn dpi_port_snapshots_materialize_values() {
    let Some(cxx) = cxx() else {
        eprintln!("skipping DPI snapshot probe: no C++ compiler");
        return;
    };
    let dir = std::env::temp_dir().join("harc-dpi-port-snapshot-probe");
    std::fs::create_dir_all(&dir).expect("scratch directory");
    std::fs::write(
        dir.join("svdpi.h"),
        "using svScope = void*; inline void svSetScope(svScope) {}\n",
    )
    .expect("fake svdpi header");
    std::fs::write(
        dir.join("verilated.h"),
        "struct VerilatedContext {}; struct Verilated { static void threadContextp(VerilatedContext*) {} };\n",
    )
    .expect("fake verilated header");
    let source = dir.join("probe.cpp");
    std::fs::write(
        &source,
        r#"
#include "harc_thread_rt.h"
#include "harc_cosim_rt.h"
#include <cstdio>

static unsigned long long scalar_value;
static unsigned wide_words[4];
static unsigned long long unpacked_values[4];
extern "C" long long harc_sv_get(int) { return (long long)scalar_value; }
extern "C" void harc_sv_set(int, long long v) { scalar_value = v; }
extern "C" long long harc_sv_get_word(int, int word) { return wide_words[word]; }
extern "C" void harc_sv_set_word(int, int word, long long v) { wide_words[word] = v; }
extern "C" long long harc_sv_get_elem(int, int idx) { return unpacked_values[idx]; }
extern "C" void harc_sv_set_elem(int, int idx, long long v) { unpacked_values[idx] = v; }

int main() {
    harc_rt::cosim::SigProxy<0> scalar;
    scalar_value = 3;
    auto scalar_snap = harc_rt::harc_port_snapshot(scalar);
    scalar_value = 9;

    harc_rt::cosim::WideSigProxy<1, 4> wide;
    wide_words[0] = 5;
    auto wide_snap = harc_rt::harc_port_snapshot(wide);
    wide_words[0] = 7;

    harc_rt::cosim::UnpackedSigProxy<2, 4> unpacked;
    unpacked_values[1] = 11;
    auto unpacked_snap = harc_rt::harc_port_snapshot(unpacked);
    unpacked_values[1] = 13;

    std::printf("%llu %u %llu\n",
        (unsigned long long)(uint64_t)scalar_snap,
        (unsigned)(uint32_t)wide_snap[0],
        (unsigned long long)(uint64_t)unpacked_snap[1]);
}
"#,
    )
    .expect("probe source");
    let binary: PathBuf = dir.join("probe");
    let built = Command::new(&cxx)
        .arg("-std=gnu++20")
        .arg("-I")
        .arg(&dir)
        .arg("-I")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime"))
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile DPI snapshot probe");
    assert!(
        built.status.success(),
        "DPI snapshot probe failed to compile:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let ran = Command::new(&binary)
        .output()
        .expect("run DPI snapshot probe");
    assert!(ran.status.success(), "DPI snapshot probe failed");
    assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "3 5 11");
}
