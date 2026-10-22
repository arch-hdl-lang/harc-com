//! Positive TBIR type-space matrices retained from the former V1 differential suite.
//!
//! Every row must parse, lower, verify, emit C++, and typecheck against the
//! Verilator stub. These are TBIR capability tests, not backend comparisons.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{lower, verify};
use harc::parser::parse_source;

fn cxx() -> Option<&'static str> {
    ["g++", "c++", "clang++"].into_iter().find(|compiler| {
        Command::new(compiler)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn assert_tbir_compiles(compiler: &str, label: &str, source: &str) {
    static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);
    let parsed = parse_source(source).unwrap_or_else(|error| panic!("{label}: parse: {error}"));
    let merged = merge::merge_for_sim(vec![parsed], None)
        .unwrap_or_else(|error| panic!("{label}: merge: {error}"));
    let program =
        lower::lower_program(&merged).unwrap_or_else(|error| panic!("{label}: lower: {error}"));
    verify::verify_program(&program).unwrap_or_else(|errors| panic!("{label}: verify: {errors:?}"));
    let cpp = tbir::emit(&program, &merged, &cpp_tb::EmitOpts::default())
        .unwrap_or_else(|error| panic!("{label}: emit: {error}"));

    let scratch = std::env::temp_dir().join(format!(
        "harc-tbir-type-space-{}-{}",
        std::process::id(),
        NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&scratch).expect("create TBIR type-space scratch directory");
    let cpp_path = scratch.join("probe.cpp");
    std::fs::write(&cpp_path, cpp).expect("write emitted C++");
    let output = Command::new(compiler)
        .args(["-std=gnu++20", "-fcoroutines", "-fsyntax-only"])
        .arg("-I")
        .arg(manifest("runtime"))
        .arg("-I")
        .arg(manifest("tests/vstub"))
        .arg(&cpp_path)
        .output()
        .unwrap_or_else(|error| panic!("{label}: spawn {compiler}: {error}"));
    assert!(
        output.status.success(),
        "{label}: emitted C++ did not typecheck:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::remove_dir_all(scratch).ok();
}

fn assert_space(template: &str, hole: &str, substitutions: &[&str]) {
    let Some(compiler) = cxx() else {
        assert!(
            std::env::var_os("HARC_SKIP_CXX_PROBE").is_some(),
            "no C++ compiler on PATH; set HARC_SKIP_CXX_PROBE=1 to skip deliberately"
        );
        return;
    };
    for substitution in substitutions {
        assert_tbir_compiles(
            compiler,
            substitution,
            &template.replace(hole, substitution),
        );
    }
}

#[test]
fn wide_scalar_storage_lowers_and_compiles_across_host_locations() {
    let widths = ["uint<64>", "uint<65>", "uint<128>", "uint<1024>"];
    for template in [
        r#"testbench Tb
    dut : Top
    w : @@TY@@ default 1
end testbench Tb
impl T for Tb
    run
        w = 2
        wait 1 cycle
    end run
    check
        assert w == 2
    end check
end impl T"#,
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    let w : @@TY@@ = 1
    run
        wait 1 cycle
    end run
    check
        assert w == 1
    end check
end impl T"#,
        r#"bus TlmMemBus
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus TlmMemBus
transactor TlmMemTarget bound to TlmMemBus
    w : @@TY@@ default 1
    thread bus.read(addr: uint<8>)
        w = w + 1
        return 1
    end thread
end transactor TlmMemTarget
testbench Tb
    dut : TlmReadInitiator
end testbench Tb
impl T for Tb
    let mem : TlmMemBus = bind dut
    let target : TlmMemTarget passive = bind mem
    run
        wait 1 cycle
    end run
end impl T"#,
        r#"scoreboard Sb
    w : @@TY@@ default 1
end scoreboard Sb
testbench Tb
    dut : Top
    sb : Sb
end testbench Tb
impl T for Tb
    run
        sb.w = 2
        wait 1 cycle
    end run
    check
        assert sb.w == 2
    end check
end impl T"#,
        r#"transactor Src
    w : @@TY@@ default 1
    hookable bump(v: uint<8>)
        w = w + 1
    end bump
end transactor Src
testbench Tb
    dut : Top
    src : Src passive
end testbench Tb
impl T for Tb
    run
        src.bump(1)
        wait 1 cycle
    end run
    check
        assert src.w == 2
    end check
end impl T"#,
    ] {
        assert_space(template, "@@TY@@", &widths);
    }
}

#[test]
fn wide_scalar_arithmetic_lowers_and_compiles_for_locals_and_fields() {
    let widths = ["uint<64>", "uint<128>", "uint<1024>"];
    for template in [
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let w : @@TY@@ = 1
        w = w + 1
        wait 1 cycle
    end run
end impl T"#,
        r#"testbench Tb
    dut : Top
    w : @@TY@@ default 1
end testbench Tb
impl T for Tb
    run
        w = w + 1
        wait 1 cycle
    end run
end impl T"#,
    ] {
        assert_space(template, "@@TY@@", &widths);
    }
}

#[test]
fn event_connect_widening_lowers_and_compiles_at_every_storage_width() {
    let template = r#"transactor NarrowSrc
    ev : out event<@@TY@@>
    hookable bump(v: uint<8>)
        emit ev(v)
    end bump
end transactor NarrowSrc
transactor WideSink
    observe : event<uint<1024>>
    seen : uint<32> default 0
    on observe(v)
        seen = seen + 1
    end on
end transactor WideSink
testbench Tb
    dut : Top
    src : NarrowSrc passive
    sink : WideSink passive
    connect
        src.ev -> sink.observe
    end connect
end testbench Tb
impl T for Tb
    run
        src.bump(1)
        wait 1 cycle
    end run
end impl T"#;
    assert_space(
        template,
        "@@TY@@",
        &["uint<8>", "uint<64>", "uint<65>", "uint<128>", "uint<1024>"],
    );
}

#[test]
fn nested_fixed_vector_component_fields_compile_at_every_leaf_width() {
    let template = r#"scoreboard NestSb
    v : Vec<Vec<@@TY@@, 2>, 2>
    n : uint<1024> default 0
    hookable put(x: uint<8>)
        v[0][1] = 1
        n = n + v[0][1]
    end put
end scoreboard NestSb
testbench Tb
    dut : Top
    sb : NestSb
end testbench Tb
impl T for Tb
    run
        wait 1 cycle
    end run
end impl T"#;
    assert_space(
        template,
        "@@TY@@",
        &["uint<8>", "sint<8>", "uint<64>", "uint<128>", "uint<1024>"],
    );
}

#[test]
fn valid_wide_operator_forms_are_not_over_refused() {
    let template = r#"testbench Tb
    dut : Top
    a : uint<160> default 1
    b : uint<256> default 1
    n : uint<64> default 1
    s : uint<160> default 0
end testbench Tb
impl T for Tb
    run
@@OP@@
        wait 1 cycle
    end run
end impl T"#;
    assert_space(
        template,
        "@@OP@@",
        &[
            "        assert (a == 1) and (b == 1)",
            "        assert (a == 1) or (b == 1)",
            "        s = a + a",
            "        s = a / a",
            "        assert a < a + 1",
            "        s = a << 1",
            "        s = a >> n",
            "        assert a == b",
            "        assert a != b",
            "        assert n < 8",
            "        repeat n\n            s = s + 1\n        end repeat",
            "        for i in 0 .. n\n            s = s + 1\n        end for",
        ],
    );
}

#[test]
fn testbench_fixed_vector_fields_compile_across_element_classes() {
    let single = r#"testbench Tb
    dut : Top
    mem : @@TY@@
end testbench Tb
impl T for Tb
    run
        mem[0] = 5
        mem[1] = mem[0]
        wait 1 cycle
    end run
end impl T"#;
    assert_space(
        single,
        "@@TY@@",
        &[
            "Vec<uint<8>, 4>",
            "Vec<sint<8>, 4>",
            "Vec<bool, 4>",
            "Vec<uint<64>, 4>",
            "Vec<uint<128>, 4>",
            "Vec<uint<1024>, 4>",
        ],
    );

    let nested = r#"testbench Tb
    dut : Top
    mem : @@TY@@
end testbench Tb
impl T for Tb
    run
        mem[0][1] = 5
        mem[1][0] = mem[0][1]
        wait 1 cycle
    end run
end impl T"#;
    assert_space(
        nested,
        "@@TY@@",
        &[
            "Vec<Vec<uint<8>, 2>, 2>",
            "Vec<Vec<sint<8>, 2>, 2>",
            "Vec<Vec<uint<128>, 2>, 2>",
        ],
    );
}

#[test]
fn no_argument_event_handlers_compile_on_agent_and_env_hosts() {
    let Some(compiler) = cxx() else {
        assert!(std::env::var_os("HARC_SKIP_CXX_PROBE").is_some());
        return;
    };
    for kind in ["agent", "env"] {
        let source = format!(
            r#"{kind} Tagger
    in_ev : event<uint<8>>
    seen : uint<32> default 0
    on in_ev()
        seen = seen + 1
    end on
end {kind} Tagger
testbench Tb
    dut : Top
    tagger : Tagger
end testbench Tb
impl T for Tb
    run
        emit tagger.in_ev(1)
        wait 1 cycle
    end run
end impl T"#
        );
        assert_tbir_compiles(compiler, kind, &source);
    }
}
