use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

fn source_file(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harc_check_cli_{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create check-cli temp dir");
    let path = dir.join(name);
    fs::write(&path, source).expect("write check-cli source");
    path
}

#[test]
fn check_accepts_wide_width_methods_through_1024_bits() {
    let path = source_file(
        "wide_ok.harc",
        r#"function wide_ok(a: uint<64>) -> uint<64>
    let v256 : uint<256> = a.zext<256>()
    let v1024 : uint<1024> = v256.zext<1024>()
    return v1024.trunc<64>()
end function wide_ok
"#,
    );
    let output = Command::new(harc_bin())
        .args(["check", path.to_str().unwrap()])
        .output()
        .expect("run harc check");
    assert!(
        output.status.success(),
        "harc check should accept widths through 1024\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn check_rejects_width_method_above_1024_bits() {
    let path = source_file(
        "wide_bad.harc",
        r#"function wide_bad(a: uint<64>) -> uint<64>
    let value = a.zext<1025>()
    return value.trunc<64>()
end function wide_bad
"#,
    );
    let output = Command::new(harc_bin())
        .args(["check", path.to_str().unwrap()])
        .output()
        .expect("run harc check");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "harc check should reject 1025 bits"
    );
    assert!(
        stderr.contains(".zext<1025>()") && stderr.contains("1..=1024"),
        "expected source-located language-limit diagnostic:\n{stderr}"
    );
}

#[test]
fn check_rejects_zero_and_nonconstant_width_methods() {
    for (name, width, expected) in [
        ("zero.harc", "0", "1..=1024"),
        ("nonconstant.harc", "WIDTH", "literal width"),
    ] {
        let source = format!(
            "function bad(a: uint<64>) -> uint<64>\n    let value = a.zext<{width}>()\n    return value.trunc<64>()\nend function bad\n"
        );
        let path = source_file(name, &source);
        let output = Command::new(harc_bin())
            .args(["check", path.to_str().unwrap()])
            .output()
            .expect("run harc check");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "harc check should reject {width}");
        assert!(
            stderr.contains(expected),
            "expected `{expected}` in diagnostic for {width}:\n{stderr}"
        );
    }
}
