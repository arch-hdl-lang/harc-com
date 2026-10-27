use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn harc_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_harc"))
}

#[test]
fn bare_return_does_not_call_next_line_statement() {
    let root = std::env::temp_dir().join(format!("harc_return_newline_{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create native regression directory");
    let dut = root.join("Top.sv");
    let source = r#"testbench ReturnTb
    dut : Top
end testbench ReturnTb
impl ReturnTest for ReturnTb
    run
        return
        assert false else fail("UNREACHABLE_CALLED")
    end run
end impl ReturnTest
"#;
    fs::write(
        &dut,
        "module Top(input logic clk, input logic rst, output logic out); assign out = rst; endmodule\n",
    )
    .expect("write DUT regression");

    let run = |name: &str, source: &str| {
        let harc = root.join(format!("{name}.harc"));
        let outdir = root.join(format!("out_{name}"));
        fs::write(&harc, source).expect("write HARC regression");
        let output = Command::new(harc_bin())
            .args([
                "sim",
                "--sv",
                dut.to_str().expect("DUT path is valid UTF-8"),
                harc.to_str().expect("HARC path is valid UTF-8"),
                "--top",
                "Top",
                "--outdir",
                outdir.to_str().expect("output path is valid UTF-8"),
            ])
            .env("HARC_NO_LEARN", "1")
            .env("ARCH_NO_LEARN", "1")
            .env("HARC_REQUIRE_VERILATOR", "1")
            .output()
            .expect("run native return regression");
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        (output.status.success(), log)
    };

    let (passed, log) = run("positive", source);
    assert!(passed, "native regression failed:\n{log}");
    assert!(
        log.contains("ALL TESTS PASSED"),
        "native test did not pass:\n{log}"
    );

    let no_return = source.replacen("        return\n", "", 1);
    let (passed, log) = run("counterfactual", &no_return);
    assert!(!passed, "counterfactual unexpectedly passed:\n{log}");
    assert!(
        log.contains("UNREACHABLE_CALLED"),
        "counterfactual did not reach g():\n{log}"
    );
}
