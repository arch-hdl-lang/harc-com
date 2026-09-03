//! Compile-and-run gate for generated wide-record pack/unpack helpers.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{lower, verify};
use harc::parser::parse_source;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wide_record_test.harc"),
    )
    .expect("read wide-record fixture")
}

fn emitted_records(codegen: &str) -> String {
    let parsed = parse_source(&fixture()).expect("fixture parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = if codegen == "v1" {
        cpp_tb::emit(&merged).expect("v1 emits")
    } else {
        let prog = lower::lower_program(&merged).expect("TB-IR lowers");
        verify::verify_program(&prog).expect("TB-IR verifies");
        tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()).expect("TB-IR emits")
    };
    let start = cpp.find("struct WideLeaf {").expect("record declarations");
    let end = cpp[start..]
        .find("struct HarcTestContext {")
        .map(|n| start + n)
        .expect("end of record helpers");
    cpp[start..end].to_string()
}

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
                .is_ok_and(|o| o.status.success())
        {
            return Some(cc.to_string());
        }
    }
    None
}

fn outdir(codegen: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "harc_wide_record_cpp_{codegen}_{}",
        std::process::id()
    ))
}

#[test]
fn both_backends_preserve_wide_record_bits_in_flat_and_structured_conversion() {
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH; set HARC_SKIP_CXX_PROBE=1 to skip deliberately"
        );
        return;
    };
    if std::env::var_os("HARC_SKIP_CXX_PROBE").is_some() {
        return;
    }

    for codegen in ["v1", "tbir"] {
        let dir = outdir(codegen);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create probe dir");
        let cpp = dir.join("probe.cpp");
        let bin = dir.join("probe");
        let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/harc_thread_rt.h");
        let random_runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/harc_random_rt.h");
        std::fs::write(
            &cpp,
            format!(
                r#"#include <array>
#include <cassert>
#include <cstdint>
#include "{}"
#include "{}"

static harc_rt::random::HarcRng harc_rng;
static inline uint64_t harc_rng_next() {{ return harc_rng.next(); }}

{}

struct RawLeaf {{
    uint32_t value[8]{{}};
    uint32_t odd[5]{{}};
    uint32_t sign[3]{{}};
    uint32_t long_sign[5]{{}};
    uint64_t tag{{}};
}};
struct RawEnvelope {{
    uint8_t head{{}};
    RawLeaf inner{{}};
    uint32_t lanes[2][5]{{}};
    uint64_t matrix[3][2]{{}};
}};

int main() {{
    WideLeaf leaf{{}};
    leaf.value.words[0] = 0x89abcdefu;
    leaf.value.words[7] = 0x80000000u;
    leaf.odd.words[0] = 0x13579bdfu;
    leaf.odd.words[4] = 2u;
    leaf.sign = (_harc_u128{{1}} << 64) | 7u;
    leaf.long_sign.words[0] = 7u;
    leaf.long_sign.words[4] = 2u;
    leaf.tag = 5;

    auto packed_leaf = harc_pack_WideLeaf(leaf);
    assert(harc_unpack_WideLeaf(packed_leaf) == leaf);
    RawLeaf raw_leaf{{}};
    harc_drive_WideLeaf(raw_leaf, leaf);
    assert(harc_unpack_WideLeaf(raw_leaf) == leaf);
    raw_leaf.odd[4] |= 0xfffffffcu;
    raw_leaf.sign[2] |= 0xfffffffeu;
    raw_leaf.long_sign[4] |= 0xfffffffcu;
    raw_leaf.tag |= 0xfffffff8u;
    assert(harc_unpack_WideLeaf(raw_leaf) == leaf);

    WideEnvelope envelope{{}};
    envelope.head = 0xa5;
    envelope.inner = leaf;
    envelope.lanes[0].words[4] = 1u;
    envelope.lanes[1].words[0] = 7u;
    envelope.matrix[0][0] = 0x12u;
    envelope.matrix[1][1] = 0x34u;
    envelope.matrix[2][0] = 0x56u;
    auto packed_envelope = harc_pack_WideEnvelope(envelope);
    assert(harc_unpack_WideEnvelope(packed_envelope) == envelope);
    RawEnvelope raw_envelope{{}};
    harc_drive_WideEnvelope(raw_envelope, envelope);
    assert(harc_unpack_WideEnvelope(raw_envelope) == envelope);
    return 0;
}}
"#,
                runtime.display(),
                random_runtime.display(),
                emitted_records(codegen)
            ),
        )
        .expect("write C++ probe");
        let compile = Command::new(&cxx)
            .arg("-std=gnu++20")
            .arg(&cpp)
            .arg("-o")
            .arg(&bin)
            .output()
            .expect("run C++ compiler");
        assert!(
            compile.status.success(),
            "[{codegen}] compile failed:\n{}",
            String::from_utf8_lossy(&compile.stderr)
        );
        let run = Command::new(&bin).output().expect("run C++ probe");
        assert!(
            run.status.success(),
            "[{codegen}] probe failed:\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
