use harc::codegen::{cpp_tb, tbir};
use harc::parser::parse_source;

const SOURCE: &str = r#"
scoreboard Registers
    regs : Vec<uint<32>, 32>
    writes : uint<32> default 0
    pending : queue<uint<32>>
end scoreboard Registers

testbench Tb
    dut : Top
    a : Registers
    b : Registers
end testbench Tb

impl Check for Tb
    run
        for i in 0 .. 31
            a.regs[i] = i + 1
            b.regs[i] = 100 + i
        end for
        a.pending.push(42)
        let item : uint<32> = a.pending.pop()
        assert item == 42 else fail("queue value")
        assert b.pending.empty() else fail("queue isolation")
        a.writes = a.writes + 1
        for i in 0 .. 31
            assert a.regs[i] == i + 1 else fail("a lane")
            assert b.regs[i] == 100 + i else fail("b lane")
        end for
        assert a.writes == 1 else fail("scalar field")
        assert b.writes == 0 else fail("instance isolation")
    end run
end impl Check
"#;

#[test]
fn data_only_scoreboard_fixed_vectors_lower_as_instance_state() {
    let parsed = parse_source(SOURCE).unwrap();
    let program =
        harc::ir::lower::lower_program(&parsed).expect("fixed-vector scoreboard must lower");
    harc::ir::verify::verify_program(&program).unwrap();
    tbir::emit(&program, &parsed, &cpp_tb::EmitOpts::default()).unwrap();
}

#[test]
fn scoreboard_vectors_execute_in_both_tbir_layouts() {
    use std::process::Command;
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "Verilator required"
    );
    if !present {
        eprintln!("skipping: Verilator absent");
        return;
    }
    let dir = std::env::temp_dir().join(format!("harc-sb-vec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("scoreboard.harc");
    std::fs::write(&source, SOURCE).unwrap();
    for layout in ["self-contained", "common"] {
        let result = Command::new(env!("CARGO_BIN_EXE_harc"))
            .env("HARC_NO_LEARN", "1")
            .args([
                "sim",
                "--sv",
                "tests/dut/top_counter.sv",
                "--top",
                "Top",
                "--cpp-split",
                "tests",
                "--cpp-split-layout",
                layout,
                "--outdir",
            ])
            .arg(dir.join(layout))
            .arg(&source)
            .output()
            .unwrap();
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result.status.success() && log.contains("ALL TESTS PASSED"),
            "{layout}: {log}"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn vector_routing_preserves_rejection_of_aggregate_defaults() {
    for field in [
        "q : queue<uint<8>> default 0",
        "v : Vec<uint<8>, 2> default 0",
    ] {
        let source =
            format!("scoreboard S\n regs : Vec<uint<32>, 32>\n {field}\nend scoreboard S\n");
        let parsed = parse_source(&source).unwrap();
        assert!(
            harc::ir::lower::lower_program(&parsed).is_err(),
            "must not discard {field}"
        );
    }
}

#[test]
fn vector_routing_does_not_enable_events_or_nested_boards() {
    for field in ["ev : event<uint<8>>", "child : Child"] {
        let source = format!("scoreboard Child\n count : uint<8> default 0\nend scoreboard Child\nscoreboard S\n regs : Vec<uint<32>, 32>\n {field}\nend scoreboard S\n");
        let parsed = parse_source(&source).unwrap();
        assert!(
            harc::ir::lower::lower_program(&parsed).is_err(),
            "unexpectedly accepted {field}"
        );
    }
}
