use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

#[test]
fn after_body_has_lexical_scope_and_restores_outer_bindings() {
    let dir = std::env::temp_dir().join(format!("harc_after_scope_{}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temporary fixture directory");
    let path = dir.join("after_scope.harc");
    fs::write(
        &path,
        r#"test AfterScope
    let dut : Top
    run
        let x : uint<64> = 1
        after 1 cycles
            let x : uint<64> = 2
            assert x == 2 else fail("inner")
        end after
        assert x == 1 else fail("outer x=${x}")
        after 1 cycles
            x = 3
            after 1 cycles
                let x : uint<64> = 4
                assert x == 4 else fail("nested inner")
            end after
            assert x == 3 else fail("nested outer x=${x}")
        end after
        assert x == 3 else fail("updated outer x=${x}")
    end run
end test AfterScope
"#,
    )
    .expect("write after scope fixture");

    let output = Command::new(harc_bin())
        .args([
            "dump-ir",
            path.to_str().expect("fixture path is valid UTF-8"),
        ])
        .output()
        .expect("run dump-ir");
    assert!(
        output.status.success(),
        "dump-ir failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dump = String::from_utf8_lossy(&output.stdout);
    assert!(
        dump.contains("Assign(%x, 1)"),
        "outer declaration missing:\n{dump}"
    );
    assert!(
        dump.contains("Assign(%x_2, 2)"),
        "inner declaration missing:\n{dump}"
    );
    assert!(
        dump.contains("AssertCheck { cond: (%x_2 == 2)"),
        "inner assertion lost its local binding:\n{dump}"
    );
    assert!(
        dump.contains("AssertCheck { cond: (%x == 1)"),
        "outer binding was not restored:\n{dump}"
    );
    assert!(
        dump.contains("Assign(%x, 3)"),
        "assignment should update the existing outer local:\n{dump}"
    );
    assert!(
        dump.contains("Assign(%x_3, 4)"),
        "nested declaration missing:\n{dump}"
    );
    assert!(
        dump.contains("AssertCheck { cond: (%x_3 == 4)"),
        "nested inner binding missing:\n{dump}"
    );
    assert!(
        dump.contains("AssertCheck { cond: (%x == 3)"),
        "nested outer binding was not restored:\n{dump}"
    );
}
