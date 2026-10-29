//! Native regression: `.resize<N>()` keeps its receiver's signedness.
//!
//! A signed source sign-extends when widening and the result is
//! `sint<N>` at every carrier class (≤64, 65..128, >128); equal width is
//! the identity and narrowing truncates to a `sint<N>`. Unsigned sources
//! keep zero-extension / truncation. Before the fix a signed `-1` widened
//! by a raw unsigned cast to all-ones in a 64/128-bit carrier, typed
//! unsigned, while >128-bit destinations masked to the 8 source bits.
//!
//! Sub-64 destinations are asserted only for parity with `.sext<N>()`:
//! both share the existing sub-64 width-method carrier.

use std::path::PathBuf;
use std::process::Command;

const TB: &str = r#"
testbench Tb
    dut : Top
end testbench Tb
impl SignedResize for Tb
    run
        let neg : sint<8> = -1
        let pos : sint<8> = 100
        let ubits : uint<8> = 255

        // Widening a signed source sign-extends; the result is signed.
        let n64 = neg.resize<64>()
        let n100 = neg.resize<100>()
        let n128 = neg.resize<128>()
        let n129 = neg.resize<129>()
        let n256 = neg.resize<256>()
        assert n64 < 0 else fail("n64 < 0")
        assert n64 == -1 else fail("n64 == -1")
        assert n100 < 0 else fail("n100 < 0")
        assert n100 == -1 else fail("n100 == -1")
        assert n128 < 0 else fail("n128 < 0")
        assert n128 == -1 else fail("n128 == -1")
        assert n129 < 0 else fail("n129 < 0")
        assert n129 == -1 else fail("n129 == -1")
        assert n256 < 0 else fail("n256 < 0")
        assert n256 == -1 else fail("n256 == -1")
        assert n128.trunc<16>() == n129.trunc<16>() else fail("128/129 carrier boundary")
        assert neg.resize<16>() == neg.sext<16>() else fail("resize<16> != sext<16>")
        assert neg.resize<16>()[15:8] == 0xFF else fail("resize<16> sign fill")

        let p16 = pos.resize<16>()
        let p64 = pos.resize<64>()
        let p100 = pos.resize<100>()
        let p129 = pos.resize<129>()
        let p256 = pos.resize<256>()
        assert p16 == 100 else fail("p16")
        assert p64 == 100 else fail("p64")
        assert p100 == 100 else fail("p100")
        assert p129 == 100 else fail("p129")
        assert p256 == 100 else fail("p256")
        assert p256 > 0 else fail("p256 > 0")

        // Equal width is the identity.
        let same = neg.resize<8>()
        assert same == -1 else fail("same-width identity")

        // Narrowing truncates and keeps the signed type.
        let big_neg : sint<256> = -2
        let nn129 = big_neg.resize<129>()
        let nn100 = big_neg.resize<100>()
        let nn64 = big_neg.resize<64>()
        assert nn129 == -2 else fail("narrow 256->129")
        assert nn100 == -2 else fail("narrow 256->100")
        assert nn100 < 0 else fail("narrow 256->100 < 0")
        assert nn64 == -2 else fail("narrow 256->64")
        let s16 : sint<16> = 0x0180
        assert s16.resize<8>() == s16.trunc<8>() else fail("narrow 16->8 bits")

        // Unsigned sources are unchanged: zero-extend / truncate.
        let u16 = ubits.resize<16>()
        let u100 = ubits.resize<100>()
        let u256 = ubits.resize<256>()
        assert u16 == 255 else fail("u16")
        assert u100 == 255 else fail("u100")
        assert u256 == 255 else fail("u256")
        log(info, "SIGNED_RESIZE_OK")
    end run
end impl SignedResize
"#;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn native_resize_sign_extends_signed_sources() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_resize_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("signed_resize.harc");
    std::fs::write(&source, TB).expect("write signed-resize fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--outdir"])
        .arg(&outdir)
        .args(["--jobs", "2"])
        .output()
        .expect("run native signed-resize fixture");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("ALL TESTS PASSED"),
        "native signed-resize fixture failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("SIGNED_RESIZE_OK"),
        "missing marker:\n{stdout}"
    );
}

/// A signed resize result is `sint<N>`, so — exactly like `.sext<N>()` —
/// it cannot be bound to an unsigned declaration without a cast.
#[test]
fn signed_resize_result_is_signed() {
    let outdir = std::env::temp_dir().join(format!(
        "harc_native_signed_resize_type_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create output directory");
    let source = outdir.join("signed_resize_type.harc");
    std::fs::write(
        &source,
        r#"
testbench Tb
    dut : Top
end testbench Tb
impl SignedResizeType for Tb
    run
        let neg : sint<8> = -1
        let u : uint<16> = neg.resize<16>()
        log(info, "${u}")
    end run
end impl SignedResizeType
"#,
    )
    .expect("write fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .args(["sim", "--emit-only", "--sv"])
        .arg(repo_root().join("tests/dut/top_counter.sv"))
        .arg(&source)
        .args(["--top", "Top", "--outdir"])
        .arg(&outdir)
        .output()
        .expect("run harc");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains("signed 16-bit value"),
        "signed resize result must be typed sint<16>:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
}
