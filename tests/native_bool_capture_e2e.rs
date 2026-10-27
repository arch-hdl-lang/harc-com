//! Native regression coverage for captured Boolean randomize constraints.
//!
//! These cases intentionally run through the default TB-IR/native simulator
//! and Verilator. A bare captured Boolean must become a Z3 predicate; a false
//! capture must report UNSAT rather than reaching Z3's host-side sort assert.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DUT: &str = "module Top(input logic clk); endmodule\n";

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

fn run_case(root: &Path, name: &str, body: &str) -> Output {
    let case = root.join(name);
    std::fs::create_dir_all(&case).expect("create case directory");
    let source = case.join(format!("{name}.harc"));
    let dut = case.join("Top.sv");
    std::fs::write(&source, body).expect("write HARC source");
    std::fs::write(&dut, DUT).expect("write SV DUT");
    Command::new(harc_bin())
        .arg("sim")
        .arg("--sv")
        .arg(&dut)
        .arg(&source)
        .args(["--top", "Top", "--outdir"])
        .arg(case.join("out"))
        .args(["--jobs", "2"])
        .env("HARC_REQUIRE_VERILATOR", "1")
        .env("HARC_NO_LEARN", "1")
        .env("ARCH_NO_LEARN", "1")
        .output()
        .expect("spawn harc sim")
}

fn output_text(output: &Output) -> String {
    format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn source(capture: bool, condition: &str, checks: &str) -> String {
    format!(
        r#"transaction Capture
    flag : bool
end transaction Capture
transaction Req
    flag : bool
    x : uint<8>
end transaction Req
test CaptureBoolean
    let dut : Top
    run
        let c : Capture
        c.flag = {capture}
        let p : Req
        blocking randomize(p) with
            {condition}
            p.x == 3
        end randomize
        {checks}
    end run
end test CaptureBoolean
"#,
        capture = if capture { "true" } else { "false" },
        condition = condition,
        checks = checks
    )
}

#[test]
fn native_captured_boolean_constraints_are_versioned_and_runtime_checked() {
    let root =
        std::env::temp_dir().join(format!("harc_native_bool_capture_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create native Boolean capture root");

    let positive_cases = [
        (
            "true_bare",
            source(true, "c.flag", "assert p.x == 3 else fail(\"payload\")\n        assert c.flag else fail(\"capture\")"),
        ),
        (
            "explicit_true",
            source(true, "c.flag == true", "assert p.x == 3 else fail(\"payload\")"),
        ),
        (
            "explicit_false",
            source(
                false,
                "c.flag == false",
                "assert p.x == 3 else fail(\"payload\")",
            ),
        ),
        (
            "target_bare",
            source(true, "p.flag", "assert p.flag else fail(\"target flag\")"),
        ),
        (
            "nested_logical",
            source(
                true,
                "(!c.flag || (c.flag && p.x == 3))",
                "assert p.x == 3 else fail(\"nested capture\")",
            ),
        ),
    ];
    for (name, source) in positive_cases {
        let output = run_case(&root, name, &source);
        let text = output_text(&output);
        assert!(output.status.success(), "{name} failed:\n{text}");
        assert!(
            text.contains("ALL TESTS PASSED"),
            "{name} lacked pass marker:\n{text}"
        );
    }

    let false_case = source(
        false,
        "c.flag",
        "assert false else fail(\"unreachable after UNSAT\")",
    );
    let output = run_case(&root, "false_bare_unsat", &false_case);
    let text = output_text(&output);
    assert!(
        !output.status.success(),
        "false capture unexpectedly passed:\n{text}"
    );
    assert!(
        text.contains("constraint UNSAT"),
        "missing UNSAT report:\n{text}"
    );
    assert!(
        !text.contains("is_bool()"),
        "Z3 sort assertion leaked:\n{text}"
    );
    assert!(
        !text.contains("SIGABRT") && !text.contains("signal: 6"),
        "host abort leaked:\n{text}"
    );

    let _ = std::fs::remove_dir_all(root);
}
