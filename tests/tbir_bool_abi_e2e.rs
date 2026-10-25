//! Native regression for Boolean helper ABI and full-CFG result storage.
//!
//! A declared `bool` return must use C++'s Boolean conversion at both the
//! pure-helper callable boundary and the result slot used when a helper is
//! inlined into the caller's CFG. Integer carriers keep their existing ABI.

use std::path::{Path, PathBuf};
use std::process::Command;

const TB: &str = r#"
function pure_zero() -> bool
    return 0
end function pure_zero

function pure_one() -> bool
    return 1
end function pure_one

function pure_truthy() -> bool
    return 2
end function pure_truthy

function pure_negative() -> bool
    return -1
end function pure_negative

// The local declaration forces the helper through full-CFG inlining.
function full_zero(d: Top) -> bool
    let ignored = 0
    return 0
end function full_zero

function full_one(d: Top) -> bool
    let ignored = 0
    return 1
end function full_one

function full_truthy(d: Top) -> bool
    let ignored = 0
    return 2
end function full_truthy

function full_negative(d: Top) -> bool
    let ignored = 0
    return -1
end function full_negative

test BoolHelperAbi
    let dut : Top
    run
        assert pure_zero() == false else fail("pure zero")
        assert pure_one() == true else fail("pure one")
        assert pure_truthy() == true else fail("pure truthy")
        assert pure_negative() == true else fail("pure negative")
        assert full_zero(dut) == false else fail("full zero")
        assert full_one(dut) == true else fail("full one")
        assert full_truthy(dut) == true else fail("full truthy")
        assert full_negative(dut) == true else fail("full negative")
        wait 1 cycle
    end run
end test BoolHelperAbi
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_backend(backend: &str, outdir: &Path) -> String {
    let source = outdir.join("bool_helper_abi.harc");
    std::fs::create_dir_all(outdir).expect("create output directory");
    std::fs::write(&source, TB).expect("write HARC source");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim"])
        .arg(&source)
        .args(["--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .args(["--codegen", backend, "--outdir"])
        .arg(outdir)
        .args(["--jobs", "2"])
        .output()
        .expect("run harc sim");
    assert!(
        output.status.success(),
        "{backend} Boolean helper simulation failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(outdir.join("bool_helper_abi.cpp")).expect("read generated C++")
}

#[test]
fn bool_helper_abi_matches_v1_for_pure_and_full_cfg_paths() {
    let root = std::env::temp_dir().join(format!("harc_bool_helper_abi_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let tbir_dir = root.join("tbir");
    let v1_dir = root.join("v1");
    let tbir = run_backend("tbir", &tbir_dir);
    let _v1 = run_backend("v1", &v1_dir);

    assert!(
        tbir.contains("static bool harc_helper_pure_truthy()")
            && tbir.contains("bool __ret"),
        "Boolean pure-helper ABI/result storage must be real C++ bools:\n{tbir}"
    );
    assert!(
        !tbir.contains("static uint64_t harc_helper_pure_truthy()"),
        "Boolean callable ABI must not fall back to uint64_t"
    );
    assert!(
        tbir.contains("bool __t") || tbir.contains("bool __ret"),
        "full-CFG Boolean result storage must be real C++ bools:\n{tbir}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
