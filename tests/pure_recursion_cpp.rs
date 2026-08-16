//! C++-level gate for recursive PURE helpers.
//!
//! TB-IR emits a pure helper as a file-scope C++ function with its
//! prototype ahead of every body, which is what makes a self-call — or a
//! mutually recursive pair — resolve. (v1 cannot express this: it emits
//! every helper as an `auto` lambda, and a lambda that names itself
//! inside its own initializer does not compile, which is why the
//! recursion check runs after the purity fixpoint rather than before it.)
//!
//! `tests/tbir.rs` checks that the prototype and the self-call are
//! emitted; that is a string match, and a string match cannot tell a
//! function that compiles and returns 120 from one that does not compile
//! at all. This test extracts the emitted functions and builds them.

use std::path::{Path, PathBuf};
use std::process::Command;

const TB: &str = r#"function fact(n: uint<8>) -> uint<32>
    if n <= 1
        return 1
    end if
    return n * fact(n - 1)
end function fact

function ping(x: uint<8>) -> uint<8>
    if x == 0
        return 0
    end if
    return pong(x - 1)
end function ping

function pong(x: uint<8>) -> uint<8>
    return ping(x) + 1
end function pong

testbench RecTb
    dut : Top
end testbench RecTb

impl RecTest for RecTb
    run
        let a = fact(5)
        let b = ping(3)
        assert a == 120 else fail("fact")
        assert b == 3 else fail("ping")
        wait 1 cycle
    end run
end impl RecTest
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

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

/// Everything from the first helper prototype up to (but not including)
/// the first covergroup / `_tb` struct — i.e. the helper prototypes and
/// bodies, which are self-contained C++ with no runtime dependency.
fn helper_section(cpp: &str) -> String {
    let start = cpp
        .find("static uint64_t harc_helper_")
        .expect("a helper prototype");
    let end = cpp[start..]
        .find("\nstruct ")
        .map(|i| start + i)
        .unwrap_or(cpp.len());
    cpp[start..end].to_string()
}

fn emit(dir: &Path) -> String {
    let tb = dir.join("rec_tb.harc");
    std::fs::write(&tb, TB).expect("write TB");
    let out = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("sim")
        .arg(&tb)
        .arg("--sv")
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg("--codegen")
        .arg("tbir")
        .arg("--emit-only")
        .arg("--outdir")
        .arg(dir)
        .output()
        .expect("run harc sim --emit-only");
    assert!(
        out.status.success(),
        "emit failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    std::fs::read_to_string(dir.join("rec_tb.cpp")).expect("read emitted cpp")
}

#[test]
fn emitted_recursive_pure_helpers_compile_and_evaluate() {
    let Some(cxx) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH (tried $CXX, g++, clang++). This test builds the \
             emitted recursive helper functions — without it, only the presence of a \
             prototype is checked. Install a compiler, or set HARC_SKIP_CXX_PROBE=1."
        );
        eprintln!("SKIP emitted_recursive_pure_helpers_compile_and_evaluate.");
        return;
    };

    let dir = std::env::temp_dir().join(format!("harc_pure_recursion_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp outdir");
    let cpp = emit(&dir);
    let helpers = helper_section(&cpp);
    assert!(
        helpers.contains("harc_helper_fact((n - 1))") && helpers.contains("harc_helper_pong("),
        "both recursive shapes must be in the extracted section:\n{helpers}"
    );

    let probe = dir.join("probe.cpp");
    std::fs::write(
        &probe,
        format!(
            r#"#include <cstdint>
#include <cstdio>
{helpers}
int main() {{
    printf("%llu %llu\n",
           (unsigned long long)harc_helper_fact(5),
           (unsigned long long)harc_helper_ping(3));
    return 0;
}}
"#
        ),
    )
    .expect("write probe");

    let bin = dir.join("probe");
    let compile = Command::new(&cxx)
        .arg("-std=c++20")
        .arg(&probe)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("run the C++ compiler");
    assert!(
        compile.status.success(),
        "the emitted recursive helpers did not compile:\n{}\n--- probe ---\n{}",
        String::from_utf8_lossy(&compile.stderr),
        std::fs::read_to_string(&probe).unwrap_or_default(),
    );
    let run = Command::new(&bin).output().expect("run the probe");
    assert!(run.status.success(), "probe crashed");
    let got = String::from_utf8_lossy(&run.stdout).trim().to_string();
    assert_eq!(
        got, "120 3",
        "expected fact(5)=120 and ping(3)=3 from the emitted functions; got `{got}`"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
