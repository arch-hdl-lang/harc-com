//! Codegen tests. The full end-to-end harc-sim → arch-sim run isn't covered
//! here (it depends on the sibling arch-com checkout being buildable); the
//! `harc sim` invocation in `examples/`-driven scripts validates that.
//! Here we just snapshot the C++ that comes out of `cpp_tb::emit`.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::parser::parse_source;

fn randomize_problem_ids(file: &harc::ast::SourceFile) -> Vec<u64> {
    harc::solver::problem_table::build_typed_solver_problem_table(file)
        .entries
        .iter()
        .filter_map(|entry| match (&entry.source, &entry.build) {
            (
                harc::solver::problem_table::TypedSolverProblemSource::RandomizeSite { .. },
                harc::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. },
            ) => Some(typed.problem_id.0),
            _ => None,
        })
        .collect()
}

fn component_solver_site_tags(cpp: &str) -> std::collections::BTreeSet<&str> {
    cpp.match_indices("_solver_site_h")
        .map(|(start, _)| {
            let tail = &cpp[start..];
            let end_marker = tail.find("_e").expect("site tag has an end-span marker") + 2;
            let digit_count = tail[end_marker..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            &tail[..end_marker + digit_count]
        })
        .collect()
}

fn v1_enum_symbol(cpp: &str, source_name: &str) -> String {
    let prefix = format!("harc_user_enum_{source_name}_");
    cpp.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .find(|token| token.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no generated enum symbol for `{source_name}` in:\n{cpp}"))
        .to_string()
}

#[test]
fn dut_interface_catalog_preserves_direction_signedness_and_lane_shape() {
    let dir =
        std::env::temp_dir().join(format!("harc_dut_interface_catalog_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create interface-catalog test directory");
    let sv = dir.join("CatalogTop.sv");
    let arch = dir.join("CatalogTop.arch");
    std::fs::write(
        &sv,
        r#"typedef logic signed [7:0] signed_byte_t;
typedef int signed_int_t;
typedef int unsigned unsigned_int_t;
typedef signed_int_t signed_alias_t;
module CatalogTop #(
  parameter int WIDTH = 8,
  parameter int LANES = 2,
  parameter type T = signed_alias_t
)(
  input logic clk,
  input logic rst,
  input logic bool_in,
  output logic signed [7:0] signed_out,
  output signed_byte_t typedef_signed_out,
  output unsigned int unsigned_prefix,
  output int unsigned unsigned_suffix,
  output signed_int_t typedef_int_out,
  output signed_alias_t typedef_alias_out,
  output T parameter_typedef_out,
  input logic [WIDTH-1:0] parameterized_in,
  input logic [LANES-1:0][WIDTH-1:0] packed_lanes,
  output logic [15:0] unpacked_words [4]
);
endmodule
module Unrelated;
  typedef logic [3:0] signed_alias_t;
endmodule
module CompactTop #(parameter int WIDTH = 9, parameter type T = logic signed [WIDTH-1:0]) (output T compact_q, input logic [WIDTH-1:0] compact_in);
endmodule
"#,
    )
    .expect("write interface fixture");
    std::fs::write(
        &arch,
        r#"module CatalogTop
  port clk: in Clock<SysDomain>;
  port rst: in Reset<Sync>;
  port bool_in: in Bool;
  port signed_out: out SInt<8>;
  port typedef_signed_out: out SInt<8>;
end module CatalogTop
"#,
    )
    .expect("write native interface fixture");
    let catalog = cpp_tb::dut_interface_catalog(
        &[sv],
        &[arch],
        "CatalogTop",
        &std::collections::HashMap::from([("packed_lanes".to_string(), 8)]),
    )
    .expect("resolve DUT interface")
    .expect("catalog exists");
    assert_eq!(catalog.dut_type(), "CatalogTop");
    assert_eq!(
        catalog.port("clk").unwrap().value_type(),
        &harc::ir::IrType::UInt(Some(1))
    );
    assert_eq!(
        catalog.port("rst").unwrap().value_type(),
        &harc::ir::IrType::UInt(Some(1))
    );
    assert_eq!(
        catalog.port("bool_in").unwrap().value_type(),
        &harc::ir::IrType::Bool
    );
    let signed = catalog.port("signed_out").expect("signed output");
    assert_eq!(signed.direction(), harc::ir::PortDirection::Out);
    assert_eq!(signed.resolved_width(), Some(8));
    assert_eq!(signed.value_type(), &harc::ir::IrType::SInt(Some(8)));
    let typedef_signed = catalog
        .port("typedef_signed_out")
        .expect("signed typedef output");
    assert_eq!(typedef_signed.resolved_width(), Some(8));
    assert_eq!(
        typedef_signed.value_type(),
        &harc::ir::IrType::SInt(Some(8))
    );
    let packed = catalog.port("packed_lanes").expect("packed lanes");
    assert_eq!(packed.resolved_width(), Some(16));
    assert_eq!(packed.packed_lane_width(), Some(8));
    let unpacked = catalog.port("unpacked_words").expect("unpacked words");
    assert_eq!(unpacked.resolved_width(), Some(16));
    assert_eq!(unpacked.unpacked_elements(), Some(4));
    for name in ["unsigned_prefix", "unsigned_suffix"] {
        assert_eq!(
            catalog.port(name).unwrap().value_type(),
            &harc::ir::IrType::UInt(Some(32))
        );
    }
    for name in [
        "typedef_int_out",
        "typedef_alias_out",
        "parameter_typedef_out",
    ] {
        assert_eq!(
            catalog.port(name).unwrap().value_type(),
            &harc::ir::IrType::SInt(Some(32))
        );
    }
    let overridden = cpp_tb::dut_interface_catalog_with_parameter_overrides(
        &[dir.join("CatalogTop.sv")],
        &[dir.join("CatalogTop.arch")],
        "CatalogTop",
        &std::collections::HashMap::from([("packed_lanes".to_string(), 8)]),
        &["WIDTH=13".to_string(), "LANES=3".to_string()],
    )
    .expect("resolve overridden DUT interface")
    .expect("overridden catalog exists");
    assert_eq!(
        overridden
            .port("parameterized_in")
            .unwrap()
            .resolved_width(),
        Some(13)
    );
    assert_eq!(
        overridden.port("packed_lanes").unwrap().resolved_width(),
        Some(39)
    );
    assert_eq!(
        overridden.port("packed_lanes").unwrap().packed_lane_width(),
        Some(13)
    );
    let compact = cpp_tb::dut_interface_catalog(
        &[dir.join("CatalogTop.sv")],
        &[],
        "CompactTop",
        &std::collections::HashMap::new(),
    )
    .expect("resolve compact ANSI DUT interface")
    .expect("compact ANSI catalog exists");
    assert_eq!(
        compact.port("compact_q").unwrap().value_type(),
        &harc::ir::IrType::SInt(Some(9))
    );
    assert_eq!(
        compact.port("compact_in").unwrap().resolved_width(),
        Some(9)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn dut_interface_catalog_resolves_imported_package_symbols_and_verilator_names() {
    let dir = std::env::temp_dir().join(format!(
        "harc_dut_interface_packages_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create package interface-catalog directory");
    let package = dir.join("width_pkg.sv");
    let top = dir.join("PackageTop.sv");
    std::fs::write(
        &package,
        r#"package width_pkg;
  localparam int WIDTH = 13;
  typedef struct packed {
    logic [WIDTH-1:0] data;
    logic valid;
  } payload_t;
endpackage
"#,
    )
    .expect("write package fixture");
    std::fs::write(
        &top,
        r#"import width_pkg::*;
module PackageTop(
  input logic [WIDTH-1:0] payload,
  input payload_t imported_payload,
  input width_pkg::payload_t qualified_payload,
  input logic TEST__ASYNC_DISABLE
);
endmodule
"#,
    )
    .expect("write package-backed top fixture");

    let catalog = cpp_tb::dut_interface_catalog(
        &[package, top],
        &[],
        "PackageTop",
        &std::collections::HashMap::new(),
    )
    .expect("resolve package-backed DUT interface")
    .expect("package-backed catalog exists");
    assert_eq!(catalog.port("payload").unwrap().resolved_width(), Some(13));
    for name in ["imported_payload", "qualified_payload"] {
        assert_eq!(catalog.port(name).unwrap().resolved_width(), Some(14));
    }
    assert_eq!(
        catalog
            .port_by_physical_name("TEST___05FASYNC_DISABLE")
            .unwrap()
            .name(),
        "TEST__ASYNC_DISABLE"
    );
    let source = parse_source(
        r#"test PhysicalPort
    let dut : PackageTop
    run
        dut.TEST___05FASYNC_DISABLE = 0
    end run
end test PhysicalPort
"#,
    )
    .expect("physical-port fixture parses");
    let source = merge::merge_for_sim(vec![source], None).expect("physical-port fixture merges");
    let program = harc::ir::lower::lower_program(&source).expect("physical-port fixture lowers");
    harc::ir::verify::verify_program(&program).expect("physical-port fixture verifies");
    harc::ir::passes::dut_access::analyze(&program, &catalog)
        .expect("Verilator physical port spelling resolves through the DUT catalog");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn native_interface_catalog_covers_fifo_type_params_and_grouped_ram_ports() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let fifo = cpp_tb::dut_interface_catalog(
        &[],
        &[root.join("tests/dut/sync_fifo.arch")],
        "TxQueue",
        &std::collections::HashMap::new(),
    )
    .expect("scan native FIFO interface")
    .expect("FIFO catalog exists");
    assert_eq!(fifo.port("clk").unwrap().resolved_width(), Some(1));
    assert_eq!(fifo.port("rst").unwrap().resolved_width(), Some(1));
    assert_eq!(
        fifo.port("push_data").unwrap().value_type(),
        &harc::ir::IrType::UInt(Some(8))
    );
    assert_eq!(
        fifo.port("pop_data").unwrap().direction(),
        harc::ir::PortDirection::Out
    );

    let ram = cpp_tb::dut_interface_catalog(
        &[],
        &[root.join("tests/dut/rom_lut.arch")],
        "RomLut",
        &std::collections::HashMap::new(),
    )
    .expect("scan native RAM interface")
    .expect("RAM catalog exists");
    assert_eq!(ram.port("rd_addr").unwrap().resolved_width(), Some(3));
    assert_eq!(
        ram.port("rd_en").unwrap().value_type(),
        &harc::ir::IrType::Bool
    );
    assert_eq!(
        ram.port("rd_data").unwrap().direction(),
        harc::ir::PortDirection::Out
    );

    let arbiter = cpp_tb::dut_interface_catalog(
        &[],
        &[root.join("tests/dut/bus_arbiter.arch")],
        "BusArbiter",
        &std::collections::HashMap::new(),
    )
    .expect("scan native arbiter interface")
    .expect("arbiter catalog exists");
    for name in ["request_valid", "request_ready"] {
        let port = arbiter.port(name).expect("flattened handshake signal");
        assert_eq!(port.resolved_width(), Some(4));
        assert_eq!(port.packed_lane_width(), Some(1));
        assert_eq!(port.packed_lane_type(), Some(&harc::ir::IrType::Bool));
    }
    assert_eq!(
        arbiter.port("request_valid").unwrap().direction(),
        harc::ir::PortDirection::In
    );
    assert_eq!(
        arbiter.port("request_ready").unwrap().direction(),
        harc::ir::PortDirection::Out
    );

    let dir =
        std::env::temp_dir().join(format!("harc_native_group_catalog_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create native group catalog directory");
    let grouped = dir.join("Grouped.arch");
    std::fs::write(
        &grouped,
        r#"module Grouped
  param COUNT: const = 3;
  ports[COUNT] req
    valid: in Bool;
    data: in SInt<8>;
  end ports req
end module Grouped
"#,
    )
    .expect("write native grouped interface");
    let grouped = cpp_tb::dut_interface_catalog(
        &[],
        &[grouped],
        "Grouped",
        &std::collections::HashMap::new(),
    )
    .expect("scan native grouped interface")
    .expect("grouped catalog exists");
    let valid = grouped.port("req_valid").expect("grouped valid");
    assert_eq!(valid.resolved_width(), Some(3));
    assert_eq!(valid.packed_lane_width(), Some(1));
    assert_eq!(valid.packed_lane_type(), Some(&harc::ir::IrType::Bool));
    let data = grouped.port("req_data").expect("grouped data");
    assert_eq!(data.resolved_width(), Some(24));
    assert_eq!(data.packed_lane_width(), Some(8));
    assert_eq!(data.value_type(), &harc::ir::IrType::SInt(Some(24)));
    assert_eq!(
        data.packed_lane_type(),
        Some(&harc::ir::IrType::SInt(Some(8)))
    );
    let overridden = cpp_tb::dut_interface_catalog_with_parameter_overrides(
        &[],
        &[dir.join("Grouped.arch")],
        "Grouped",
        &std::collections::HashMap::new(),
        &["COUNT=5".to_string()],
    )
    .expect("scan overridden native grouped interface")
    .expect("overridden grouped catalog exists");
    assert_eq!(
        overridden.port("req_valid").unwrap().resolved_width(),
        Some(5)
    );
    assert_eq!(
        overridden.port("req_data").unwrap().resolved_width(),
        Some(40)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn parameterized_ansi_packed_lanes_use_one_catalog_and_reject_wide_elements() {
    let dir = std::env::temp_dir().join(format!(
        "harc_wide_packed_lane_catalog_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create wide packed-lane test directory");
    let harc_source = parse_source(
        r#"test WideLane
    let dut : LaneTop
    clock clk = 10ns
    run
        dut.lanes[0] = 1
    end run
end test WideLane"#,
    )
    .expect("wide packed-lane HARC parses");
    let merged = merge::merge_for_sim(vec![harc_source], None).expect("wide lane source merges");
    let program = harc::ir::lower::lower_program(&merged).expect("wide lane source lowers");
    harc::ir::verify::verify_program(&program).expect("wide lane IR verifies structurally");

    for (name, sv) in [
        (
            "one_line",
            "module LaneTop #(parameter int W=8) (input logic clk, input logic [1:0][W-1:0] lanes); endmodule\n",
        ),
        (
            "multi_line",
            "module LaneTop #(\n  parameter int W=8\n) (\n  input logic clk,\n  input logic [1:0][W-1:0] lanes\n);\nendmodule\n",
        ),
    ] {
        let sv_path = dir.join(format!("{name}.sv"));
        std::fs::write(&sv_path, sv).expect("write packed-lane SV fixture");
        let narrow_catalog = cpp_tb::dut_interface_catalog_with_parameter_overrides(
            std::slice::from_ref(&sv_path),
            &[],
            "LaneTop",
            &std::collections::HashMap::new(),
            &["W=8".to_string()],
        )
        .expect("scan narrow parameterized packed-lane catalog")
        .expect("narrow packed-lane catalog exists");
        let mut narrow_opts = cpp_tb::EmitOpts::default();
        narrow_opts.dut_interface = Some(narrow_catalog);
        for emitted in [
            cpp_tb::emit_with_opts(&merged, narrow_opts.clone())
                .expect("v1 emits the canonical narrow packed lane"),
            harc::codegen::tbir::emit(&program, &merged, &narrow_opts)
                .expect("TBIR emits the canonical narrow packed lane"),
        ] {
            assert!(
                emitted.contains("harc_vec_lane_write<8>"),
                "{name}: canonical lane shape was not used:\n{emitted}"
            );
            assert!(
                !emitted.contains("dut->lanes[0]"),
                "{name}: packed lane fell back to raw C++ indexing:\n{emitted}"
            );
        }
        let catalog = cpp_tb::dut_interface_catalog_with_parameter_overrides(
            &[sv_path],
            &[],
            "LaneTop",
            &std::collections::HashMap::new(),
            &["W=128".to_string()],
        )
        .expect("scan parameterized packed-lane catalog")
        .expect("packed-lane catalog exists");
        assert_eq!(catalog.port("lanes").unwrap().resolved_width(), Some(256));
        assert_eq!(
            catalog.port("lanes").unwrap().packed_lane_width(),
            Some(128)
        );
        let mut opts = cpp_tb::EmitOpts::default();
        opts.dut_interface = Some(catalog);
        for error in [
            cpp_tb::emit_with_opts(&merged, opts.clone())
                .expect_err("v1 must reject a packed lane wider than its helper carrier"),
            harc::codegen::tbir::emit(&program, &merged, &opts)
                .expect_err("TBIR must reject the same packed lane"),
        ] {
            assert!(error.0.contains("64"), "{name}: {error}");
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

fn compile_and_run_runtime_cpp(name: &str, body: &str) {
    use std::fs;
    use std::process::Command;

    let dir = std::env::temp_dir().join(format!("harc_runtime_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let cpp = dir.join("test.cpp");
    let bin = dir.join("test_bin");
    fs::write(
        &cpp,
        format!(
            "#include <cassert>\n#include <cstdint>\n#include \"{}\"\nint main() {{\n{}\nreturn 0;\n}}\n",
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("runtime/harc_thread_rt.h")
                .display(),
            body
        ),
    )
    .unwrap();
    let compile = Command::new("c++")
        .arg("-std=c++20")
        .arg(&cpp)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("spawn c++");
    assert!(
        compile.status.success(),
        "compile failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr)
    );
    let run = Command::new(&bin)
        .output()
        .expect("run runtime helper test");
    assert!(
        run.status.success(),
        "runtime helper test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}

// Snapshot test for the all-in-one counter TB form lives in
// `split_test_via_extend_round_trips_to_same_cpp`, which exercises the
// split-file form and locks the same emitted C++ via insta.

#[test]
fn missing_test_is_a_clean_error() {
    let parsed = parse_source("transaction T\n  addr : uint<32>\nend transaction T").unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(err.0.contains("no `test` declaration"));
}

#[test]
fn arch_scalar_bit_select_uses_lane_helper_in_both_emitters() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let parsed = parse_source(include_str!("fixtures/lazy_message_calls_fail_test.harc"))
        .expect("parse scalar bit-select fixture");
    let merged = merge::merge_for_sim(vec![parsed], Some("LazyMessageCallsFailTest"))
        .expect("merge selected test");

    let scalar_widths =
        cpp_tb::dut_port_widths_from_files(&[root.join("tests/dut/top_counter.arch")], "Top");
    let mut opts = cpp_tb::EmitOpts::default();
    cpp_tb::add_arch_scalar_bit_lanes(&mut opts.vec_lane_widths, &scalar_widths);

    let v1 = cpp_tb::emit_with_opts(&merged, opts.clone()).expect("v1 emits");
    let prog = harc::ir::lower::lower_program(&merged).expect("TBIR lowers");
    harc::ir::verify::verify_program(&prog).expect("TBIR verifies");
    let tbir = tbir::emit(&prog, &merged, &opts).expect("TBIR emits");

    assert!(
        v1.contains("harc_rt::harc_vec_lane_read<1>(dut->count_out"),
        "v1 must lower the scalar bit-select"
    );
    assert!(
        tbir.contains("harc_rt::harc_vec_lane_read<1>"),
        "TBIR must lower the scalar bit-select"
    );
}

#[test]
fn semantic_trace_runtime_and_events_emit() {
    let src = r#"
transaction Req
    addr : uint<32>
end transaction Req

test TraceTest
    let dut : Top
    run
        let t : Req
        randomize(t)
        assert false else fail("boom")
    end run
end test TraceTest
"#;
    let parsed = parse_source(src).unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");
    assert!(cpp.contains("#include \"harc_trace_rt.h\""));
    assert!(cpp.contains("#include \"harc_log_rt.h\""));
    assert!(cpp.contains("struct HarcTestContext {"));
    assert!(cpp.contains("auto* dut = ctx.dut;"));
    assert!(cpp.contains("auto& _checkers = ctx._checkers;"));
    assert!(cpp.contains("harc_rt::trace::HarcTraceWriter trace;"));
    assert!(cpp.contains("harc_rt::trace::harc_start_trace(trace, harc_rng.state, \"Top\", \"TraceTest\", cycle_count);"));
    assert!(cpp.contains("trace.set_timing(t, clock, clock_cycle);"));
    assert!(cpp.contains("trace.randomize(cycle_count, _trace_fields);"));
    assert!(cpp.contains("HARC_RT_LOG_PRINTF(log_ctx.sim_log, &trace, cycle_count, sev, fmt);"));
    assert!(cpp.contains(
        "return harc_rt::log::harc_finish_sim_run(log_ctx, trace, cycle_count, ctx.errors);"
    ));
    assert!(cpp_tb::TRACE_RT_HEADER.contains("vcd_time"));
    assert!(cpp_tb::TRACE_RT_HEADER.contains("clock_cycle"));
    assert!(cpp_tb::TRACE_RT_HEADER.contains("raw(\"assertion_failure\""));
    assert!(cpp_tb::TRACE_RT_HEADER.contains("raw(\"tlm_call\""));
}

#[test]
fn semantic_trace_tlm_method_calls_emit() {
    let parsed = parse_source(
        r#"bus B
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
    tlm_method read_ooo(addr: uint<8>) -> uint<32>: out_of_order tags 2;
end bus B

test T
    let dut : SomeDut
    let b : B = bind dut
    run
        let got = b.read(3)
        let later = fork b.read_ooo(4)
        join_all
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("trace.tlm_call(cycle_count, \"\", \"b\", \"read\", \"request\", \"initiator\");")
            && cpp.contains("trace.tlm_call(cycle_count, \"\", \"b\", \"read\", \"response\", \"initiator\");")
            && cpp.contains("trace.tlm_call(cycle_count, \"\", \"b\", \"read_ooo\", \"request\", \"initiator\", (int64_t)(dut->b_read_ooo_req_tag));")
            && cpp.contains("trace.tlm_call(cycle_count, \"\", \"b\", \"read_ooo\", \"response\", \"initiator\", (int64_t)(0));"),
        "expected initiator TLM trace events; got:\n{cpp}"
    );
}

#[test]
fn blocking_fork_tlm_method_keeps_req_ready_wait() {
    let parsed = parse_source(
        r#"bus B
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus B

test T
    let dut : SomeDut
    let b : B = bind dut
    run
        let later = fork b.read(4)
        join_all
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("while (!dut->b_read_req_ready && _b > 0)")
            && cpp.contains("// join_all bus.read response"),
        "blocking fork must preserve the req_ready wait path:\n{cpp}"
    );
}

#[test]
fn runtime_random_problem_table_emits_without_switching_solver_path() {
    let src = r#"
transaction Req
    addr : uint<8>
    keep addr != 7
end transaction Req

test RuntimeProblemTableTest
    let dut : Top
    run
        let t : Req
        randomize(t) with
            t.addr != 9
        end randomize
    end run
end test RuntimeProblemTableTest
"#;
    let parsed = parse_source(src).unwrap();
    let [problem_id] = randomize_problem_ids(&parsed)[..] else {
        panic!("expected one randomize problem")
    };
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    assert!(cpp.contains("#include \"harc_random_rt.h\""));
    assert!(
        cpp.contains("HarcRuntimeProblemDescriptor _harc_runtime_random_problem_table_entries[]")
    );
    assert!(cpp.contains("HarcRuntimeProblemTable _harc_runtime_random_problem_table"));
    assert!(
        cpp.contains("std::vector<harc_rt::random::HarcRuntimeCallSite> _harc_solver_call_sites{")
    );
    assert!(cpp.contains("{1ULL, \"randomize(Req)\""));
    assert!(cpp.contains(&format!("{{{problem_id}ULL, \"randomize(Req) with\"")));
    assert!(cpp.contains("{1ULL, 1ULL, 0}"));
    assert!(cpp.contains(&format!("{{{problem_id}ULL, {problem_id}ULL, 0}}")));
    assert!(cpp.contains("HarcRandomizeCall harc_prepare_randomize_call"));
    assert!(cpp.contains("harc_rt::random::harc_prepare_randomize_call("));
    assert!(cpp.contains(&format!(
        "auto _harc_rt_call = harc_prepare_randomize_call(ctx, {problem_id}ULL, ctx.rng.initial_seed, 0);"
    )));
    assert!(cpp.contains("auto* _harc_rt_problem = _harc_rt_call.problem;"));
    assert!(cpp.contains("auto _harc_rt_seed = _harc_rt_call.seed;"));
    assert!(cpp
        .contains("auto _harc_rt_generated_solver = [&]() -> harc_rt::random::HarcSolveStatus {"));
    assert!(cpp.contains("harc_rt::random::harc_solve_constrained("));
    assert!(cpp.contains("harc_rt::random::HarcSolveMode::Queued"));
    assert!(
        cpp.contains("z3::context _ctx;"),
        "runtime constrained callback must still delegate to generated inline Z3 for now; got:\n{cpp}"
    );
}

#[test]
fn unconstrained_randomize_routes_through_runtime_shell() {
    let src = r#"
transaction Empty
end transaction Empty

test RuntimeFastPathTest
    let dut : Top
    run
        let t : Empty
        randomize(t)
    end run
end test RuntimeFastPathTest
"#;
    let parsed = parse_source(src).unwrap();
    let [problem_id] = randomize_problem_ids(&parsed)[..] else {
        panic!("expected one randomize problem")
    };
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    assert!(cpp.contains(&format!(
        "auto _harc_rt_call = harc_prepare_randomize_call(ctx, {problem_id}ULL, ctx.rng.initial_seed, 0);"
    )));
    assert!(cpp.contains(
        "harc_solve_queued(t, _harc_rt_call.problem_id, _harc_rt_seed, [&](auto* _harc_target) { randomize_Empty(_harc_target, _harc_site_rng); });"
    ));
    assert!(cpp.contains("_harc_site_rng.state = _harc_rt_seed;"));
    assert!(cpp.contains("harc_rt::random::harc_handle_solve_status(_harc_rt_status);"));
    assert!(!cpp.contains("randomize_Empty(&t);"));
    assert!(
        !cpp.contains("z3::context _ctx;"),
        "unconstrained fast path should not enter inline Z3; got:\n{cpp}"
    );
}

#[test]
fn waveform_trace_scaffolding_is_always_emitted_and_gated() {
    // Issue #209: every emitted TB must contain the trace
    // scaffolding (include + setup + per-cycle dump + teardown),
    // gated by `HARC_TRACE_VCD` / `HARC_TRACE_FST` so non-trace
    // builds compile it out. The codegen does NOT depend on a CLI
    // flag — `harc sim --waves` flips the gate by defining one of
    // the macros at Verilator compile time.
    let src = r#"
test WaveTest
    let dut : Top
    run
        wait 1 cycle
    end run
end test WaveTest
"#;
    let parsed = parse_source(src).unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");
    // Format-selecting header includes.
    assert!(
        cpp.contains("#if defined(HARC_TRACE_VCD)") && cpp.contains("\"verilated_vcd_c.h\""),
        "expected VCD trace header guard; got:\n{cpp}"
    );
    assert!(
        cpp.contains("#elif defined(HARC_TRACE_FST)") && cpp.contains("\"verilated_fst_c.h\""),
        "expected FST trace header guard; got:\n{cpp}"
    );
    assert!(cpp.contains("using HarcTraceC = VerilatedVcdC;"));
    assert!(cpp.contains("using HarcTraceC = VerilatedFstC;"));
    // Setup is gated by `#if HARC_TRACE_ENABLED`; dump/log/teardown call
    // runtime macros that compile away in a non-waves build.
    assert!(cpp.contains("Verilated::traceEverOn(true);"));
    assert!(cpp.contains("auto* tfp = ctx.tfp;"));
    assert!(cpp.contains("tfp = new HarcTraceC;"));
    assert!(cpp.contains(
        "harc_rt::log::harc_open_wave_trace(dut, tfp, harc_rt::log::harc_wave_default_name());"
    ));
    assert!(cpp.contains("HARC_RT_LOG_WAVE_FILE(log_ctx.sim_log, _wave_path);"));
    assert!(cpp.contains("HARC_RT_DUMP_WAVE_TRACE(tfp, t);"));
    assert!(cpp.contains("_harc_trace_dump_next(\"clk\", (uint64_t)(cycle_count + 1));"));
    assert!(cpp.contains("HARC_RT_WRITE_COVERAGE(ctx.verilated.coveragep());"));
    assert!(cpp.contains("HARC_RT_CLOSE_WAVE_TRACE(tfp);"));
}

#[test]
fn discard_binding_and_params_emit_cleanly() {
    let parsed = parse_source(
        r#"function consume(_: uint<8>, _: uint<8>)
    let _ = 1
end function consume

agent Sink
    in_ev : event<uint<8>>
    hookable ignore(_: uint<8>, _: uint<8>)
        let _ = 2
    end ignore
    on in_ev(_)
        let _ = 3
    end on
end agent Sink

test DiscardTest
    let dut : DummyDut
    run
        let s : Sink
        let _ = consume(1, 2)
        s.ignore(3, 4)
    end run
end test DiscardTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("discard forms should emit cleanly");
    assert!(
        cpp.contains("(void)(1);") && cpp.contains("(void)(2);") && cpp.contains("(void)(3);"),
        "expected discard lets to force evaluation with void casts; got:\n{cpp}"
    );
    assert!(
        cpp.contains("uint64_t _discard0, uint64_t _discard1"),
        "expected duplicate `_` params to synthesize unique C++ names; got:\n{cpp}"
    );
}

#[test]
fn typed_wide_integer_lets_keep_declared_cpp_width() {
    let parsed = parse_source(
        r#"test WideLocalTest
    let dut : DummyDut
    run
        let child_ptrs : uint<75> = 0x7FFFFFFFFFFFFFF9461
        let keys : uint<240> = 0x00000000000000000000000000000000000E000A00060002000C00040008
        dut.left_child = child_ptrs
        dut.keys = keys
    end run
end test WideLocalTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("_harc_u128 child_ptrs = (((_harc_u128)0x7FFULL << 64) | (_harc_u128)0xFFFFFFFFFFFF9461ULL);"),
        "uint<75> local should lower to _harc_u128, not int64_t; got:\n{cpp}",
    );
    assert!(
        cpp.contains("harc_rt::HarcWide<8> keys = harc_rt::HarcWide<8>({0x00040008u, 0x0002000Cu, 0x000A0006u, 0x0000000Eu"),
        "uint<240> local should lower to word-preserving HarcWide storage; got:\n{cpp}",
    );
    assert!(
        !cpp.contains("int64_t child_ptrs") && !cpp.contains("int64_t keys"),
        "typed wide locals must not use int64_t; got:\n{cpp}",
    );
}

// Tests for the legacy `impl sim for T` two-block form were removed
// alongside its parser entry in Phase 2 of docs/test-ergonomics.md.
// Inline-form coverage lives in the fixture suite (counter_test,
// rom_lut_inline_test, etc.) — those exercise the same lowering
// through the new single-block path.

#[test]
fn covergroup_auto_crosses_use_sample_local_bin_hits() {
    let parsed = parse_source(
        r#"covergroup G @(posedge dut.clk)
    cp_addr : cover dut.addr
        bins
            zero = {0}
            high = [8..15]
        end bins
    cp_data : cover dut.data
        bins
            small = [0..3]
            large = [12..15]
        end bins
end covergroup G

test CoverAutoCrossTest
    let dut : DummyDut
    run
        let cov : G
    end run
end test CoverAutoCrossTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("uint64_t _auto_cross_cp_addr__cp_data[2][2] = {};")
            && cpp.contains("bool _cg_hit_cp_addr[2] = {};")
            && cpp.contains("bool _cg_hit_cp_data[2] = {};")
            && cpp.contains("_cg_hit_cp_addr[0] = true;")
            && cpp.contains("_cg_hit_cp_data[1] = true;")
            && cpp.contains("if (_cg_hit_cp_addr[_i] && _cg_hit_cp_data[_j]) cov._auto_cross_cp_addr__cp_data[_i][_j]++;")
            && cpp.contains("harc_rt::log::harc_print_covergroup_summary(\"G\", _hit, _total);")
            && cpp.contains("harc_rt::log::harc_print_covergroup_bin(\"cp_addr\", \"zero\", cp_addr.zero);")
            && cpp.contains("harc_rt::log::harc_print_covergroup_cross_summary(\"G\", \"auto_cross\", \"cp_addr x cp_data\", _cross_hit, 4);")
            && cpp.contains("harc_rt::log::harc_print_covergroup_missing_bin(\"cp_addr.zero x cp_data.small\")")
            && cpp.contains("uint64_t _cross_missing = 0;")
            && cpp.contains("harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, 16, \"auto-cross\");"),
        "covergroup post-sim crosses should be updated from bins hit in the same sample record; got:\n{cpp}",
    );
}

#[test]
fn cover_statement_report_uses_runtime_helpers() {
    let src = r#"test CoverStmtTest
    let dut : Top
    run
        cover dut.done
    end run
end test CoverStmtTest"#;
    let parsed = parse_source(src).unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(cpp.contains("harc_rt::log::harc_print_cover_summary(_cov_hit, _cov_total);"));
    assert!(cpp.contains("harc_rt::log::harc_print_cover_point(\"cov_"));
}

#[test]
fn covergroup_can_sample_dut_bit_slice() {
    let parsed = parse_source(
        r#"covergroup G @(posedge dut.clk)
    cp_index : cover dut.cpu_addr[7:0]
        bins
            zero = {0}
            last = {255}
        end bins
end covergroup G

test CoverBitSliceTest
    let dut : DummyDut
    run
        let cov : G
    end run
end test CoverBitSliceTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("bit-sliced coverpoint should lower");
    assert!(
        cpp.contains(
            "harc_rt::harc_bits(harc_rt::harc_read(dut->cpu_addr), (uint32_t)(7), (uint32_t)(0))"
        ),
        "covergroup bit slice should lower through harc_bits; got:\n{cpp}",
    );
}

#[test]
fn covergroup_declared_crosses_lower_and_report() {
    let parsed = parse_source(
        r#"covergroup G @(posedge dut.clk)
    cp_addr : cover dut.addr
        bins
            zero = {0}
            high = [8..15]
        end bins
    cp_data : cover dut.data
        bins
            small = [0..3]
            large = [12..15]
        end bins
    cross cp_addr, cp_data
end covergroup G

test CoverDeclaredCrossTest
    let dut : DummyDut
    run
        let cov : G
    end run
end test CoverDeclaredCrossTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("uint64_t _cross_2_cp_addr__cp_data[4] = {};")
            && cpp.contains("if (_cg_hit_cp_addr[_i0] && _cg_hit_cp_data[_i1]) {")
            && cpp.contains("_cg_hit_cp_addr[0] = true;")
            && cpp.contains("_cg_hit_cp_data[1] = true;")
            && cpp.contains("cov._cross_2_cp_addr__cp_data[(_i0 * 2 + _i1)]++;")
            && cpp.contains("harc_rt::log::harc_print_covergroup_cross_summary(\"G\", \"cross\", \"cp_addr x cp_data\", _cross_hit, 4);")
            && !cpp.contains("harc_rt::log::harc_print_covergroup_cross_summary(\"G\", \"auto_cross\", \"cp_addr x cp_data\"")
            && cpp.contains("harc_rt::log::harc_print_covergroup_missing_bin(\"cp_addr.zero x cp_data.small\")")
            && cpp.contains("uint64_t _cross_missing = 0;")
            && cpp.contains("harc_rt::log::harc_print_covergroup_more_missing(_cross_missing, 16, \"cross\");"),
        "declared covergroup crosses should update and report sample-local bin combinations; got:\n{cpp}",
    );
}

#[test]
fn covergroup_declared_three_way_crosses_flatten_bins() {
    let parsed = parse_source(
        r#"covergroup G @(posedge dut.clk)
    cp_op : cover dut.op
        bins
            read = {0}
            write = {1}
        end bins
    cp_burst : cover dut.burst
        bins
            fixed = {0}
            incr = {1}
        end bins
    cp_len : cover dut.len
        bins
            single = {0}
            multi = [1..15]
        end bins
    cross cp_op, cp_burst, cp_len
end covergroup G

test CoverDeclaredThreeWayCrossTest
    let dut : DummyDut
    run
        let cov : G
    end run
end test CoverDeclaredThreeWayCrossTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("uint64_t _cross_3_cp_op__cp_burst__cp_len[8] = {};")
            && cpp.contains("cov._cross_3_cp_op__cp_burst__cp_len[((_i0 * 2 + _i1) * 2 + _i2)]++;")
            && cpp.contains("harc_rt::log::harc_print_covergroup_cross_summary(\"G\", \"cross\", \"cp_op x cp_burst x cp_len\", _cross_hit, 8);")
            && cpp.contains("harc_rt::log::harc_print_covergroup_missing_bin(\"cp_op.read x cp_burst.fixed x cp_len.single\")"),
        "declared three-way crosses should flatten and report all bin tuples; got:\n{cpp}",
    );
}

#[test]
fn covergroup_declared_crosses_validate_targets() {
    let parsed = parse_source(
        r#"covergroup G @(posedge dut.clk)
    cp_addr : cover dut.addr
        bins
            zero = {0}
        end bins
    cross cp_addr, cp_missing
end covergroup G

test CoverBadCrossTest
    let dut : DummyDut
    run
        let cov : G
    end run
end test CoverBadCrossTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0
            .contains("covergroup `G` cross references unknown coverpoint `cp_missing`"),
        "declared covergroup crosses should validate point names; got: {}",
        err.0
    );
}

#[test]
fn hook_triggered_covergroups_sample_at_hook_point() {
    let parsed = parse_source(
        r#"transaction Txn
    op : uint<8>
    len : uint<8>
end transaction Txn

agent Mon
    hookable observed(t: Txn)
    end observed
end agent Mon

covergroup TxnCov @(mon.observed(t) post)
    cp_op : cover t.op
        bins
            read = {0}
            write = {1}
        end bins
    cp_len : cover t.len
        bins
            short = [0..7]
            long = [8..15]
        end bins
end covergroup TxnCov

test HookCoverTriggerTest
    let dut : DummyDut
    run
        let mon : Mon
        let cov : TxnCov
    end run
end test HookCoverTriggerTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("Mon_observed_post.push_back([&](Txn t) {")
            && cpp.contains("uint64_t _v = (uint64_t)(t.op);")
            && cpp.contains("uint64_t _v = (uint64_t)(t.len);")
            && cpp.contains("bool _cg_hit_cp_op[2] = {};")
            && cpp.contains("if (_cg_hit_cp_op[_i] && _cg_hit_cp_len[_j]) cov._auto_cross_cp_op__cp_len[_i][_j]++;")
            && !cpp.contains("hook-triggered covergroup sampling is parsed but not lowered yet"),
        "hook-triggered covergroups should register a sample lambda on the hook vector; got:\n{cpp}",
    );
}

#[test]
fn hook_triggered_covergroups_validate_hook_args() {
    let parsed = parse_source(
        r#"transaction Txn
    op : uint<8>
end transaction Txn

agent Mon
    hookable observed(t: Txn)
    end observed
end agent Mon

covergroup TxnCov @(mon.observed(pkt) post)
    cp_op : cover pkt.op
        bins
            read = {0}
        end bins
end covergroup TxnCov

test HookCoverArgTest
    let dut : DummyDut
    run
        let mon : Mon
        let cov : TxnCov
    end run
end test HookCoverArgTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0
            .contains("hook trigger argument `pkt` must match hook parameter `t`"),
        "hook-triggered covergroups should validate trigger args against hook params; got: {}",
        err.0
    );
}

#[test]
fn hook_triggered_covergroups_resolve_nested_paths() {
    let parsed = parse_source(
        r#"transaction Txn
    op : uint<8>
end transaction Txn

agent Mon
    hookable observed(t: Txn)
    end observed
end agent Mon

env Env
    mon : Mon
end env Env

covergroup TxnCov @(env.mon.observed(t) post)
    cp_op : cover t.op
        bins
            read = {0}
            write = {1}
        end bins
end covergroup TxnCov

test NestedHookCoverTriggerTest
    let dut : DummyDut
    run
        let env : Env
        let cov : TxnCov
    end run
end test NestedHookCoverTriggerTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("Mon_observed_post.push_back([&](Txn t) {")
            && cpp.contains("uint64_t _v = (uint64_t)(t.op);"),
        "hook-triggered covergroups should resolve nested component paths; got:\n{cpp}",
    );
}

#[test]
fn hook_triggered_covergroups_reject_non_hookable_targets() {
    let parsed = parse_source(
        r#"transaction Txn
    op : uint<8>
end transaction Txn

agent Mon
    function observed(t: Txn)
    end function observed
end agent Mon

covergroup TxnCov @(mon.observed(t) post)
    cp_op : cover t.op
        bins
            read = {0}
        end bins
end covergroup TxnCov

test BadHookCoverTriggerTest
    let dut : DummyDut
    run
        let mon : Mon
        let cov : TxnCov
    end run
end test BadHookCoverTriggerTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0
            .contains("hook trigger must resolve to a `hookable` on a known component type"),
        "hook-triggered covergroups should reject non-hookable methods; got: {}",
        err.0
    );
}

/// `size` on addrmap instances triggers static overlap detection
/// (docs/ral-support.md §4). Two sized windows that share any byte
/// must fail codegen with a message naming both instances and
/// their address ranges. Aliased pairs are skipped (alias support
/// pending).
#[test]
fn addrmap_overlap_errors_clearly() {
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
end regblock R

addrmap M via H
    instance a : R @ 0x1000 size 0x200
    instance b : R @ 0x1100 size 0x100
end addrmap M

test T
    let dut : SomeDut
    run
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let err = cpp_tb::emit(&merged).unwrap_err();
    assert!(
        err.0.contains("addrmap `M`")
            && err.0.contains("instance `a`")
            && err.0.contains("instance `b`")
            && err.0.contains("overlaps"),
        "expected overlap error naming addrmap M + both instances a + b; got: {}",
        err.0,
    );
}

#[test]
fn addrmap_no_overlap_emits_cleanly() {
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
end regblock R

addrmap M via H
    instance a : R @ 0x1000 size 0x100
    instance b : R @ 0x1200 size 0x100
end addrmap M

test T
    let dut : SomeDut
    run
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    cpp_tb::emit(&merged).expect("non-overlapping sized instances should emit cleanly");
}

#[test]
fn addrmap_alias_to_missing_instance_errors() {
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
end regblock R

addrmap M via H
    instance a : R @ 0x1000
    instance b : R @ 0x2000 alias of nonexistent
end addrmap M

test T
    let dut : SomeDut
    run
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let err = cpp_tb::emit(&merged).unwrap_err();
    assert!(
        err.0.contains("`b`") && err.0.contains("aliases `nonexistent`"),
        "expected error naming the bad alias; got: {}",
        err.0,
    );
}

#[test]
fn addrmap_chained_alias_errors() {
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
end regblock R

addrmap M via H
    instance a : R @ 0x1000
    instance b : R @ 0x2000 alias of a
    instance c : R @ 0x3000 alias of b
end addrmap M

test T
    let dut : SomeDut
    run
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let err = cpp_tb::emit(&merged).unwrap_err();
    assert!(
        err.0.contains("chained aliases"),
        "expected chained-alias error; got: {}",
        err.0,
    );
}

#[test]
fn addrmap_alias_skips_overlap_check() {
    // Aliased pairs intentionally share storage at different bus
    // bases — the overlap check skips them.
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
end regblock R

addrmap M via H
    instance a : R @ 0x1000 size 0x200
    instance b : R @ 0x1100 size 0x100 alias of a
end addrmap M

test T
    let dut : SomeDut
    run
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    cpp_tb::emit(&merged).expect("aliased instances should bypass the overlap check");
}

#[test]
fn addrmap_size_optional_skips_overlap_check() {
    // Without `size`, the codegen can't bound the window and
    // skips the check. This matches the documented behavior in
    // docs/ral-support.md §4 (`size` is optional).
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
end regblock R

addrmap M via H
    instance a : R @ 0x1000
    instance b : R @ 0x1080
end addrmap M

test T
    let dut : SomeDut
    run
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    cpp_tb::emit(&merged).expect("instances without `size` should not trip the overlap check");
}

#[test]
fn multiple_tests_with_different_duts_errors_at_emit() {
    // Phase 1b: merge_for_sim now passes multiple tests through to
    // codegen (each emits its own `run_<TestName>` function). The
    // shared-DUT validation moved to emit_with_opts — it surfaces a
    // clear error when two tests pick different SV modules, since
    // Verilator can only build one V<top> per binary. The validation
    // names both tests + both DUT types.
    let f = parse_source(
        r#"test A
    let dut : X
    run end run
end test A
test B
    let dut : Y
    run end run
end test B
"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![f], None).expect("merge keeps both tests");
    let err = cpp_tb::emit(&merged).unwrap_err();
    assert!(
        err.0.contains("multi-DUT") && err.0.contains("`X`") && err.0.contains("`Y`"),
        "expected multi-DUT error naming X and Y; got: {}",
        err.0,
    );
}

#[test]
fn multiple_tests_with_same_dut_emit_all_run_functions() {
    let f = parse_source(
        r#"test A
    let dut : X
    run end run
end test A
test B
    let dut : X
    run end run
end test B
"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![f], None).expect("merge keeps both tests");
    let cpp = cpp_tb::emit(&merged).expect("same-DUT multi-test emits cleanly");
    assert!(
        cpp.contains("int run_A(int argc"),
        "expected run_A function"
    );
    assert!(
        cpp.contains("int run_B(int argc"),
        "expected run_B function"
    );
    assert!(
        cpp.contains("std::strcmp(test_sel, \"A\") == 0")
            && cpp.contains("std::strcmp(test_sel, \"B\") == 0"),
        "expected dispatcher branches for both A and B; got:\n{cpp}",
    );
    assert!(cpp.contains("const char* test_sel = harc_rt::log::harc_select_test(argc, argv);"));
    assert!(cpp.contains("harc_rt::log::harc_report_unknown_test(test_sel, \"A, B\");"));
}

#[test]
fn focused_codegen_filter_keeps_only_selected_test() {
    let f = parse_source(
        r#"test A
    let dut : X
    run end run
end test A
test B
    let dut : X
    run end run
end test B
"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![f], Some("B")).expect("merge validates selected test");
    let focused =
        merge::filter_tests_for_codegen(&merged, "B").expect("selected test should filter");
    let cpp = cpp_tb::emit(&focused).expect("focused test emits cleanly");

    assert!(
        !cpp.contains("int run_A(int argc"),
        "focused compile should not emit unselected run_A:\n{cpp}"
    );
    assert!(
        cpp.contains("int run_B(int argc"),
        "focused compile should emit selected run_B:\n{cpp}"
    );
    assert!(
        !cpp.contains("std::strcmp(test_sel, \"A\") == 0")
            && cpp.contains("std::strcmp(test_sel, \"B\") == 0"),
        "focused dispatcher should only name B:\n{cpp}"
    );
}

#[test]
fn split_tests_emit_dispatcher_and_one_unit_per_test() {
    let f = parse_source(
        r#"test A
    let dut : X
    run end run
end test A
test B
    let dut : X
    run end run
end test B
"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![f], None).expect("merge keeps both tests");
    let split = cpp_tb::emit_split_tests(&merged, cpp_tb::EmitOpts::default())
        .expect("split emit should succeed");

    assert_eq!(split.test_names, vec!["A".to_string(), "B".to_string()]);
    assert_eq!(
        split.files.len(),
        3,
        "expected dispatcher main.cpp + one self-contained .cpp per test"
    );

    let main = split
        .files
        .iter()
        .find(|f| f.filename == "main.cpp")
        .expect("dispatcher file missing");
    assert!(main
        .contents
        .contains("extern int run_A(int argc, char** argv);"));
    assert!(main
        .contents
        .contains("extern int run_B(int argc, char** argv);"));
    assert!(main.contents.contains("std::strcmp(test_sel, \"A\") == 0"));
    assert!(main.contents.contains("std::strcmp(test_sel, \"B\") == 0"));

    // Each shard is a self-contained translation unit — no factored
    // header/.inc files are emitted.
    for gf in &split.files {
        assert!(
            !gf.filename.ends_with(".hpp") && !gf.filename.ends_with(".inc"),
            "split output should not emit factored headers/snippets, got `{}`",
            gf.filename
        );
    }

    let test_a = split
        .files
        .iter()
        .find(|f| f.filename == "test_A.cpp")
        .expect("test_A.cpp missing");
    // Self-contained: the unit inlines the shared scaffolding rather than
    // including factored headers.
    assert!(
        !test_a.contents.contains("common_prefix.hpp")
            && !test_a.contents.contains("common_run_prefix.inc"),
        "test unit should not include factored headers:\n{}",
        test_a.contents
    );
    assert!(
        test_a.contents.contains("struct HarcTestContext"),
        "test unit should inline shared scaffolding:\n{}",
        test_a.contents
    );
    assert!(test_a.contents.contains("int run_A(int argc"));
    assert!(!test_a.contents.contains("int run_B(int argc"));
    assert!(
        !test_a.contents.contains("int main(int argc"),
        "per-test split unit should not define main:\n{}",
        test_a.contents
    );

    let prefixed = cpp_tb::emit_split_tests_with_file_prefix(
        &merged,
        cpp_tb::EmitOpts::default(),
        "suite__",
        1,
    )
    .expect("prefixed split emit should succeed");
    // The file prefix applies to the per-test unit names; still no factored
    // headers.
    assert!(!prefixed
        .files
        .iter()
        .any(|f| f.filename.ends_with(".hpp") || f.filename.ends_with(".inc")));
    let prefixed_test_a = prefixed
        .files
        .iter()
        .find(|f| f.filename == "suite__test_A.cpp")
        .expect("prefixed test_A.cpp missing");
    assert!(prefixed_test_a.contents.contains("int run_A(int argc"));
    assert!(!prefixed_test_a.contents.contains("int main(int argc"));

    let grouped = cpp_tb::emit_split_tests_with_file_prefix(
        &merged,
        cpp_tb::EmitOpts::default(),
        "suite__",
        2,
    )
    .expect("grouped split emit should succeed");
    assert!(grouped
        .files
        .iter()
        .any(|f| f.filename == "suite__shard1.cpp"));
    assert!(!grouped
        .files
        .iter()
        .any(|f| f.filename == "suite__test_A.cpp"));
    let shard1 = grouped
        .files
        .iter()
        .find(|f| f.filename == "suite__shard1.cpp")
        .expect("suite__shard1.cpp missing");
    assert!(shard1.contents.contains("int run_A(int argc"));
    assert!(shard1.contents.contains("int run_B(int argc"));
    assert!(!shard1.contents.contains("suite__common_suffix"));
}

#[test]
fn grouped_split_reemits_per_test_suffixes() {
    let f = parse_source(
        r#"transaction Req
    addr : uint<8>
end transaction Req

test A
    let dut : X
    run
        let r : Req
        randomize(r) with
            r.addr == 1
        end randomize
    end run
end test A

test B
    let dut : X
    run
        let r : Req
        randomize(r) with
            r.addr == 2
        end randomize
        randomize(r) with
            r.addr == 3
        end randomize
    end run
end test B
"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![f], None).expect("merge keeps both tests");
    let grouped = cpp_tb::emit_split_tests_with_file_prefix(
        &merged,
        cpp_tb::EmitOpts::default(),
        "suite__",
        2,
    )
    .expect("grouped split emit should succeed");
    let shard = grouped
        .files
        .iter()
        .find(|f| f.filename == "suite__shard1.cpp")
        .expect("suite__shard1.cpp missing");

    assert_eq!(
        grouped.files.len(),
        2,
        "grouped split should emit dispatcher plus one coherent shard"
    );
    assert!(shard.contents.contains("int run_A(int argc"));
    assert!(shard.contents.contains("int run_B(int argc"));
    assert!(shard.contents.contains("r.addr == 1"));
    assert!(shard.contents.contains("r.addr == 2"));
    assert!(shard.contents.contains("r.addr == 3"));
    assert!(!shard.contents.contains("suite__common_suffix"));
    assert!(shard
        .contents
        .contains("static inline harc_rt::random::HarcRandomizeCall harc_prepare_randomize_call("));
}

#[test]
fn missing_dut_let_is_a_clean_error() {
    let parsed = parse_source(
        r#"test T
    run
    end run
end test T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(err.0.contains("let dut"));
}

/// `${expr:WWx}` and `${expr:WWX}` format specs with WW > 16 route
/// through the `HarcHexBuf128` runtime helper (printf `%s`) so the
/// full ≤128-bit value prints. The current-default narrow path would
/// truncate to the lower 64 bits — fine for register dumps that fit in a
/// uint64, useless for AES blocks. Specs with width ≤ 16 stay on the
/// legacy `%llx` path and route arguments through `harc_printf_ll`.
#[test]
fn wide_hex_format_spec_routes_through_hexbuf128() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        // Wide-hex spec — width 32 hex digits = 128 bits.
        log(info, "ct=0x${dut.text_out:032x}")
        // Narrow-hex spec — width 8 hex digits = stays on long long.
        log(info, "narrow=0x${dut.x:08x}")
        // Uppercase wide spec.
        log(info, "CT=0x${dut.text_out:032X}")
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Wide-hex lowercase — printf `%s` + HarcHexBuf128 with upper=false.
    assert!(
        cpp.contains("\"ct=0x%s\""),
        "expected `%s` format token for wide-hex spec:\n{}",
        cpp
    );
    assert!(
        cpp.contains(
            "(const char*)harc_rt::HarcHexBuf128(harc_rt::harc_read(dut->text_out), 32, false)"
        ),
        "expected HarcHexBuf128 lowering for `:032x`:\n{}",
        cpp
    );

    // Wide-hex uppercase — same shape, upper=true.
    assert!(
        cpp.contains(
            "(const char*)harc_rt::HarcHexBuf128(harc_rt::harc_read(dut->text_out), 32, true)"
        ),
        "expected HarcHexBuf128 lowering for `:032X`:\n{}",
        cpp
    );

    // Narrow-hex stays on the legacy path.
    assert!(
        cpp.contains("\"narrow=0x%08llx\""),
        "expected `%08llx` for narrow `:08x` spec:\n{}",
        cpp
    );
    assert!(
        cpp.contains("harc_rt::harc_printf_ll(harc_rt::harc_read(dut->x))"),
        "expected narrow interpolation args to use harc_printf_ll:\n{}",
        cpp
    );
    assert!(
        !cpp.contains("HarcHexBuf128(harc_rt::harc_read(dut->x)")
            && !cpp.contains("HarcHexBuf128(dut->x"),
        "narrow-hex spec must NOT route through HarcHexBuf128:\n{}",
        cpp
    );
}

/// Hex literals wider than 128 bits (>32 hex digits) overflow
/// `_harc_u128` and route through the `harc_assign_words` /
/// `harc_eq_words` runtime helpers — taking an
/// `std::initializer_list<uint32_t>` of the literal split into
/// LSB-first 32-bit words. This is what makes wide DATA buses
/// (AXI 256/512/1024-bit, vector lanes, etc.) drivable as
/// whole-signal hex literals.
#[test]
fn wide_hex_literal_routes_assign_and_eq_through_word_helpers() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        // 256-bit literal — 64 hex digits — must split into 8
        // words and route through harc_assign_words for the write
        // and harc_eq_words for the compare.
        dut.data = 0x0123456789abcdef_fedcba9876543210_aabbccddeeff0011_2233445566778899
        assert dut.data == 0xffffffffffffffff_0000000000000000_aabbccddeeff0011_2233445566778899
            else fail("nope")
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Assignment: harc_assign_words_checked with 8 LSB-first words and a
    // <8> required-word-count template arg (highest non-zero word is at
    // index 7 — 0x01234567 — so the value needs all 8 words).
    assert!(cpp.contains("harc_rt::harc_assign_words_checked<8>(dut->data, {0x66778899u, 0x22334455u, 0xeeff0011u, 0xaabbccddu, 0x76543210u, 0xfedcba98u, 0x89abcdefu, 0x01234567u})"),
        "expected harc_assign_words_checked call with LSB-first words:\n{}", cpp);

    // Equality: harc_eq_words with 8 LSB-first words from the
    // compared literal.
    assert!(cpp.contains("harc_rt::harc_eq_words(dut->data, {0x66778899u, 0x22334455u, 0xeeff0011u, 0xaabbccddu, 0x00000000u, 0x00000000u, 0xffffffffu, 0xffffffffu})"),
        "expected harc_eq_words call with LSB-first words:\n{}", cpp);
}

/// Over-width literal guard (value-based).
///
/// Regression for the SHA-256 fixture bug: a hex literal written *wider*
/// than the target port — with set bits above the port's top bit — was
/// silently chunked into one too many 32-bit words. The high word was
/// then dropped at runtime by `harc_assign_words`, misaligning the
/// message; harc emitted no error.
///
/// The fix routes wide-literal assignments through
/// `harc_assign_words_checked<ReqWords>`, where `ReqWords` is the number
/// of 32-bit words the literal's *value* actually needs (one past its
/// highest non-zero word). The runtime helper `static_assert`s that
/// `ReqWords` fits the signal's physical word capacity, so an over-width
/// literal fails the C++ build with a named diagnostic instead of
/// corrupting data silently.
///
/// This test pins the value-based word count the codegen emits. Here the
/// 17-word "abc" SHA-256 block (highest non-zero word — the 0x18 length
/// field — sits at index 16) yields `<17>`, which a 16-word `[15:0][31:0]`
/// `msg` port then rejects via the `static_assert`.
#[test]
fn over_width_wide_literal_emits_value_based_word_count_guard() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        // 544-bit literal (136 hex digits → 17 words). The meaningful
        // 0x18 length word lands in word 16, one past a 512-bit port.
        dut.msg = 0x0000001800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000061626380
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // The required-word-count template arg counts the value's significant
    // words: highest non-zero word is index 16 (0x18) → 17.
    assert!(
        cpp.contains("harc_rt::harc_assign_words_checked<17>(dut->msg,"),
        "expected over-width literal to emit a <17>-word guard:\n{}",
        cpp
    );
}

/// Companion to `over_width_wide_literal_emits_value_based_word_count_guard`:
/// a literal whose value FITS the port but is written with extra leading
/// zero words must NOT inflate the required-word count — leading-zero high
/// words don't count. This is the false-positive guard: the "empty"
/// SHA-256 block (only 0x80000000 in word 0, the rest zero) needs just one
/// word and must lower to `<1>`, even though it occupies a 16-word port.
#[test]
fn fitting_wide_literal_does_not_overcount_leading_zero_words() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        // 512-bit literal whose only set bits are in word 0 (0x80000000).
        dut.msg = 0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080000000
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Value needs a single word, so the guard is <1> — well within any
    // wide port; no static_assert fires.
    assert!(
        cpp.contains("harc_rt::harc_assign_words_checked<1>(dut->msg,"),
        "expected leading-zero-only literal to emit a <1>-word guard:\n{}",
        cpp
    );
}

/// Hex literals wider than 64 bits (>16 hex digits) lower to a
/// composite `_harc_u128` shifted-OR expression so they fit C++'s
/// integer-literal grammar and flow through `harc_assign` /
/// `harc_read` at full 128-bit precision. Mirrors arch-com's
/// `_arch_u128` model (arch-com src/sim_codegen/mod.rs:767).
#[test]
fn wide_hex_literal_lowers_to_harc_u128_composite() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        dut.x = 0x000102030405060708090a0b0c0d0e0f
        assert dut.y == 0x66e94bd4ef8a2c3b884cfa59ca342b2e
            else fail("nope")
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // The 128-bit literal `0x000102030405060708090a0b0c0d0e0f` should
    // emit as `((_harc_u128)0x0001020304050607ULL << 64) |
    //         (_harc_u128)0x08090a0b0c0d0e0fULL`.
    assert!(
        cpp.contains("(_harc_u128)0x0001020304050607ULL << 64")
            && cpp.contains("(_harc_u128)0x08090a0b0c0d0e0fULL"),
        "expected composite _harc_u128 lowering for the assigned literal:\n{}",
        cpp,
    );
    assert!(
        cpp.contains("(_harc_u128)0x66e94bd4ef8a2c3bULL << 64")
            && cpp.contains("(_harc_u128)0x884cfa59ca342b2eULL"),
        "expected composite _harc_u128 lowering for the compared literal:\n{}",
        cpp,
    );

    // Narrow literals (<= 16 hex digits) stay as plain hex —
    // no composite, no _harc_u128 cast.
    assert!(
        !cpp.contains("(_harc_u128)0xDEADBEEF") && !cpp.contains("(_harc_u128)0xdeadbeef"),
        "narrow hex shouldn't be wrapped:\n{}",
        cpp
    );
}

/// `wait N cycles` matches Verilog's `@(posedge clk)` semantic: values
/// set in the segment BEFORE the wait are sampled at the next posedge.
/// To honor this — including for the FIRST segment (set during
/// `bootstrap()` before the loop) — the emitted main loop must:
///
/// 1. Do an initial `dut->eval()` with `clk=0` before the loop, so
///    bootstrap's combinational outputs settle without advancing time.
/// 2. Per loop iteration, do the posedge FIRST (clk 0→1, eval), then
///    `sched.tick()` (advance run coroutine for next cycle's inputs),
///    then the falling edge (clk 1→0, eval) for comb resettle.
///
/// Otherwise — if `tick()` happened first as it did pre-fix — the first
/// iteration's tick would decrement the bootstrap slot's WaitCycles to
/// 0 and run the next segment immediately, overwriting the bootstrap
/// segment's outputs before any posedge could sample them.
#[test]
fn main_loop_settles_comb_before_first_posedge_then_posedge_before_tick() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        dut.x = 1
        wait 1 cycle
        dut.x = 2
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Initial comb settle: `_harc_eval_negedge(dut)` BEFORE the loop
    // opens. There must be NO posedge call between bootstrap and the loop.
    let bootstrap_pos = cpp
        .find("sched.bootstrap()")
        .expect("expected sched.bootstrap() call");
    let loop_pos = cpp
        .find("while (_run_slot.kind != harc_rt::WaitKind::Done")
        .expect("expected main run loop");
    assert!(bootstrap_pos < loop_pos);
    let between = &cpp[bootstrap_pos..loop_pos];
    assert!(
        between.contains("_harc_eval_negedge(dut);"),
        "expected initial `_harc_eval_negedge(dut);` between bootstrap and loop:\n{}",
        between
    );
    assert!(
        !between.contains("_harc_eval_posedge(dut);"),
        "no posedge should appear between bootstrap and loop:\n{}",
        between
    );

    // Inside the loop body, the order must be: posedge (clk=1 guarded eval)
    // FIRST, then sched.tick(), then negedge (clk=0 guarded eval).
    let loop_body_end = cpp[loop_pos..]
        .find("\n    }\n")
        .map(|p| loop_pos + p)
        .expect("expected loop close");
    let body = &cpp[loop_pos..loop_body_end];
    let posedge_pos = body
        .find("_harc_eval_posedge(dut);")
        .expect("expected posedge inside loop");
    let tick_pos = body
        .find("sched.tick();")
        .expect("expected sched.tick() inside loop");
    let falling_pos = body
        .find("_harc_eval_negedge(dut);")
        .expect("expected falling edge inside loop");
    assert!(
        posedge_pos < tick_pos && tick_pos < falling_pos,
        "expected loop order: posedge → tick → falling. \
         got posedge@{posedge_pos}, tick@{tick_pos}, falling@{falling_pos}\n\
         body:\n{}",
        body
    );
}

/// `dut.<signal> = <expr>` and `dut.<signal>` accesses lower through
/// `harc_rt::harc_assign(...)` and `harc_rt::harc_read(...)` so wide
/// signals (Verilator's `VlWide<N>` for >64-bit ports) work without
/// the test author having to think about word-level decomposition.
/// Narrow signals see the same wrapper, which `if constexpr`-folds
/// to a plain assignment / cast.
#[test]
fn pointer_rooted_signal_access_uses_wide_helpers() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        dut.wide_in = 305419896
        dut.narrow_in = 5
        assert dut.wide_out == 305419896
            else fail("wide read")
        let v = dut.wide_out + 1
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Writes lower as harc_assign(...).
    assert!(
        cpp.contains("harc_rt::harc_assign(dut->wide_in,"),
        "expected `harc_rt::harc_assign(dut->wide_in, ...)` in:\n{}",
        cpp
    );
    assert!(
        cpp.contains("harc_rt::harc_assign(dut->narrow_in,"),
        "expected `harc_rt::harc_assign(dut->narrow_in, ...)` in:\n{}",
        cpp
    );

    // Reads lower as harc_read(...).
    assert!(
        cpp.contains("harc_rt::harc_read(dut->wide_out)"),
        "expected `harc_rt::harc_read(dut->wide_out)` in:\n{}",
        cpp
    );

    // L-value path must NOT wrap with harc_read — the assignment
    // target stays a plain L-value reference passed to harc_assign.
    // Spot-check: the assignment line should contain the field as
    // an L-value, not `harc_read(dut->wide_in)`.
    let assign_line = cpp
        .lines()
        .find(|l| l.contains("harc_assign(dut->wide_in,"))
        .expect("expected assign line");
    assert!(
        !assign_line.contains("harc_read(dut->wide_in"),
        "L-value position must not be wrapped with harc_read:\n{}",
        assign_line
    );
}

/// `const NAME : Ty = expr` lowers to a file-scope `static constexpr`
/// so it's available inside `main()`, hookable lambdas, tseq lambdas,
/// and on-handler closures.
#[test]
fn top_level_const_lowers_to_static_constexpr() {
    let parsed = parse_source(
        r#"const MSHR_SIZE : uint<32> = 32
const HALF      : uint<32> = MSHR_SIZE / 2
test T
    let dut : DummyDut
    run
        assert MSHR_SIZE == 32
            else fail("MSHR_SIZE wrong")
        assert HALF == 16
            else fail("HALF wrong")
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Both consts are emitted under collision-proof C++ names at file scope,
    // before main().
    assert!(
        cpp.contains("static constexpr uint64_t harc_user_const_MSHR_SIZE_")
            && cpp.contains(" = 32;"),
        "expected a mangled file-scope MSHR_SIZE definition in:\n{}",
        cpp
    );
    assert!(
        cpp.contains("static constexpr uint64_t harc_user_const_HALF_"),
        "expected a mangled file-scope HALF definition in:\n{}",
        cpp
    );

    // Order matters — both should appear BEFORE `int main`.
    let main_pos = cpp.find("int main").expect("expected `int main` in output");
    let mshr_pos = cpp
        .find("static constexpr uint64_t harc_user_const_MSHR_SIZE_")
        .unwrap();
    let half_pos = cpp
        .find("static constexpr uint64_t harc_user_const_HALF_")
        .unwrap();
    assert!(
        mshr_pos < main_pos,
        "MSHR_SIZE should be emitted before main()"
    );
    assert!(half_pos < main_pos, "HALF should be emitted before main()");
}

/// Spec §7.7: `log(error, ...)` increments the failure counter, and
/// `log(fatal, ...)` additionally sets a flag so the main simulation
/// loop aborts at end of the current cycle. `info` / `warn` / `debug`
/// have no test-result effect.
#[test]
fn log_severity_test_result_semantics() {
    // We need a `let dut : SomeModule` to satisfy the emit prelude;
    // the actual lowering doesn't depend on it.
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        log(info,  "info: no effect")
        log(warn,  "warn: no effect")
        log(debug, "debug: no effect")
        logf("detail.log", info, "detail: no effect")
        log(error, "error: should bump counter")
        log(fatal, "fatal: should abort")
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Sanity: the test-result flag is declared.
    assert!(
        cpp.contains("bool _fatal = false;"),
        "expected `_fatal` flag declaration in main()"
    );

    // The main simulation loop guard checks _fatal so the test instance
    // exits at end of current cycle when fatal is set.
    assert!(
        cpp.contains("&& !_fatal"),
        "expected main loop to check `!_fatal`"
    );

    // `log(info|warn|debug, ...)` must not bump errors. We grep for
    // `sim_log_line(\"INFO\"`, ... and verify the line that follows is
    // NOT `errors++`. Easier: count `errors++;` occurrences and confirm
    // exactly the right number.
    //
    // Each line starts a printf-call. After the closing `);`, the next
    // statement is either nothing (info/warn/debug) or `errors++;`
    // (error) or `errors++; _fatal = true;` (fatal).
    let errors_inc_count = cpp.matches("errors++;").count();
    // From-source: 2 (one for log(error), one for log(fatal)).
    // Plus existing `errors++;` from assert/fail paths: 0 here (no
    // asserts in the fixture).
    // Plus 1 from the prologue's `HarcQueueFatalScope` reporter, which
    // routes an empty-queue pop through the same FATAL path (#644). It
    // is scaffolding, not a lowering of any statement in the fixture —
    // the point of this count is still that info/warn/debug lower to no
    // bump at all, and a regression there moves the total either way.
    assert_eq!(
        errors_inc_count, 3,
        "expected exactly 3 `errors++;` lines (ERROR, FATAL, and the queue-pop \
         reporter in the prologue); got {} in:\n{}",
        errors_inc_count, cpp
    );

    // `log(fatal, ...)` additionally sets `_fatal = true`.
    assert!(
        cpp.contains("_fatal = true;"),
        "expected `_fatal = true;` in FATAL lowering"
    );
    assert!(
        cpp.contains(
            "sim_logf_line(log_ctx.file(\"detail.log\"), \"INFO\", \"detail: no effect\");"
        ),
        "expected logf to resolve files through HarcLogContext directly; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("get_log_file"),
        "generated get_log_file forwarding lambda should not be emitted; got:\n{cpp}"
    );
}

/// Custom `phase <name> ... end phase <name>` blocks inside an
/// `impl sim for X` lower as `[&]`-capturing void-returning lambdas
/// at main() scope, callable by name from `run` (or from each other).
/// Spec §7.2: phases are pure code-organization helpers — not
/// auto-fired by the runtime, only invoked by explicit user calls.
#[test]
fn impl_sim_custom_phase_lowers_as_named_lambda() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    phase warmup
        log(info, "warmup phase")
    end phase warmup

    run
        warmup()
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Phase emits as `auto warmup = [&]() -> void { ... };`
    assert!(
        cpp.contains("auto warmup = [&]() -> void {"),
        "expected `auto warmup = [&]() -> void {{` in:\n{cpp}"
    );
    // Body of the phase contains the log line.
    assert!(
        cpp.contains("warmup phase"),
        "phase body should contain its log message; got:\n{cpp}"
    );
    // Run-coroutine body invokes the phase by name.
    assert!(
        cpp.contains("warmup();"),
        "run body should call `warmup();` to invoke the phase; got:\n{cpp}"
    );

    // Phase lambda emits BEFORE the run-coroutine bootstrap so the
    // capture-by-reference closure is in scope when run calls it.
    let phase_pos = cpp.find("auto warmup = [&]()").unwrap();
    let bootstrap_pos = cpp.find("sched.bootstrap()").unwrap();
    assert!(
        phase_pos < bootstrap_pos,
        "custom phase lambda must be emitted before sched.bootstrap()"
    );
}

#[test]
fn bound_test_inherits_testbench_check_without_local_check() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    check
        log(info, "tb final check")
    end check
end testbench Tb

impl Smoke for Tb
    run
        log(info, "test run")
    end run
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("tb final check"),
        "testbench check should be emitted:\n{cpp}"
    );
    let run_pos = cpp.find("test run").unwrap();
    let check_pos = cpp.find("tb final check").unwrap();
    assert!(
        run_pos < check_pos,
        "testbench check must run after test run:\n{cpp}"
    );
}

#[test]
fn testbench_and_test_lifecycle_blocks_emit_in_order() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    setup
        log(info, "tb setup")
    end setup
    check
        log(info, "tb check")
    end check
    teardown
        log(info, "tb teardown")
    end teardown
end testbench Tb

impl Smoke for Tb
    setup
        log(info, "test setup")
    end setup
    run
        log(info, "test run")
    end run
    check
        log(info, "test check")
    end check
    teardown
        log(info, "test teardown")
    end teardown
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    let ordered = [
        "tb setup",
        "test setup",
        "test run",
        "tb check",
        "test check",
        "test teardown",
        "tb teardown",
    ];
    let mut last = 0usize;
    for needle in ordered {
        let pos = cpp
            .find(needle)
            .unwrap_or_else(|| panic!("missing `{needle}` in:\n{cpp}"));
        assert!(pos >= last, "`{needle}` emitted out of order in:\n{cpp}");
        last = pos;
    }
}

#[test]
fn testbench_lifecycle_wraps_bare_statement_impl_run() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    check
        log(info, "tb check")
    end check
end testbench Tb

impl Smoke for Tb
    log(info, "bare run")
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    let run_pos = cpp.find("bare run").unwrap();
    let check_pos = cpp.find("tb check").unwrap();
    assert!(
        run_pos < check_pos,
        "testbench check must run after bare-statement run body:\n{cpp}"
    );
}

/// Code-review finding A on PR arch-hdl-lang/harc-com#306: the
/// `_tb.dut = dut` wire-up MUST precede the first read of
/// `_tb.dut.*` in ANY lifecycle block. Earlier shape only injected
/// the wire when at least one `setup` block existed, otherwise put
/// it at the start of `run` — but `tb.check` and `tb.teardown` can
/// also dereference `_tb.dut.*`, and run hasn't executed when check
/// reads at end-of-test in the "no setup anywhere" case. This test
/// nails down the load-bearing invariant: a tb-only `check` that
/// reads `dut.*` must still see a wired `_tb.dut` even when neither
/// testbench nor impl declared a setup.
#[test]
fn dut_wire_precedes_lifecycle_reads_with_no_user_setup() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    check
        assert dut.done == 1 else fail("dut.done in tb.check")
    end check
end testbench Tb

impl Smoke for Tb
    run
        wait 1 cycle
    end run
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    let wire_pos = cpp
        .find("_tb.dut = dut")
        .unwrap_or_else(|| panic!("missing `_tb.dut = dut` wire-up; got:\n{cpp}"));
    // The `tb.check` block reads `_tb.dut->done` (bare `dut.done`
    // rewritten to point at the testbench's `_tb.dut` view, since
    // `check` lives on the testbench field-rewrite side, not the
    // synthesized test-scope `let dut` side).
    let read_pos = cpp
        .find("dut->done")
        .unwrap_or_else(|| panic!("missing `dut->done` read in lowered check; got:\n{cpp}"));
    assert!(
        wire_pos < read_pos,
        "`_tb.dut = dut` wire-up (at {wire_pos}) must precede first \
         `dut->done` read (at {read_pos}); got:\n{cpp}",
    );
}

/// Locks the **source-order independence** of testbench lifecycle
/// blocks (`setup` / `check` / `teardown`). Per the §7 cleanup
/// (arch-com#463), each phase keyword produces a typed
/// `ComponentItem::Lifecycle(LifecyclePhase, Block)` node, and the
/// codegen aggregator routes by phase tag — never by source position.
/// So declaring `check` source-before `setup` must produce the same
/// emitted lifecycle ordering as the canonical setup → check →
/// teardown order.
///
/// Without this test, a future refactor that started consuming
/// source order (e.g. an iterator that took the first Lifecycle
/// node it saw as authoritative) would slip through silently.
#[test]
fn testbench_lifecycle_phases_are_source_order_independent() {
    // Canonical order: setup, check, teardown.
    let canonical = parse_source(
        r#"testbench Tb
    dut : DummyDut
    setup
        log(info, "set up")
    end setup
    check
        assert dut.done == 1 else fail("dut.done in tb.check")
    end check
    teardown
        log(info, "torn down")
    end teardown
end testbench Tb

impl Smoke for Tb
    run
        wait 1 cycle
    end run
end impl Smoke"#,
    )
    .expect("canonical-order parse");

    // Reversed-source order: teardown, check, setup. Same three
    // phase bodies, same content — only the declaration order
    // differs in the .harc source.
    let reversed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    teardown
        log(info, "torn down")
    end teardown
    check
        assert dut.done == 1 else fail("dut.done in tb.check")
    end check
    setup
        log(info, "set up")
    end setup
end testbench Tb

impl Smoke for Tb
    run
        wait 1 cycle
    end run
end impl Smoke"#,
    )
    .expect("reversed-order parse");

    let canonical_cpp = cpp_tb::emit(&canonical).expect("emit canonical");
    let reversed_cpp = cpp_tb::emit(&reversed).expect("emit reversed");

    // The emit-time phase order is determined by the tb_lifecycle
    // aggregator in cpp_tb.rs, which writes phase fields by tag —
    // never by source position. So:
    //   1. The canonical fixture emits setup → check → teardown.
    //   2. The reversed fixture (source order: teardown → check →
    //      setup) ALSO emits setup → check → teardown.
    // The property is proven by the two having identical relative
    // emit-order despite different declaration orders.
    let phase_order = |cpp: &str, label: &str| -> (usize, usize, usize) {
        let setup_pos = cpp
            .find("\"set up\"")
            .unwrap_or_else(|| panic!("{label}: setup body missing in:\n{cpp}"));
        let check_pos = cpp
            .find("dut.done in tb.check")
            .unwrap_or_else(|| panic!("{label}: check body missing in:\n{cpp}"));
        let teardown_pos = cpp
            .find("\"torn down\"")
            .unwrap_or_else(|| panic!("{label}: teardown body missing in:\n{cpp}"));
        (setup_pos, check_pos, teardown_pos)
    };

    let (c_setup, c_check, c_teardown) = phase_order(&canonical_cpp, "canonical");
    let (r_setup, r_check, r_teardown) = phase_order(&reversed_cpp, "reversed");

    assert!(
        c_setup < c_check && c_check < c_teardown,
        "canonical fixture: emit-order should be setup → check → \
         teardown. Got byte positions setup={c_setup} check={c_check} \
         teardown={c_teardown}.\n{canonical_cpp}"
    );
    assert!(
        r_setup < r_check && r_check < r_teardown,
        "reversed fixture: emit-order MUST still be setup → check → \
         teardown despite the source order being reversed. If this \
         fails, the lifecycle aggregator regressed to consuming \
         source position. Got byte positions setup={r_setup} \
         check={r_check} teardown={r_teardown}.\n{reversed_cpp}"
    );
}

/// Code-review finding B on PR arch-hdl-lang/harc-com#306: the
/// bare-statement form of `impl ... for Tb` (no `setup`/`run`/
/// `check`/`teardown` scopes) MUST go through the same bare-name
/// rewrite as the scoped form. A bare body that references a
/// testbench field by bare name must be rewritten to `_tb.<name>`,
/// just like the scoped form does. This test pins the equivalence.
#[test]
fn bare_statement_impl_rewrites_testbench_field_references() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    expected : uint<32> default 7
end testbench Tb

impl Smoke for Tb
    assert expected == 7 else fail("expected not rewritten")
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // The bare `expected` identifier must be rewritten to
    // `_tb.expected`, mirroring how the scoped form rewrites
    // testbench fields inside lifecycle bodies.
    assert!(
        cpp.contains("_tb.expected"),
        "bare-stmt body must rewrite `expected` to `_tb.expected`; got:\n{cpp}",
    );
}

/// Code-review finding B follow-on: bare-statement impl bodies
/// compose with `tb.teardown` in the same emit order as the scoped
/// form (the scoped form's ordering is covered by
/// `testbench_and_test_lifecycle_blocks_emit_in_order`). The bare
/// run stmts must execute before `tb.teardown`.
#[test]
fn bare_statement_impl_runs_before_tb_teardown() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut
    teardown
        log(info, "tb teardown after bare")
    end teardown
end testbench Tb

impl Smoke for Tb
    log(info, "bare run body")
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    let run_pos = cpp
        .find("bare run body")
        .unwrap_or_else(|| panic!("missing bare run body in:\n{cpp}"));
    let td_pos = cpp
        .find("tb teardown after bare")
        .unwrap_or_else(|| panic!("missing tb teardown in:\n{cpp}"));
    assert!(
        run_pos < td_pos,
        "bare-stmt run must execute before tb.teardown; got:\n{cpp}",
    );
}

// The legacy `impl <target> for <Test>` form was removed in Phase 2
// of docs/test-ergonomics.md, so an emu-only impl is no longer
// expressible at parse time. Backend selection moves to CLI
// subcommands (`harc sim` / future `harc emu`); see RFC §5.

/// `expr as Type` (postfix cast, same shape as arch-com's grammar)
/// emits a C++ cast `((<c_type>)(<inner>))` when the target is a
/// builtin numeric type. Critical for width-widening cases like
/// `1 as uint<32> << 31` — without the cast, C++'s `int` literal
/// shift-by-31 hits sign-bit UB; with the cast, the shift operates
/// on `uint64_t` and is well-defined.
#[test]
fn cast_to_builtin_emits_cpp_cast() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        // Walk-1 pattern that needs cast-widened source: `(1 as
        // uint<32>) << 31` would otherwise be `1 << 31` against a
        // 32-bit int literal — UB in C++.
        let mask : uint<32> = (1 as uint<32>) << 31
        dut.X = mask
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // Cast emits as `((uint64_t)(1))` — HARC's c_type_for maps
    // uint<32> to uint64_t (the C++ widening covers all ≤64-bit
    // unsigned ints uniformly).
    assert!(
        cpp.contains("((uint64_t)(1))"),
        "expected `((uint64_t)(1))` from `1 as uint<32>`; got:\n{cpp}"
    );
    // And the shift uses that cast result, not a bare `1 << 31`.
    assert!(
        cpp.contains("((uint64_t)(1))) << 31") || cpp.contains("((uint64_t)(1)) << 31"),
        "expected shift to operate on the cast result; got:\n{cpp}"
    );
}

/// Standalone `fail("...")` lowers to the same emission as the
/// failure arm of an `assert ... else fail(...)`: a `sim_log_line`
/// + `errors++;`. Without the surrounding `if (!cond)` guard, it
/// is an unconditional failure — useful when the failure trigger
/// is control-flow-structural rather than a single boolean
/// predicate.
#[test]
fn standalone_fail_emits_sim_log_and_errors_bump() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        for i in 0 .. 4
            if i == 3
                fail("loop reached unreachable branch at i=${i}")
            end if
        end for
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // The standalone fail emits a sim_log_line + errors++ inline,
    // no surrounding `if (!...)` guard.
    assert!(
        cpp.contains("sim_log_line(\"FAIL\", \"loop reached unreachable branch at i=%lld\""),
        "expected sim_log_line(\"FAIL\", ...) for standalone fail; got:\n{cpp}"
    );
    assert!(
        cpp.contains("errors++"),
        "expected `errors++;` after standalone fail; got:\n{cpp}"
    );
}

/// Every component struct (transactor/agent/env/scoreboard/sequencer)
/// gets two auto-injected heartbeat fields — `_last_in_cycle` and
/// `_last_out_cycle` — used by the built-in `idle(N)` / `idle_in(N)` /
/// `idle_out(N)` predicates. These fields default to 0 and are bumped
/// at every site the framework knows an in/out has just happened:
/// `on <event>` handler body entry, `emit ev(arg)`, `bus.<ch>.send`,
/// `bus.<ch>.recv`. This pins the lowering shape (spec §7.x).
#[test]
fn component_heartbeat_fields_and_bump_sites() {
    let parsed = parse_source(
        r#"transaction T
    addr  : uint<8>
    value : uint<32>
end transaction T

agent Producer
    out : event<T>
    in_ev : event<T>

    on in_ev(t)
        emit out(t)
    end on
end agent Producer

test HeartbeatTest
    let dut : DummyDut
    let prod : Producer
    run
        let stuck = prod.idle(50)
        let stuck_in = prod.idle_in(10)
        let stuck_out = prod.idle_out(20)
        wait 1 cycle
    end run
end test HeartbeatTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // 1. Heartbeat fields appear on the component struct.
    assert!(
        cpp.contains("struct Producer {"),
        "Producer struct should be emitted; got:\n{cpp}"
    );
    assert!(
        cpp.contains("uint64_t _last_in_cycle = 0;"),
        "expected `_last_in_cycle` field on component; got:\n{cpp}"
    );
    assert!(
        cpp.contains("uint64_t _last_out_cycle = 0;"),
        "expected `_last_out_cycle` field on component; got:\n{cpp}"
    );

    // 2. The `on in_ev(t)` handler body bumps `_last_in_cycle` at entry.
    assert!(
        cpp.contains("prod._last_in_cycle = (uint64_t)cycle_count;"),
        "on-handler body should bump _last_in_cycle on entry; got:\n{cpp}"
    );

    // 3. The `emit out(t)` inside the handler bumps `_last_out_cycle`.
    assert!(
        cpp.contains("prod._last_out_cycle = (uint64_t)cycle_count;"),
        "emit inside component body should bump _last_out_cycle; got:\n{cpp}"
    );

    // 4. `prod.idle(N)` lowers to a conjunction over both cycle deltas.
    assert!(
        cpp.contains("((uint64_t)cycle_count - prod._last_in_cycle) >= (uint64_t)(50))")
            && cpp.contains("((uint64_t)cycle_count - prod._last_out_cycle) >= (uint64_t)(50))"),
        "idle(N) should lower to (in_delta >= N) && (out_delta >= N); got:\n{cpp}",
    );
    // 5. `prod.idle_in(N)` lowers to in-delta only.
    assert!(
        cpp.contains("((uint64_t)cycle_count - prod._last_in_cycle) >= (uint64_t)(10))"),
        "idle_in(N) should lower to (in_delta >= N); got:\n{cpp}"
    );
    // 6. `prod.idle_out(N)` lowers to out-delta only.
    assert!(
        cpp.contains("((uint64_t)cycle_count - prod._last_out_cycle) >= (uint64_t)(20))"),
        "idle_out(N) should lower to (out_delta >= N); got:\n{cpp}"
    );
}

/// `bus.<ch>.send` and `bus.<ch>.recv` inside a component body bump
/// `_last_out_cycle` / `_last_in_cycle` respectively. Bus calls in
/// free test-run code don't attribute to any component instance and
/// emit no bump.
#[test]
fn bus_send_recv_bump_component_heartbeat() {
    // Setup mirrors `axilite_seqdrv_test.harc` — a bound active
    // transactor whose on-handler uses bus.send. The handshake spin
    // loop ends with a bump to `_last_out_cycle` on the driver
    // instance.
    let parsed = parse_source(
        r#"transaction RegOp
    addr  : uint<8>
    value : uint<32>
end transaction RegOp

bus BusLite
    handshake_channel w: send kind: valid_ready
        addr : uint<8>
        data : uint<32>
    end handshake_channel w
end bus BusLite

transactor SeqXactor bound to BusLite
    dut : DummyDut

    when active
        req : in event<RegOp>
        on req(t)
            bus.w.send(t.addr, t.value)
        end on
    end when
end transactor SeqXactor

test BusHeartbeatTest
    let dut : DummyDut
    let axil : BusLite = bind dut
    let drv : SeqXactor active = bind axil
    run
        wait 1 cycle
    end run
end test BusHeartbeatTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Bound-driver actor pops a transaction → bumps _last_in_cycle.
    assert!(
        cpp.contains("drv._last_in_cycle = (uint64_t)cycle_count;"),
        "bound-driver actor pop should bump _last_in_cycle; got:\n{cpp}"
    );
    // bus.w.send inside the actor body → bumps _last_out_cycle.
    assert!(
        cpp.contains("drv._last_out_cycle = (uint64_t)cycle_count;"),
        "bus.<ch>.send inside component body should bump _last_out_cycle; got:\n{cpp}"
    );
}

/// A user-defined `hookable idle(n)` on a component or transactor
/// wins over the built-in `idle(N)` predicate. The call lowers to
/// `<Type>_idle(obj, n)` as a regular hookable dispatch, NOT to the
/// boolean heartbeat-delta predicate. This is what lets pre-existing
/// fixtures keep their custom `idle()` semantics (e.g.
/// `buf_mgr_test.harc`'s `hookable idle(n)` that holds bus valids
/// low for `n` cycles).
#[test]
fn user_hookable_idle_wins_over_builtin_predicate() {
    let parsed = parse_source(
        r#"transactor Xact
    dut : DummyDut

    hookable idle(n: uint<32>)
        for _ in 0 .. n
            wait 1 cycle
        end for
    end idle
end transactor Xact

test UserIdleTest
    let dut : DummyDut
    let xact : Xact passive
    run
        xact.idle(4)
    end run
end test UserIdleTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // The call should dispatch to the user's hookable, NOT to the
    // built-in heartbeat predicate.
    assert!(
        cpp.contains("Xact_idle(xact, 4)"),
        "user `hookable idle(n)` should be called via the standard dispatcher; got:\n{cpp}"
    );
    // Specifically, the call's lowering should NOT contain the
    // built-in delta-comparison shape.
    assert!(
        !cpp.contains("cycle_count - xact._last_in_cycle"),
        "built-in predicate should NOT shadow user's `hookable idle`; got:\n{cpp}"
    );
}

/// `idle()` predicate on a nested sub-component path (e.g.
/// `env.drv.idle(N)`) walks the type chain to confirm the leaf is a
/// component-typed binding. Mirrors `resolve_component_method_call`'s
/// chain walk so the predicate works wherever method dispatch works.
#[test]
fn idle_predicate_resolves_through_nested_component_path() {
    let parsed = parse_source(
        r#"agent Worker
    in_ev : event<int>
end agent Worker

env TopEnv
    w : Worker
end env TopEnv

test NestedTest
    let dut : DummyDut
    let top : TopEnv
    run
        let hung = top.w.idle(100)
        wait 1 cycle
    end run
end test NestedTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(cpp.contains("top.w._last_in_cycle") && cpp.contains("top.w._last_out_cycle"),
        "nested idle() should walk through the env's field to the agent's heartbeat fields; got:\n{cpp}");
}

/// `env.quiesced(N)` lowers to all nested component heartbeat
/// predicates. In a timed `wait until`, it expands before diagnostic
/// emission so timeout logs attribute the blocker to `env.sub.idle(N)`
/// instead of the opaque aggregate helper.
#[test]
fn env_quiesced_aggregates_nested_component_idle_predicates() {
    let parsed = parse_source(
        r#"agent Producer
    in_ev : event<int>
end agent Producer

scoreboard DrainSb
    expected : queue<int>
end scoreboard DrainSb

env TopEnv
    prod : Producer
    sb   : DrainSb
end env TopEnv

test EnvQuiescedTest
    let dut : DummyDut
    let top : TopEnv
    run
        wait until top.quiesced(12)
            timeout 100 cycles fail("environment did not quiesce")
    end run
end test EnvQuiescedTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    assert!(
        cpp.contains("top.prod._last_in_cycle") && cpp.contains("top.prod._last_out_cycle"),
        "quiesced(N) should include nested agent heartbeat fields; got:\n{cpp}"
    );
    assert!(
        cpp.contains("top.sb._last_in_cycle") && cpp.contains("top.sb._last_out_cycle"),
        "quiesced(N) should include nested scoreboard heartbeat fields; got:\n{cpp}"
    );
    assert!(
        cpp.contains("not yet true: top.prod.idle(12)"),
        "timeout diagnostic should attribute the producer leaf; got:\n{cpp}"
    );
    assert!(
        cpp.contains("not yet true: top.sb.idle(12)"),
        "timeout diagnostic should attribute the scoreboard leaf; got:\n{cpp}"
    );
}

/// `wait until <expr>` with no timeout lowers to a direct
/// `co_await harc_rt::wait_until(_slot, [&]{ return <expr>; });` —
/// the most efficient shape (the scheduler evaluates the predicate
/// once per cycle and only resumes when true). Pins the lowering
/// shape (spec §7.9).
#[test]
fn wait_until_no_timeout_lowers_to_coroutine_wait_until() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        wait until dut.ready
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("co_await harc_rt::wait_until(_slot, [&]{ return")
            && cpp.contains("dut->ready"),
        "untimed wait until should lower to coroutine wait_until + predicate lambda; got:\n{cpp}",
    );
}

/// `wait until all of <e1>, <e2> timeout N cycles fail("...")` lowers
/// to a single `co_await harc_rt::wait_until_timeout(_slot, pred, N)`
/// call in coroutine context (the runtime handles the per-cycle
/// predicate evaluation and countdown internally — one scheduler
/// round-trip instead of N). On timeout the awaiter returns false
/// and the diagnostic block fires with per-sub-predicate breakdown.
/// The diagnostic identifies each sub-predicate by its pretty-printed
/// source text (so logs show `dut.ready` rather than a synthetic index).
#[test]
fn wait_until_all_of_with_timeout_emits_per_predicate_diagnostic() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        wait until all of dut.ready, dut.empty
            timeout 500 cycles fail("did not quiesce")
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Budget captured once into a local — same as before.
    assert!(
        cpp.contains("int64_t _wu_budget = (int64_t)(500);"),
        "expected `_wu_budget` initialized from the timeout expr; got:\n{cpp}"
    );
    // The optimization: a single co_await of wait_until_timeout
    // instead of a per-cycle co_await wait_cycles(1) polling loop.
    assert!(
        cpp.contains("co_await harc_rt::wait_until_timeout(_slot,")
            && cpp.contains("(uint32_t)_wu_budget);"),
        "expected single `co_await wait_until_timeout(_slot, pred, _wu_budget)`; got:\n{cpp}"
    );
    assert!(
        cpp.contains("if (!_wu_satisfied) {"),
        "expected `if (!_wu_satisfied)` guard around the diagnostic; got:\n{cpp}"
    );
    // No more per-cycle co_await wait_cycles(_slot, 1) in this lowering.
    // (Other wait_cycles calls — from `wait N cycles` elsewhere — may
    // appear, but inside the wait-until-timeout's brace block we don't
    // expect one. We assert the runtime helper is the only suspension.)
    assert!(
        !cpp.contains("if (!_wu_satisfied) {\n        co_await harc_rt::wait_cycles"),
        "wait_until_timeout should not be followed by a per-cycle polling loop; got:\n{cpp}"
    );
    // Per-sub-predicate breakdown: one line per condition still false.
    assert!(
        cpp.contains("not yet true: dut.ready"),
        "expected per-predicate diagnostic mentioning `dut.ready`; got:\n{cpp}"
    );
    assert!(
        cpp.contains("not yet true: dut.empty"),
        "expected per-predicate diagnostic mentioning `dut.empty`; got:\n{cpp}"
    );
    // User-supplied header line.
    assert!(
        cpp.contains("did not quiesce"),
        "expected the user-supplied fail() message in the timeout log; got:\n{cpp}"
    );
    // Errors counter bumps.
    assert!(
        cpp.contains("errors++;"),
        "expected `errors++;` on timeout; got:\n{cpp}"
    );
}

/// `wait until any of <e1>, <e2>` on timeout reports "none of" with
/// the joined source list (we can't say which one was supposed to
/// fire — by definition none did).
#[test]
fn wait_until_any_of_timeout_reports_none_of_list() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        wait until any of dut.error, dut.done
            timeout 200 cycles fail("expected error or done")
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // The overall cond is `||`-joined — emit_expr wraps DUT signals
    // in `harc_rt::harc_read(...)` so we test the join-shape with a
    // tolerant match rather than exact lexical comparison.
    assert!(
        cpp.contains("dut->error)) || (harc_rt::harc_read(dut->done"),
        "any-of overall predicate should be ||-joined; got:\n{cpp}"
    );
    // Diagnostic lists every sub-predicate (none fired).
    assert!(
        cpp.contains("none of: dut.error, dut.done"),
        "expected `none of:` listing every sub-predicate; got:\n{cpp}"
    );
}

/// `wait until <cond> timeout N cycles fail("…")` inside a *sync*
/// context (hookable body, free function) keeps the explicit polling
/// loop — `wait_until_timeout` is a coroutine awaiter and can't be
/// used here. The optimization to a single co_await applies only in
/// coroutine context (test-run body, bound-driver actor body, etc.).
#[test]
fn wait_until_with_timeout_in_sync_context_keeps_polling_loop() {
    let parsed = parse_source(
        r#"transactor X
    dut : DummyDut
    hookable wait_for_ready_bounded()
        wait until dut.ready timeout 100 cycles fail("ready never asserted")
    end wait_for_ready_bounded
end transactor X

test T
    let dut : DummyDut
    let xact : X passive
    run
        xact.wait_for_ready_bounded()
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Sync context: while-loop with tick() body.
    assert!(
        cpp.contains("_wu_start = (int64_t)cycle_count;")
            && cpp.contains("- _wu_start) < _wu_budget) {"),
        "sync timed wait-until should keep the explicit polling loop; got:\n{cpp}"
    );
    assert!(
        cpp.contains(") tick();") || cpp.contains("tick();\n            }"),
        "sync timed wait-until should call tick() each cycle; got:\n{cpp}"
    );
    // The coroutine awaiter must NOT be used here (it would suspend
    // a non-coroutine, which the C++ compiler would reject).
    assert!(
        !cpp.contains("co_await harc_rt::wait_until_timeout"),
        "sync timed wait-until should not use the coroutine awaiter; got:\n{cpp}"
    );
    // The user-supplied fail message still threads through.
    assert!(
        cpp.contains("ready never asserted"),
        "user fail message should still appear in sync diagnostic; got:\n{cpp}"
    );
}

/// `wait until <expr>` with no timeout still works inside a sync
/// context — e.g. inside a hookable method body — and lowers to a
/// `while (!cond) tick();` synchronous polling loop instead of
/// `co_await`. (Coroutines aren't available inside hookable bodies
/// because they run between coroutine yields, not as their own
/// coroutines.)
#[test]
fn wait_until_in_sync_context_uses_tick_loop() {
    let parsed = parse_source(
        r#"transactor X
    dut : DummyDut
    hookable wait_for_ready()
        wait until dut.ready
    end wait_for_ready
end transactor X

test T
    let dut : DummyDut
    let xact : X passive
    run
        xact.wait_for_ready()
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Inside a hookable, wait until must lower synchronously.
    assert!(
        cpp.contains("while (!(") && cpp.contains(")) tick();"),
        "wait until in sync context should be `while (!cond) tick();`; got:\n{cpp}"
    );
}

/// `on <N> cycles … end on` lowers to a `_checkers` closure with a
/// `static int64_t _last` counter and a `cycle_count - _last >= period`
/// guard — fires the body once every N cycles (spec §7.10). The
/// period expression is re-read each cycle so per-test overrides via
/// field assignment work without re-installation.
#[test]
fn on_n_cycles_lowers_to_periodic_checker() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        on 100 cycles
            log(info, "heartbeat")
        end on
        wait 5 cycles
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Body wrapped in a static-state _checkers closure.
    assert!(
        cpp.contains("_checkers.push_back([&]() {"),
        "expected periodic on-handler to register a _checkers closure; got:\n{cpp}"
    );
    // The period expression is captured into a local each cycle.
    assert!(
        cpp.contains("_period = (int64_t)(100);"),
        "expected period to be re-read each cycle; got:\n{cpp}"
    );
    // Guard against zero/negative period + correct delta comparison.
    assert!(
        cpp.contains("_period > 0") && cpp.contains("cycle_count -") && cpp.contains(">= "),
        "expected `_period > 0 && cycle_count - _last >= _period` guard; got:\n{cpp}"
    );
    // The body's log call survives.
    assert!(
        cpp.contains("\"heartbeat\""),
        "expected the body's log message; got:\n{cpp}"
    );
}

#[test]
fn on_phase_post_eval_lowers_to_post_eval_service() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        on 1 cycles phase post_eval
            log(info, "service")
        end on
        wait 2 cycles
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("std::vector<std::function<void()>> _post_eval_services;"),
        "expected generated post-eval service vector; got:\n{cpp}"
    );
    assert!(
        cpp.contains("_post_eval_services.push_back([&]() {"),
        "expected post-eval handler to register as a service; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("_checkers.push_back([&]() {\n        static int64_t _t_"),
        "post-eval periodic handler should not register as a checker; got:\n{cpp}"
    );
}

/// Regression for issue #477. With a declared `clock` (the multi-clock
/// scheduler / `now_ps` path) AND active `phase post_eval` services, the
/// generated `eval_clocks_until` loop must dump the waveform exactly ONCE
/// per physical timestamp. Previously it dumped at `now_ps`, ran the
/// post_eval services + a follow-up `eval`, then dumped again at the same
/// `now_ps`; Verilator's VCD tracer ignores the second dump and warns
/// `previous dump at t=..., dump call ignored` (thousands of lines in long
/// sims), and the *ignored* dump is the one carrying the settled post_eval
/// state. The fix sets the semantic trace timing before post_eval and
/// defers the single dump until after post_eval has settled DUT state.
#[test]
fn clocked_post_eval_dumps_waveform_once_per_timestamp() {
    let src = r#"domain SysDomain
  freq_mhz: 100
end domain SysDomain

test ClockedPostEvalTest
    let dut : Top
    clock clk = SysDomain
    run
        on 1 cycles phase post_eval
            log(info, "service")
        end on
        wait 2 cycles
    end run
end test ClockedPostEvalTest"#;
    let parsed = parse_source(src).unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    // Isolate the multi-clock scheduler body (declared `clock` => `now_ps`).
    let start = cpp
        .find("auto eval_clocks_until")
        .expect("expected clocked scheduler for a declared `clock`");
    let end = cpp[start..]
        .find("auto tick =")
        .map(|p| start + p)
        .expect("expected `tick` lambda after the scheduler");
    let body = &cpp[start..end];

    // Exactly one physical-timestamp waveform dump inside the scheduler loop.
    let dumps = body.matches("_harc_trace_dump_at((uint64_t)now_ps").count();
    assert_eq!(
        dumps, 1,
        "clocked + post_eval loop must dump the waveform once per timestamp (issue #477); \
         found {dumps}:\n{body}"
    );

    // Semantic trace timing is set *before* the post_eval services run (so
    // their trace events carry this edge's time) but the single dump is
    // deferred until *after* the services + follow-up eval settle DUT state.
    let set_timing = body
        .find("trace.set_timing((uint64_t)now_ps")
        .expect("expected set_timing before post_eval services");
    let services = body
        .find("for (auto& _svc : _post_eval_services) _svc();")
        .expect("expected post_eval services in the loop");
    let dump = body
        .find("_harc_trace_dump_at((uint64_t)now_ps")
        .expect("expected the deferred waveform dump");
    assert!(
        set_timing < services && services < dump,
        "expected order set_timing -> post_eval services -> dump (issue #477); \
         set_timing@{set_timing}, services@{services}, dump@{dump}\n{body}"
    );

    // The old duplicate-dump shape must be gone.
    assert!(
        !body.contains("if (_primary_rising && !_post_eval_services.empty()) _harc_trace_dump_at"),
        "the duplicate same-timestamp dump must be removed (issue #477); got:\n{body}"
    );
}

#[test]
fn active_post_eval_handler_calls_component_field_method() {
    let parsed = parse_source(
        r#"struct ReadResponse
    matched : uint<1>
    data : uint<32>
end struct ReadResponse

transactor ProtocolModel
    function predict_read(addr: uint<8>) -> ReadResponse
        let r : ReadResponse
        r.matched = 1
        r.data = addr + 256
        return r
    end predict_read
end transactor ProtocolModel

transactor BusResponder
    dut : ProviderDut
    model : ProtocolModel

    when active
        on 1 cycles phase post_eval
            if dut.req_valid != 0
                let r : ReadResponse = model.predict_read(dut.req_addr)
                dut.rsp_data = r.data
            end if
        end on
    end when
end transactor BusResponder

testbench Tb
    dut : ProviderDut
    responder : BusResponder active
end testbench Tb

impl ActivePostEvalProviderCallTest for Tb
    run
        responder.dut = dut
        wait 1 cycle
    end run
end impl ActivePostEvalProviderCallTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains(
            "ProtocolModel_predict_read(_tb.responder.model, harc_rt::harc_read(dut->req_addr))"
        ),
        "expected component-field provider call to dispatch through generated method; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("model.predict_read"),
        "bare component field calls must not fall through to C++ member calls; got:\n{cpp}"
    );
}

/// Issue #300 regression: a transactor-local `hookable` method
/// called from that same transactor's reactive `on N cycles phase
/// post_eval` handler must lower to the qualified
/// `<Type>_<method>(<instance>, ...)` form. Without the qualification
/// the generated C++ emits a bare `<method>()` identifier whose helper
/// symbol isn't in scope at the emit site, and the downstream C++
/// compile fails with `'<method>' was not declared in this scope`.
#[test]
fn transactor_local_hookable_call_from_post_eval_handler_is_qualified() {
    let parsed = parse_source(
        r#"transactor LocalMonitor
    hookable sample_value() -> uint<32>
        return 0
    end sample_value

    on 1 cycles phase post_eval
        let observed_value = sample_value()
    end on
end transactor LocalMonitor

testbench LocalHookableTb
    dut : DummyDut
    monitor : LocalMonitor active
end testbench LocalHookableTb

impl LocalHookableTest for LocalHookableTb
    run
        wait 2 cycles
    end run
end impl LocalHookableTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("LocalMonitor_sample_value(_tb.monitor)"),
        "transactor-local hookable call from a periodic post_eval \
         handler must lower to the qualified \
         `<Type>_<method>(<instance>)` form; got:\n{cpp}"
    );
    // And the bug pattern — a bare `sample_value()` call inside the
    // post_eval service closure — must not appear, since that's the
    // exact form that left the downstream C++ compile broken in #300.
    assert!(
        !cpp.contains("= sample_value();"),
        "post-eval body must not emit an unqualified bare-ident hookable call; got:\n{cpp}"
    );
}

#[test]
fn handler_component_field_types_do_not_overwrite_outer_let_types() {
    let parsed = parse_source(
        r#"struct ReadResponse
    matched : uint<1>
    data : uint<32>
end struct ReadResponse

transactor ProtocolModel
    function predict_read(addr: uint<8>) -> ReadResponse
        let r : ReadResponse
        r.matched = 1
        r.data = addr + 256
        return r
    end predict_read
end transactor ProtocolModel

transactor OuterModel
    function check_read(addr: uint<8>) -> ReadResponse
        let r : ReadResponse
        r.matched = 1
        r.data = addr + 512
        return r
    end check_read
end transactor OuterModel

transactor BusResponder
    dut : ProviderDut
    model : ProtocolModel

    when active
        on 1 cycles phase post_eval
            if dut.req_valid != 0
                let r : ReadResponse = model.predict_read(dut.req_addr)
                dut.rsp_data = r.data
            end if
        end on
    end when
end transactor BusResponder

test HandlerFieldTypeRestoreTest
    let dut : ProviderDut
    let model : OuterModel active
    let responder : BusResponder active
    run
        responder.dut = dut
        let r : ReadResponse = model.check_read(1)
        wait 1 cycle
    end run
end test HandlerFieldTypeRestoreTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("OuterModel_check_read(model, 1)"),
        "handler field type bindings must restore same-named outer lets; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("model.check_read"),
        "outer let call must not fall through after handler type scope; got:\n{cpp}"
    );
}

#[test]
fn component_typed_method_parameters_dispatch_provider_methods() {
    let parsed = parse_source(
        r#"struct ReadResponse
    matched : uint<1>
    data : uint<32>
end struct ReadResponse

transactor ProtocolModel
    function predict_read(addr: uint<8>) -> ReadResponse
        let r : ReadResponse
        r.matched = 1
        r.data = addr + 256
        return r
    end predict_read
end transactor ProtocolModel

scoreboard ResponseScoreboard
    function observe(addr: uint<8>, model: ProtocolModel)
        let r : ReadResponse = model.predict_read(addr)
    end observe
end scoreboard ResponseScoreboard

testbench Tb
    dut : DummyDut
    sb : ResponseScoreboard
    model : ProtocolModel active
end testbench Tb

impl ComponentParamDispatchTest for Tb
    run
        sb.observe(1, model)
    end run
end impl ComponentParamDispatchTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("ProtocolModel_predict_read(model, addr)"),
        "component-typed method parameters must dispatch provider methods; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("model.predict_read"),
        "component-typed method parameters must not fall through to C++ member calls; got:\n{cpp}"
    );
}

#[test]
fn testbench_method_bare_sibling_call_dispatches_through_self() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : DummyDut

    function write_reg(addr: uint<32>, data: uint<32>)
        dut.addr = addr
        dut.wdata = data
    end write_reg

    function program_defaults()
        write_reg(0x1000, 0)
        write_reg(0x1004, 1)
    end program_defaults
end testbench Tb

impl BareSiblingTestbenchCallTest for Tb
    run
        program_defaults()
    end run
end impl BareSiblingTestbenchCallTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("Tb_write_reg(self, 0x1000, 0)")
            && cpp.contains("Tb_write_reg(self, 0x1004, 1)"),
        "testbench sibling calls should dispatch through self; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("write_reg(0x1000, 0)") && !cpp.contains("write_reg(0x1004, 1)"),
        "testbench sibling calls must not emit as bare C++ calls; got:\n{cpp}"
    );
}

#[test]
fn transactor_method_bare_sibling_call_dispatches_through_self() {
    let parsed = parse_source(
        r#"transactor HelperTransactor
    function write_value(data: uint<32>)
        last = data
    end write_value

    function program_defaults()
        write_value(7)
    end program_defaults

    last : uint<32> default 0
end transactor HelperTransactor

testbench Tb
    dut : DummyDut
    helper : HelperTransactor active
end testbench Tb

impl BareSiblingTransactorCallTest for Tb
    run
        helper.program_defaults()
    end run
end impl BareSiblingTransactorCallTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("HelperTransactor_write_value(self, 7)"),
        "transactor sibling call should dispatch through self; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("write_value(7)"),
        "transactor sibling call must not emit as a bare C++ call; got:\n{cpp}"
    );
}

#[test]
fn transactor_always_on_bare_call_to_when_active_sibling_errors_for_passive_backdoor() {
    let parsed = parse_source(
        r#"transactor HelperTransactor
    function outer()
        active_only()
    end outer

    when active
        function active_only()
            last = 1
        end active_only
    end when

    last : uint<32> default 0
end transactor HelperTransactor

testbench Tb
    dut : DummyDut
    helper : HelperTransactor passive
end testbench Tb

impl BareSiblingActiveBackdoorTest for Tb
    run
        helper.outer()
    end run
end impl BareSiblingActiveBackdoorTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("active_only") && err.0.contains("when active") && err.0.contains("outer"),
        "expected bare active sibling call diagnostic, got:\n{}",
        err.0,
    );
}

#[test]
fn scoreboard_method_bare_sibling_call_dispatches_through_self() {
    let parsed = parse_source(
        r#"scoreboard Score
    count : uint<32> default 0

    function bump()
        count = count + 1
    end bump

    function observe()
        bump()
    end observe
end scoreboard Score

testbench Tb
    dut : DummyDut
    sb : Score
end testbench Tb

impl BareSiblingScoreboardCallTest for Tb
    run
        sb.observe()
    end run
end impl BareSiblingScoreboardCallTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("Score_bump(self)"),
        "scoreboard sibling call should dispatch through self; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("bump()"),
        "scoreboard sibling call must not emit as a bare C++ call; got:\n{cpp}"
    );
}

#[test]
fn scoreboard_queue_of_struct_lowers_to_typed_harc_queue() {
    let parsed = parse_source(
        r#"struct CheckerError
    checker_id : uint<8>
    code : uint<16>
    got : uint<64>
    expected : uint<64>
end struct CheckerError

scoreboard GlobalScoreboard
    errors : queue<CheckerError>
end scoreboard GlobalScoreboard

testbench Tb
    dut : DummyDut
    sb : GlobalScoreboard
end testbench Tb

impl TypedScoreboardQueueLoweringTest for Tb
    run
        assert sb.errors.empty() else fail("expected empty")
    end run
end impl TypedScoreboardQueueLoweringTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("HarcQueue<CheckerError> errors;"),
        "scoreboard queue<struct> should preserve record type; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("HarcQueue<uint64_t> errors;"),
        "scoreboard queue<struct> must not fall back to uint64_t; got:\n{cpp}"
    );
}

#[test]
fn scoreboard_method_pushes_struct_into_typed_queue() {
    let parsed = parse_source(
        r#"struct CheckerError
    checker_id : uint<8>
    code : uint<16>
    got : uint<64>
    expected : uint<64>
end struct CheckerError

scoreboard GlobalScoreboard
    errors : queue<CheckerError>

    function record_error(checker_id: uint<8>, code: uint<16>, got: uint<64>, expected: uint<64>)
        let err : CheckerError
        err.checker_id = checker_id
        err.code = code
        err.got = got
        err.expected = expected
        errors.push(err)
    end record_error
end scoreboard GlobalScoreboard

testbench Tb
    dut : DummyDut
    sb : GlobalScoreboard
end testbench Tb

impl TypedScoreboardQueuePushTest for Tb
    run
        sb.record_error(1, 0x1001, 2, 3)
    end run
end impl TypedScoreboardQueuePushTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("HarcQueue<CheckerError> errors;"),
        "scoreboard queue should preserve CheckerError element type; got:\n{cpp}"
    );
    assert!(
        cpp.contains("self.errors.push(err);"),
        "scoreboard method should push the typed record into the queue; got:\n{cpp}"
    );
}

#[test]
fn main_loop_runs_post_eval_services_before_coroutine_tick() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        on dut.ready phase post_eval level
            log(info, "ready")
        end on
        dut.x = 1
        wait 1 cycle
        dut.x = 2
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    let loop_pos = cpp
        .find("while (_run_slot.kind != harc_rt::WaitKind::Done")
        .expect("expected main run loop");
    let loop_body_end = cpp[loop_pos..]
        .find("\n    }\n")
        .map(|p| loop_pos + p)
        .expect("expected loop close");
    let body = &cpp[loop_pos..loop_body_end];
    let posedge_pos = body
        .find("_harc_eval_posedge(dut);")
        .expect("expected posedge eval inside loop");
    let service_pos = body
        .find("for (auto& _svc : _post_eval_services) _svc();")
        .expect("expected post-eval services inside loop");
    let tick_pos = body
        .find("sched.tick();")
        .expect("expected scheduler tick inside loop");
    let low_settle_pos = body
        .find("_harc_eval_negedge(dut);")
        .expect("expected clk-low settle inside loop");
    assert!(
        posedge_pos < service_pos && service_pos < tick_pos && tick_pos < low_settle_pos,
        "expected loop order: posedge eval -> post-eval service -> tick -> clk-low settle. \
         got posedge@{posedge_pos}, service@{service_pos}, tick@{tick_pos}, low@{low_settle_pos}\n\
         body:\n{}",
        body
    );
    assert!(
        body.contains("if (!_post_eval_services.empty()) dut->eval();"),
        "expected immediate re-eval after post-eval services; got:\n{body}"
    );
}

/// `watchdog` agent body item (spec §8.6) lowers to:
/// 1. Hook vectors `<Type>_watchdog_pre` / `<Type>_watchdog_post`
/// 2. A `<Type>_watchdog` method lambda whose body asserts the agent
///    has been idle for >= max_idle cycles
/// 3. A periodic `_checkers` closure at let-time that calls the
///    method every `period` cycles
#[test]
fn watchdog_lowers_to_method_plus_periodic_checker() {
    let parsed = parse_source(
        r#"agent Foo
    in_ev : event<int>

    watchdog
        period 250 cycles
        max_idle 1000 cycles
    end watchdog
end agent Foo

test T
    let dut : DummyDut
    let foo : Foo
    run
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Hook vectors so `on Foo.watchdog pre/post` works.
    assert!(
        cpp.contains("Foo_watchdog_pre;") && cpp.contains("Foo_watchdog_post;"),
        "expected watchdog hook vectors; got:\n{cpp}"
    );
    // Synthetic method lambda.
    assert!(
        cpp.contains("auto Foo_watchdog = [&](Foo& self) -> void {"),
        "expected `Foo_watchdog` lambda taking `Foo& self`; got:\n{cpp}"
    );
    // Idle check inside the method: BOTH in and out deltas must be ≥ max_idle.
    assert!(
        cpp.contains("self._last_in_cycle") && cpp.contains("self._last_out_cycle"),
        "expected idle check to read both heartbeat fields; got:\n{cpp}"
    );
    assert!(
        cpp.contains("_wdog_max_idle = (int64_t)(1000)"),
        "expected max_idle threshold from the watchdog clause; got:\n{cpp}"
    );
    assert!(
        cpp.contains("watchdog: Foo has been idle for"),
        "expected the watchdog fail message; got:\n{cpp}"
    );
    // Periodic checker installed at let-time: calls Foo_watchdog(foo) every `period` cycles.
    assert!(
        cpp.contains("_wdog_foo_period = (int64_t)(250)"),
        "expected per-instance period variable; got:\n{cpp}"
    );
    assert!(
        cpp.contains("Foo_watchdog(foo);"),
        "expected the periodic checker to call Foo_watchdog(foo); got:\n{cpp}"
    );
}

/// `watchdog disabled` emits NO hook vectors, NO method, NO periodic
/// checker — the user explicitly opted out. Existing fixtures that
/// don't declare a watchdog get the same treatment automatically
/// (no auto-injected watchdog), so this test pins both the
/// disabled-by-keyword path and the no-mention default.
#[test]
fn watchdog_disabled_emits_nothing() {
    let parsed = parse_source(
        r#"agent NoWdog
    in_ev : event<int>
    watchdog disabled
end agent NoWdog

test T
    let dut : DummyDut
    let nw : NoWdog
    run
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        !cpp.contains("NoWdog_watchdog"),
        "watchdog disabled should emit NO `NoWdog_watchdog` method/hooks; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("_wdog_nw"),
        "watchdog disabled should emit NO periodic checker for the instance; got:\n{cpp}"
    );
}

/// Watchdog period and max_idle can reference component fields, so
/// per-test overrides via field assignment work without recompiling
/// the agent. The field reference inside the period expression
/// rewrites to `<instance>.<field>` at let-time; inside the method
/// body it rewrites to `self.<field>`.
#[test]
fn watchdog_period_and_max_idle_can_reference_component_fields() {
    let parsed = parse_source(
        r#"agent Foo
    wdog_period   : uint<32> default 1000
    wdog_max_idle : uint<32> default 10000

    watchdog
        period wdog_period cycles
        max_idle wdog_max_idle cycles
    end watchdog
end agent Foo

test T
    let dut : DummyDut
    let foo : Foo
    run
        foo.wdog_period = 100
        foo.wdog_max_idle = 500
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Inside the method body, the field reference uses `self.`.
    assert!(
        cpp.contains("_wdog_max_idle = (int64_t)(harc_rt::harc_read(self.wdog_max_idle))")
            || cpp.contains("_wdog_max_idle = (int64_t)(self.wdog_max_idle)"),
        "max_idle inside method should resolve to self.wdog_max_idle; got:\n{cpp}"
    );
    // At let-time, the period reference uses `<instance>.`.
    assert!(
        cpp.contains("_wdog_foo_period = (int64_t)(harc_rt::harc_read(foo.wdog_period))")
            || cpp.contains("_wdog_foo_period = (int64_t)(foo.wdog_period)"),
        "period inside checker should resolve to foo.wdog_period; got:\n{cpp}"
    );
}

/// Transaction-level `keep` constraints flow through to the Z3
/// solver block on bare `randomize(t)` (no `with` clause). Before
/// this change, `keep` items were silently dropped — the parser
/// accepted them but the codegen only visited `TxnBodyItem::Field`,
/// so users could write `keep len in [1..256]` and `randomize(t)`
/// would happily produce `len = 0xFFFFFFFF`. Now every `randomize`
/// of a transaction with `keep`s routes through Z3.
#[test]
fn bare_randomize_routes_keep_constraints_through_z3() {
    let parsed = parse_source(
        r#"transaction T
    addr : uint<32>
    len  : uint<8>

    keep len in [1..16]
    keep addr % 4 == 0
end transaction T

test KeepTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test KeepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Z3 block emitted even though there's no `with`.
    assert!(
        cpp.contains("z3::context _ctx;") && cpp.contains("z3::solver _s(_ctx);"),
        "bare randomize on a keep-bearing txn should still emit the Z3 block; got:\n{cpp}"
    );
    // The keep constraints are added to the solver — len in [1..16]
    // lowers via z3::uge/ule, addr % 4 == 0 lowers as plain ==.
    assert!(
        cpp.contains("z3::uge(_z_len") && cpp.contains("z3::ule(_z_len"),
        "expected `len in [1..16]` to lower to uge/ule pair; got:\n{cpp}"
    );
    assert!(
        cpp.contains("_z_addr") && cpp.contains(" %% ")
            || cpp.contains("_z_addr") && cpp.contains("z3"),
        "expected `addr % 4 == 0` to reach the solver; got:\n{cpp}"
    );
    // No fallback to randomize_T(&t) — that would silently bypass the keeps.
    assert!(
        !cpp.contains("randomize_T(&t);"),
        "should NOT fall back to PRNG `randomize_T`; got:\n{cpp}"
    );
}

/// Both transaction-level `keep`s AND the user's `with` body are
/// added to the same Z3 solver call. The user's constraints can
/// reference the same fields the keeps constrain — the solver
/// finds a satisfying assignment across the combined set.
#[test]
fn randomize_with_merges_keeps_and_user_constraints() {
    let parsed = parse_source(
        r#"transaction T
    val : uint<32>
    keep val in [10..200]
end transaction T

test MergeTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.val > 100
        end randomize
    end run
end test MergeTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Both constraints reach the solver: the txn's `keep val in [10..200]`
    // AND the user's `t.val > 100`. Z3 has to satisfy both.
    assert!(
        cpp.contains("z3::uge(_z_val") && cpp.contains("z3::ule(_z_val"),
        "transaction's `val in [10..200]` should still apply; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::ugt(_z_val, _ctx.bv_val((uint64_t)100"),
        "user's `t.val > 100` should also reach the solver; got:\n{cpp}"
    );
}

#[test]
fn transaction_list_fields_randomize_and_support_len_method() {
    let parsed = parse_source(
        r#"transaction Packet
    items : list<uint<8>>
end transaction Packet

test ListRandomizeTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p) with
            p.items.len() <= 4
        end randomize
        assert p.items.len() <= 4
    end run
end test ListRandomizeTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("std::vector<uint64_t> items = {};"),
        "list fields should lower to std::vector storage; got:\n{cpp}"
    );
    assert!(
        cpp.contains("p.items.resize((size_t)_raw_items_len);"),
        "solver randomize should choose the intrinsic list length; got:\n{cpp}"
    );
    assert!(
        cpp.contains("p.items.size() <= 4"),
        "`items.len()` should lower to the vector size method; got:\n{cpp}"
    );
}

#[test]
fn constraint_solver_supports_list_len_and_sum_slice() {
    let parsed = parse_source(
        r#"transaction Packet
    items : list<uint<8>>
    total : uint<10>
end transaction Packet

test ListConstraintTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p) with
            p.items.len() >= 1
            p.items.len() <= 4
            sum(p.items[0..p.items.len()]) == p.total
            p.total == 7
        end randomize
    end run
end test ListConstraintTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_items_len = _ctx.bv_const(\"items_len\", 64);"),
        "solver should allocate a length variable for list fields; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::expr _z_items_0 = _ctx.bv_const(\"items_0\", 64);")
            && cpp.contains("z3::expr _z_items_3 = _ctx.bv_const(\"items_3\", 64);"),
        "solver should allocate fixed element slots up to max length; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::ugt(_z_items_len, _ctx.bv_val((uint64_t)0, 64))")
            && cpp.contains("_z_items_0"),
        "`sum(items[0..items.len()])` should lower with length guards; got:\n{cpp}"
    );
    assert!(
        cpp.contains("p.items.resize((size_t)_raw_items_len);"),
        "solver model should resize the vector to the solved intrinsic length; got:\n{cpp}"
    );
}

#[test]
fn constraint_solver_supports_modulo_of_list_sum() {
    let parsed = parse_source(
        r#"transaction Packet
    items : list<uint<8>>
end transaction Packet

test ListModuloConstraintTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p) with
            p.items.len() >= 1
            p.items.len() <= 4
            sum(p.items[0..p.items.len()]) % 4 == 0
        end randomize
    end run
end test ListModuloConstraintTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::urem("),
        "modulo on list-sum constraints should lower through unsigned remainder; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::ugt(_z_items_len, _ctx.bv_val((uint64_t)0, 64))")
            && cpp.contains("_z_items_0"),
        "modulo operand should preserve guarded list-sum lowering; got:\n{cpp}"
    );
}

#[test]
fn constraint_solver_supports_foreach_list_item_constraints() {
    let parsed = parse_source(
        r#"transaction Packet
    items : list<uint<8>>
end transaction Packet

test ListForeachConstraintTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p) with
            p.items.len() >= 1
            p.items.len() <= 4
            for item in p.items
                item > 0
                item < 16
            end for
        end randomize
    end run
end test ListForeachConstraintTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::ule(_z_items_len, _ctx.bv_val((uint64_t)0, 64)) || (z3::ugt(_z_items_0"),
        "foreach should lower item constraints under a len<=index guard; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::ult(_z_items_3, _ctx.bv_val((uint64_t)16, 64))"),
        "foreach should unroll item constraints through the inferred length bound; got:\n{cpp}"
    );
}

#[test]
fn transaction_keep_supports_foreach_list_item_constraints() {
    let parsed = parse_source(
        r#"transaction Packet
    items : list<uint<8>>
    keep items.len() >= 1
    keep items.len() <= 4
    keep for item in items
        item > 0
        item < 16
    end for
end transaction Packet

test ListForeachKeepTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p)
    end run
end test ListForeachKeepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::ule(_z_items_len, _ctx.bv_val((uint64_t)0, 64)) || (z3::ugt(_z_items_0"),
        "foreach keep should lower item constraints under a len<=index guard; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::ult(_z_items_3, _ctx.bv_val((uint64_t)16, 64))"),
        "foreach keep should unroll item constraints through the inferred length bound; got:\n{cpp}"
    );
}

#[test]
fn when_subtype_keep_supports_foreach_list_item_constraints() {
    let parsed = parse_source(
        r#"transaction Packet
    enabled : bool
    items : list<uint<8>>
    keep items.len() <= 2

    when enabled
        keep for item in items
            item > 7
        end for
    end when
end transaction Packet

test ListForeachWhenKeepTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p)
    end run
end test ListForeachWhenKeepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::ule(_z_items_len, _ctx.bv_val((uint64_t)0, 64)) || (!((_z_enabled != _ctx.bv_val((uint64_t)0, 64))) || z3::ugt(_z_items_0"),
        "when-guarded foreach keep should distribute the guard into each unrolled item constraint; got:\n{cpp}"
    );
}

#[test]
fn constraint_solver_seed_flows_from_harc_rng() {
    let parsed = parse_source(
        r#"transaction T
    val : uint<32>
    keep val > 100
end transaction T

test SeededSolverTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test SeededSolverTest"#,
    )
    .unwrap();
    let [problem_id] = randomize_problem_ids(&parsed)[..] else {
        panic!("expected one randomize problem")
    };
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    assert!(
        cpp.contains("z3::params _p(_ctx);")
            && cpp.contains(&format!(
                "harc_prepare_randomize_call(ctx, {problem_id}ULL, ctx.rng.initial_seed, 0)"
            ))
            && cpp.contains("harc_rt::random::harc_solve_constrained(")
            && cpp.contains("harc_rt::random::harc_handle_solve_status(")
            && cpp.contains(
                "_p.set(\"random_seed\", static_cast<unsigned>(_harc_rt_seed & 0x7fffffffU));"
            )
            && cpp.contains("_s.set(_p);"),
        "Z3 solver-backed randomize should consume the runtime-derived HARC seed; got:\n{cpp}"
    );
    assert!(
        !cpp.contains(
            "_p.set(\"random_seed\", static_cast<unsigned>(harc_rng_next() & 0x7fffffffU));"
        ),
        "solver seed should not bypass runtime call-site seeding; got:\n{cpp}"
    );
    assert!(
        cpp.contains("uint64_t _pref_")
            && cpp.contains("_s.add(_z_val == harc_z3_bv_value(_ctx, _pref_")
            && cpp.contains("harc_rt::random::HarcSolverRetryPolicy _harc_rt_retry_policy;")
            && cpp.contains("harc_rt::random::harc_retry_without_preferences(")
            && cpp.contains("harc_rt::random::harc_retry_without_unique_history(")
            && cpp.contains("retry without seeded preferences")
            && !cpp.contains("static std::vector"),
        "ordinary solver-backed randomize should use seeded free-field preferences without persistent diversity history; got:\n{cpp}"
    );
}

#[test]
fn constraint_solver_uses_range_and_dist_metadata() {
    let parsed = parse_source(
        r#"transaction T
    len : uint<8> with [range(1, 4)] [dist {[1..2] :/ 70, [3..4] :/ 30}]
    keep len > 0
end transaction T

test DistSolverTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.len dist { [3..4] :/ 90, [1..2] :/ 10 }
        end randomize
    end run
end test DistSolverTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    assert!(
        cpp.contains("z3::uge(_z_len, _ctx.bv_val((uint64_t)1, 64))")
            && cpp.contains("z3::ule(_z_len, _ctx.bv_val((uint64_t)4, 64))"),
        "field [range] should lower as hard solver bounds; got:\n{cpp}"
    );
    assert!(
        cpp.contains("uint64_t _pref_")
            && cpp.contains(
                "harc_rt::random::harc_prefer_dist(_harc_rt_seed, 0, {{(int64_t)(3), (int64_t)(4), (int64_t)(90)}"
            )
            && cpp.contains("{(int64_t)(1), (int64_t)(2), (int64_t)(10)}}"),
        "dist metadata should feed seeded solver preferences; got:\n{cpp}"
    );
}

#[test]
fn auto_coverage_goals_feed_solver_preferences() {
    let parsed = parse_source(
        r#"enum Op { Read, Write }

transaction T
    op : Op
    len : uint<8> with [range(1, 4)]
end transaction T

test AutoCovPrefTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test AutoCovPrefTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("HarcAutoCovPlan _auto_cov_plan_")
            && cpp.contains("HarcAutoCovState _auto_cov_state_")
            && cpp.contains("HarcAutoCovPointMeta _auto_cov_points_")
            && cpp.contains("HarcAutoCovCrossMeta _auto_cov_crosses_")
            && cpp.contains("_auto_cov_labels_")
            && cpp.contains("_op[]")
            && cpp.contains("_len[]")
            && cpp.contains("_auto_cross_labels_")
            && cpp.contains("__len[]")
            && cpp.contains("std::vector<std::function<void()>> _auto_cov_reports;")
            && cpp.contains("harc_auto_cov_register_report(")
            && cpp.contains("harc_auto_cov_report(_auto_cov_plan_")
            && cpp.contains("T.op=Read")
            && cpp.contains("T.op=Write")
            && cpp.contains("T.len=1")
            && cpp.contains("T.len=4")
            && cpp.contains("T.op=Read x T.len=1")
            && cpp.contains("harc_rt::random::HarcAutoCovSelection _auto_cov_selection_")
            && cpp.contains("_pref_")
            && cpp.contains("_auto_point_vals_")
            && cpp.contains("_auto_cross_vals_")
            && cpp.contains("{0ULL, 1ULL}")
            && cpp.contains("{1ULL, 4ULL}")
            && cpp.contains("harc_auto_cov_apply_point_preference(_auto_cov_plan_")
            && cpp.contains("harc_auto_cov_apply_cross_preference(_auto_cov_plan_")
            && cpp.contains("harc_auto_cov_mark_selected_point_blocked(_auto_cov_plan_")
            && cpp.contains("harc_auto_cov_mark_selected_cross_blocked(_auto_cov_plan_")
            && cpp.contains("harc_auto_cov_mark_value_hit(_val_")
            && cpp.contains("harc_auto_cov_mark_cross_hit(_val_")
            && cpp.contains("harc_rt::random::harc_retry_without_preferences(")
            && cpp.contains("retry without seeded preferences")
            && !cpp.contains("_auto_cov_blocked_")
            && !cpp.contains("static bool _auto_cross_"),
        "auto coverage goals and pairwise crosses should feed solver preferences, hit/blocked tracking, and reporting; got:\n{cpp}",
    );
}

#[test]
fn constrained_field_skips_auto_coverage_preferences() {
    let parsed = parse_source(
        r#"enum Op { Read, Write }

transaction T
    op : Op
    len : uint<8> with [range(1, 4)]
end transaction T

test AutoCovConstrainedTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.op == Read
        end randomize
    end run
end test AutoCovConstrainedTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        (!cpp.contains("_auto_cov_labels_") || !cpp.contains("_op[]"))
            && cpp.contains("_len[]")
            && !cpp.contains("_auto_cross_labels_"),
        "explicitly constrained fields should not receive auto coverage preferences or crosses; got:\n{cpp}",
    );
}

#[test]
fn signed_range_auto_coverage_includes_negative_endpoint() {
    let parsed = parse_source(
        r#"transaction T
    delta : sint<8> with [range(-4, 4)]
end transaction T

test SignedAutoCovPrefTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test SignedAutoCovPrefTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("_delta, 2")
            && cpp.contains("{-4LL, 4LL}")
            && cpp.contains("T.delta=-4")
            && cpp.contains("T.delta=4")
            && cpp.contains("harc_auto_cov_mark_value_hit(_val_delta, -4LL"),
        "signed [range] endpoints should feed auto coverage preferences and report as signed values; got:\n{cpp}",
    );
}

#[test]
fn natural_numeric_endpoints_feed_auto_coverage_preferences() {
    let parsed = parse_source(
        r#"transaction T
    addr : uint<8>
    delta : sint<4>
end transaction T

test NaturalEndpointAutoCovTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test NaturalEndpointAutoCovTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("_addr, 18")
            && cpp.contains("_delta, 2")
            && cpp.contains("T.addr=0")
            && cpp.contains("T.addr=255")
            && cpp.contains("T.addr=1")
            && cpp.contains("T.addr=128")
            && cpp.contains("T.addr=254")
            && cpp.contains("T.addr=127")
            && cpp.contains("T.delta=-8")
            && cpp.contains("T.delta=7")
            && cpp.contains("{0ULL, 255ULL, 1ULL, 254ULL, 2ULL, 253ULL")
            && cpp.contains("{-8LL, 7LL}"),
        "natural numeric min/max and walking-bit endpoints should feed auto coverage preferences without redundant [range] attrs; got:\n{cpp}",
    );
}

#[test]
fn wide_numeric_auto_coverage_uses_full_width_solver_values() {
    let parsed = parse_source(
        r#"transaction T
    data : uint<128>
end transaction T

test WideEndpointAutoCovTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test WideEndpointAutoCovTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_data = _ctx.bv_const(\"data\", 128);")
            && cpp.contains("_harc_u128 _pref_")
            && cpp.contains("harc_rt::random::harc_prefer_u128(_harc_rt_seed, 0, 128)")
            && cpp.contains("_data, 34")
            && cpp.contains("T.data=2^128-1")
            && cpp.contains("T.data=2^127")
            && cpp.contains("harc_z3_bv_value(_ctx, _pref")
            && cpp.contains("Z3_get_numeral_binary_string(_ctx, _eval_data)")
            && cpp.contains("harc_rt::harc_wide_from_binary<4>(_bin_data)")
            && !cpp.contains("is_numeral_u64(_raw_data)"),
        ">64-bit fields should feed full-width auto coverage preferences and model extraction; got:\n{cpp}",
    );
}

#[test]
fn signed_wide_auto_coverage_uses_signed_full_width_solver_values() {
    let parsed = parse_source(
        r#"transaction T
    data : sint<128>
end transaction T

test SignedWideEndpointAutoCovTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test SignedWideEndpointAutoCovTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_data = _ctx.bv_const(\"data\", 128);")
            && cpp.contains("_harc_u128 _pref_")
            && cpp.contains("T.data=-2^127")
            && cpp.contains("T.data=2^127-1")
            && cpp.contains("harc_z3_bv_signed_value(_ctx, _pref")
            && cpp.contains(", 128, 128)")
            && cpp.contains("Z3_get_numeral_binary_string(_ctx, _eval_data)")
            && cpp.contains("harc_rt::harc_wide_from_binary<4>(_bin_data)")
            && !cpp.contains("T.data=2^128-1"),
        "signed >64-bit fields should feed signed full-width auto coverage preferences and model extraction; got:\n{cpp}",
    );
}

#[test]
fn mixed_width_signed_wide_solver_uses_sign_extended_domain_and_preferences() {
    let parsed = parse_source(
        r#"transaction T
    wide : bits<256>
    delta : sint<128>
    keep wide != 0
    keep delta < 0
end transaction T

test SignedWideMixedSolverTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test SignedWideMixedSolverTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_delta = _ctx.bv_const(\"delta\", 256);")
            && cpp.contains("_z_delta >= harc_z3_bv_signed_value(_ctx, ")
            && cpp.contains(", 128, 256)")
            && cpp.contains("_z_delta <= harc_z3_bv_signed_value(_ctx, ")
            && cpp.contains("_s.add(_z_delta == harc_z3_bv_signed_value(_ctx, _pref")
            && cpp.contains(", 128, 256));"),
        "signed wide fields in a wider solver block should sign-extend domain bounds and preferences; got:\n{cpp}",
    );
}

#[test]
fn mixed_width_signed_solver_extracts_twos_complement_low_bits() {
    let parsed = parse_source(
        r#"transaction T
    data : uint<128>
    delta : sint<8>
    keep data > 0
    keep delta < 0
end transaction T

test MixedWidthSignedSolverTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test MixedWidthSignedSolverTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_delta = _ctx.bv_const(\"delta\", 128);")
            && cpp.contains("_z_delta >= harc_z3_bv_signed_value(_ctx, (int64_t)(-(1LL << 7)), 8, 128)")
            && cpp.contains("_z_delta <= harc_z3_bv_signed_value(_ctx, (int64_t)((1LL << 7) - 1), 8, 128)")
            && cpp.contains("_z_delta < harc_z3_bv_value(_ctx, (int64_t)(0), 128)")
            && cpp.contains("uint64_t _raw_delta = harc_z3_bv_low_u64(_ctx, _eval_delta);")
            && cpp.contains("int64_t _val_delta = (int64_t)_raw_delta;"),
        "signed narrow fields in a wide solver block should keep signed bounds and extract low two's-complement bits; got:\n{cpp}",
    );
}

#[test]
fn wide_constraint_literals_do_not_truncate_through_uint64() {
    let parsed = parse_source(
        r#"transaction T
    data : bits<256>
    keep data == 256'h8000000000000000000000000000000000000000000000000000000000000001
end transaction T

test WideLiteralSolverTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test WideLiteralSolverTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_data = _ctx.bv_const(\"data\", 256);")
            && cpp.contains("_z_data == harc_z3_bv_value(_ctx, harc_rt::HarcWide<8>")
            && !cpp.contains("_z_data == _ctx.bv_val((uint64_t)"),
        "wide constraint literals should lower through word values, not uint64_t truncation; got:\n{cpp}",
    );
}

#[test]
fn auto_coverage_caps_1024_bit_numeric_fields() {
    let parsed = parse_source(
        r#"transaction T
    data : bits<1024>
end transaction T

test Wide1024EndpointAutoCovTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test Wide1024EndpointAutoCovTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_data = _ctx.bv_const(\"data\", 1024);")
            && cpp.contains("harc_rt::HarcWide<32>")
            && cpp.contains("harc_rt::random::harc_prefer_wide<32>(_harc_rt_seed, 0, 1024)")
            && cpp.contains("_data, 34")
            && cpp.contains("T.data=2^1024-1")
            && cpp.contains("T.data=2^1023")
            && !cpp.contains("_data, 66"),
        "1024-bit fields should get capped min/max and walking-pattern auto coverage, not unbounded bins; got:\n{cpp}",
    );
}

#[test]
fn walking_auto_coverage_caps_wide_numeric_fields() {
    let parsed = parse_source(
        r#"transaction T
    data : uint<32>
end transaction T

test WalkingAutoCovPrefTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test WalkingAutoCovPrefTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("_data, 34")
            && cpp.contains("T.data=0")
            && cpp.contains("T.data=4294967295")
            && cpp.contains("T.data=1")
            && cpp.contains("T.data=4294967294")
            && cpp.contains("T.data=2147483648")
            && cpp.contains("T.data=2147483647")
            && !cpp.contains("_data, 66"),
        "uint<32> should get min/max plus capped walking-one/walking-zero goals, not every bit twice without a cap; got:\n{cpp}",
    );
}

#[test]
fn uint64_unique_randomize_uses_checked_model_extraction() {
    let parsed = parse_source(
        r#"transaction T
    data : uint<64> with [unique within test]
end transaction T

test Uint64UniqueTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test Uint64UniqueTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("_data, 34")
            && cpp.contains("18446744073709551615ULL")
            && cpp.contains("9223372036854775808ULL")
            && cpp.contains("z3::expr _eval_data = _m.eval(_z_data, true).simplify();")
            && cpp.contains("uint64_t _raw_data = harc_z3_bv_low_u64(_ctx, _eval_data);")
            && cpp.contains("uint64_t _val_data = (uint64_t)_raw_data;")
            && !cpp.contains("_m.eval(_z_data).get_numeral_uint64()"),
        "uint<64> unique randomize should keep high auto-coverage endpoints and avoid assert-heavy Z3 extraction; got:\n{cpp}",
    );
}

#[test]
fn unique_field_randomize_uses_recycling_solver_history() {
    let parsed = parse_source(
        r#"transaction T
    tag : uint<8> with [unique within test]
end transaction T

test UniqueTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test UniqueTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::solver _s(_ctx);")
            && cpp.contains(
                "for (auto _v : harc_rt::random::harc_unique_values(ctx.rng.unique.get<uint64_t>("
            )
            && cpp.contains("ULL))) _s.add")
            && cpp.contains("// [unique within test] policy: no repeat until exhausted")
            && cpp.contains("harc_rt::random::harc_retry_without_unique_history(")
            && cpp.contains(
                "harc_rt::random::harc_unique_clear(ctx.rng.unique.get<uint64_t>("
            )
            && cpp.contains(
                "harc_rt::random::harc_unique_remember(ctx.rng.unique.get<uint64_t>("
            )
            && cpp.contains("_s.push();   // unique history constraints")
            && cpp.contains("_s.pop();   // drop exhausted unique-history scope")
            && !cpp.contains("if (_solver_site_")
            && !cpp.contains("randomize_T(&t);")
            && !cpp.contains("thread_local")
            && !cpp.contains("static harc_rt::random::HarcRng")
            && !cpp.contains("static harc_rt::random::HarcUniqueHistory")
            && !cpp.contains("static harc_rt::random::HarcAutoCovState")
            && !cpp.contains("static harc_rt::random::HarcRuntimeCallSite"),
        "unique fields should route bare randomize through scoped recycling solver history; got:\n{cpp}",
    );
    let history_pos = cpp
        .find("for (auto _v : harc_rt::random::harc_unique_values(ctx.rng.unique.get<uint64_t>(")
        .expect("unique history constraint");
    let preference_pos = cpp
        .find("_s.push();   // seeded candidate preferences")
        .expect("seeded preference scope");
    assert!(
        history_pos < preference_pos,
        "unique history must be applied before seeded preferences so preference retry preserves it"
    );
}

#[test]
fn constrained_unique_field_keeps_its_scoped_history() {
    let parsed = parse_source(
        r#"transaction T
    tag : uint<8> with [unique within tseq]
end transaction T

tseq Draw() -> TSeq<T>
    let first : T
    let second : T
    randomize(first) with
        first.tag >= 0
    end randomize
    randomize(second) with
        second.tag >= 0
    end randomize
    yield first
    yield second
end tseq Draw

test ConstrainedUniqueTest
    let dut : DummyDut
    run
        let draws = Draw()
    end run
end test ConstrainedUniqueTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert_eq!(
        cpp.matches(
            "for (auto _v : harc_rt::random::harc_unique_values(_harc_unique_tseq.get<uint64_t>("
        )
        .count(),
        2,
        "both constrained sites must consult the invocation history:\n{cpp}"
    );
    assert_eq!(
        cpp.matches("harc_rt::random::harc_unique_remember(_harc_unique_tseq.get<uint64_t>(")
            .count(),
        2,
        "both constrained sites must update the invocation history:\n{cpp}"
    );
}

#[test]
fn bare_unique_is_per_call_and_allocates_no_persistent_history() {
    let src = r#"transaction T
    tag : uint<8> with [unique]
end transaction T

test BareUniqueTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test BareUniqueTest"#;
    let cpp = v1_cpp(src);
    assert!(cpp.contains("z3::solver _s(_ctx);"), "{cpp}");
    assert!(!cpp.contains("harc_unique_values("), "{cpp}");
    assert!(!cpp.contains("harc_unique_clear("), "{cpp}");
    assert!(!cpp.contains("harc_unique_remember("), "{cpp}");
    assert!(!cpp.contains(".rng.unique.get<"), "{cpp}");
}

#[test]
fn bare_unique_list_is_distinct_only_within_the_current_call() {
    let src = r#"transaction T
    items : list<uint<8>> with [unique]
    keep items.len() == 3
end transaction T

test BareUniqueListTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test BareUniqueListTest"#;
    let cpp = v1_cpp(src);
    assert!(
        cpp.contains(
            "_s.add(z3::ule(_z_items_len, _ctx.bv_val((uint64_t)1, 64)) || (_z_items_0 != _z_items_1));   // [unique] within this randomize call"
        ),
        "{cpp}"
    );
    assert!(
        cpp.contains(
            "_s.add(z3::ule(_z_items_len, _ctx.bv_val((uint64_t)2, 64)) || (_z_items_1 != _z_items_2));   // [unique] within this randomize call"
        ),
        "{cpp}"
    );
    assert!(!cpp.contains(".rng.unique.get<"), "{cpp}");
}

#[test]
fn unique_within_test_shares_one_history_across_randomize_sites() {
    let src = r#"transaction T
    tag : uint<8> with [unique within test]
end transaction T

test SharedUniqueTest
    let dut : DummyDut
    run
        let first : T
        let second : T
        randomize(first)
        randomize(second)
    end run
end test SharedUniqueTest"#;
    let cpp = v1_cpp(src);
    let marker = "ctx.rng.unique.get<uint64_t>(";
    let keys = cpp
        .match_indices(marker)
        .map(|(start, _)| {
            let tail = &cpp[start + marker.len()..];
            tail.split("ULL").next().expect("unique key")
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), 1, "both sites must share test history:\n{cpp}");
}

#[test]
fn unique_within_tseq_is_owned_by_each_invocation() {
    let src = r#"transaction T
    tag : uint<8> with [unique within tseq]
end transaction T

tseq Draw() -> TSeq<T>
    let t : T
    randomize(t)
    yield t
end tseq Draw

test TseqUniqueTest
    let dut : DummyDut
    run
        let first = Draw()
        let second = Draw()
    end run
end test TseqUniqueTest"#;
    let parsed = parse_source(src).expect("parse");
    let v1 = cpp_tb::emit(&parsed).expect("v1 emit");
    let program = harc::ir::lower::lower_program(&parsed).expect("lower");
    let tbir = harc::codegen::tbir::emit(&program, &parsed, &cpp_tb::EmitOpts::default())
        .expect("TB-IR emit");
    for (backend, cpp) in [("v1", v1), ("TB-IR", tbir)] {
        assert!(
            cpp.contains("harc_rt::random::HarcUniqueRegistry _harc_unique_tseq;"),
            "{backend}: tseq invocation must own its history:\n{cpp}"
        );
        assert!(
            cpp.contains("_harc_unique_tseq.get<uint64_t>("),
            "{backend}: tseq randomize must use the invocation registry:\n{cpp}"
        );
        assert!(
            !cpp.contains("ctx.rng.unique.get<uint64_t>("),
            "{backend}: {cpp}"
        );
    }
}

#[test]
fn unique_within_sequencer_is_owned_by_each_instance() {
    let src = r#"transaction T
    tag : uint<8> with [unique within sequencer]
end transaction T

sequencer Source
    _harc_unique : uint<8> default 0
    _harc_unique_1 : uint<8> default 0
    hookable draw()
        let t : T
        randomize(t)
    end draw
end sequencer Source

test SequencerUniqueTest
    let dut : DummyDut
    let first : Source
    let second : Source
    run
        first.draw()
        second.draw()
    end run
end test SequencerUniqueTest"#;
    let parsed = parse_source(src).expect("parse");
    let v1 = cpp_tb::emit(&parsed).expect("v1 emit");
    let program = harc::ir::lower::lower_program(&parsed).expect("lower");
    let tbir = harc::codegen::tbir::emit(&program, &parsed, &cpp_tb::EmitOpts::default())
        .expect("TB-IR emit");
    for (backend, cpp) in [("v1", v1), ("TB-IR", tbir)] {
        assert!(
            cpp.contains("uint64_t _harc_unique = 0;")
                && cpp.contains("uint64_t _harc_unique_1 = 0;")
                && cpp.contains("harc_rt::random::HarcUniqueRegistry _harc_unique_2;"),
            "{backend}: each sequencer object must own a registry:\n{cpp}"
        );
        assert!(
            cpp.contains("self._harc_unique_2.get<uint64_t>("),
            "{backend}: sequencer randomize must use its receiver's registry:\n{cpp}"
        );
        assert!(
            !cpp.contains("ctx.rng.unique.get<uint64_t>("),
            "{backend}: {cpp}"
        );
    }
}

#[test]
fn scoped_unique_rejects_the_wrong_lexical_owner_in_both_backends() {
    for (scope, required_owner) in [("tseq", "tseq"), ("sequencer", "sequencer")] {
        let src = format!(
            r#"transaction T
    tag : uint<8> with [unique within {scope}]
end transaction T

test InvalidUniqueScope
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test InvalidUniqueScope"#
        );
        let parsed = parse_source(&src).expect("parse");
        let v1 = cpp_tb::emit(&parsed).expect_err("v1 rejects invalid unique scope");
        assert!(v1.0.contains(required_owner), "v1: {v1}");
        let program = harc::ir::lower::lower_program(&parsed).expect("lower");
        let tbir = harc::codegen::tbir::emit(&program, &parsed, &cpp_tb::EmitOpts::default())
            .expect_err("TB-IR rejects invalid unique scope");
        assert!(tbir.0.contains(required_owner), "TB-IR: {tbir}");
    }
}

#[test]
fn constrained_unique_field_keeps_unique_history_policy() {
    let parsed = parse_source(
        r#"transaction T
    tag : uint<8> with [unique within test]
end transaction T

test UniqueOverrideTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.tag == 7
        end randomize
    end run
end test UniqueOverrideTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("_s.add(_z_tag == _ctx.bv_val((uint64_t)7, 64));")
            && cpp.contains("// [unique within test] policy: no repeat until exhausted")
            && cpp.contains("harc_rt::random::harc_unique_remember(ctx.rng.unique.get<uint64_t>("),
        "explicit constraints narrow the unique domain without disabling its history; got:\n{cpp}",
    );
}

#[test]
fn range_constrained_unique_field_uses_seeded_sampling_and_history() {
    let parsed = parse_source(
        r#"transaction T
    tag : uint<8> with [unique within test]
end transaction T

test UniqueConstrainedTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.tag inside {7, 8}
        end randomize
    end run
end test UniqueConstrainedTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("uint64_t _pref_")
            && cpp.contains("harc_rt::random::harc_retry_without_preferences(")
            && cpp.contains("retry without seeded preferences")
            && !cpp.contains("static std::vector")
            && cpp.contains("// [unique within test] policy: no repeat until exhausted")
            && cpp.contains("harc_rt::random::harc_unique_remember(ctx.rng.unique.get<uint64_t>("),
        "constraints mentioning a unique field should preserve both scoped history and seeded sampling; got:\n{cpp}",
    );
}

#[test]
fn randomize_with_bit_slice_constraints_emit_solver_masks() {
    let parsed = parse_source(
        r#"transaction Choice
    word : uint<32>
    addr : uint<32>
end transaction Choice

test BitSliceRandomizeTest
    let dut : DummyDut
    run
        let c : Choice
        randomize(c) with
            c.word[1:0] == 3
            c.addr[0] == 0
        end randomize
    end run
end test BitSliceRandomizeTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("(_z_word & harc_z3_bv_value(_ctx, (uint64_t)0x0000000000000003ULL, 64))")
            && cpp.contains(
                "(_z_addr & harc_z3_bv_value(_ctx, (uint64_t)0x0000000000000001ULL, 64))"
            )
            && !cpp.contains("constraint expression not supported in v0 solver path"),
        "bit-slice constraints should lower to solver-width masks; got:\n{cpp}"
    );
}

#[test]
fn randomize_with_wide_bit_slice_constraint_emits_wide_mask() {
    let parsed = parse_source(
        r#"transaction WideChoice
    word : uint<128>
end transaction WideChoice

test WideBitSliceRandomizeTest
    let dut : DummyDut
    run
        let c : WideChoice
        randomize(c) with
            c.word[95:0] == 0
        end randomize
    end run
end test WideBitSliceRandomizeTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("((_harc_u128)0xffffffffULL << 64)")
            && cpp.contains("& harc_z3_bv_value(_ctx,")
            && !cpp.contains("constraint expression not supported in v0 solver path"),
        "wide bit-slice constraints should preserve mask bits above 64; got:\n{cpp}"
    );
}

#[test]
fn randomize_accepts_solve_order_hints_as_metadata() {
    let parsed = parse_source(
        r#"transaction T
    len : uint<8>
    addr : uint<16>
end transaction T

test SolveOrderTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            solve_order(t.addr, t.len)
            t.len > 0
        end randomize
    end run
end test SolveOrderTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("// solve_order(addr, len) accepted as solver scheduling metadata")
            && cpp.contains("// solve-order sampling order: addr, len")
            && cpp.find("_addr = harc_rt::random::harc_prefer_uint(_harc_rt_seed, 0, 16)")
                .unwrap()
                < cpp.find("_len = harc_rt::random::harc_prefer_uint(_harc_rt_seed, 1, 8)")
                    .unwrap()
            && cpp.contains("_s.add(z3::ugt(_z_len, _ctx.bv_val((uint64_t)0, 64)));"),
        "expected solve-order hints to order sampling metadata while ordinary constraints still lower; got:\n{cpp}",
    );
}

#[test]
fn randomize_rejects_bad_solve_order_targets() {
    let parsed = parse_source(
        r#"transaction T
    addr : uint<16>
    len : uint<8>
end transaction T

test BadSolveOrderTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            solve_order(t.addr + 1, t.len)
        end randomize
    end run
end test BadSolveOrderTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("solve_order")
            && err.0.contains("arguments must be fields")
            && err.0.contains("transaction `T`"),
        "expected clear solve-order target diagnostic; got: {}",
        err.0,
    );
}

#[test]
fn randomize_rejects_cyclic_solve_order_hints() {
    let parsed = parse_source(
        r#"transaction T
    addr : uint<16>
    len : uint<8>
end transaction T

test CyclicSolveOrderTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            solve_order(t.addr, t.len)
            solve_order(t.len, t.addr)
        end randomize
    end run
end test CyclicSolveOrderTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("solve-order hints form a cycle"),
        "expected solve-order cycle diagnostic; got: {}",
        err.0,
    );
}

#[test]
fn randomize_rejects_inert_solve_order_fields() {
    let non_random = parse_source(
        r#"transaction T
    !mode : uint<8>
    len : uint<8>
end transaction T

test NonRandomSolveOrderTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            solve_order(t.mode, t.len)
        end randomize
    end run
end test NonRandomSolveOrderTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&non_random).unwrap_err();
    assert!(
        err.0.contains("solve_order")
            && err.0.contains("field `mode` is non-random")
            && err.0.contains("cannot be ordered"),
        "expected non-random solve_order diagnostic; got: {}",
        err.0,
    );

    let pinned = parse_source(
        r#"transaction T
    addr : uint<16>
    len : uint<8>
end transaction T

test PinnedSolveOrderTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            solve_order(t.addr, t.len)
            t.addr == 7
        end randomize
    end run
end test PinnedSolveOrderTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&pinned).unwrap_err();
    assert!(
        err.0.contains("solve_order")
            && err.0.contains("field `addr` is equality-pinned")
            && err.0.contains("cannot affect sampling order"),
        "expected pinned solve_order diagnostic; got: {}",
        err.0,
    );
}

#[test]
fn blocking_randomize_marks_immediate_solver_path() {
    let parsed = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test BlockingRandomizeTest
    let dut : DummyDut
    run
        let t : T
        blocking randomize(t) with
            t.len > 0
        end randomize
    end run
end test BlockingRandomizeTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("// blocking randomize(t) with")
            && cpp.contains("runtime constrained solve callback")
            && cpp.contains("harc_rt::random::HarcSolveMode::Blocking"),
        "blocking randomize should be visible in emitted solver path; got:\n{cpp}",
    );
}

#[test]
fn randomize_rejects_runtime_state_dependencies_by_mode() {
    let queued = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test QueuedRuntimeDepTest
    let dut : DummyDut
    run
        let t : T
        let other : T
        randomize(t) with
            other.len == 1
        end randomize
    end run
end test QueuedRuntimeDepTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&queued).unwrap_err();
    assert!(
        err.0.contains("queued randomize(T)")
            && err.0.contains("runtime state `other.len`")
            && err.0.contains("blocking randomize"),
        "queued randomize should reject live non-target state with guidance; got: {}",
        err.0,
    );

    let blocking = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test BlockingRuntimeDepTest
    let dut : DummyDut
    run
        let t : T
        let other : T
        blocking randomize(t) with
            t.len == other.len
        end randomize
    end run
end test BlockingRuntimeDepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&blocking).expect("blocking runtime dependency emits");
    assert!(
        cpp.contains("_ctx.bv_val((uint64_t)(other.len), 64)")
            && cpp.contains("_z_len == _ctx.bv_val((uint64_t)(other.len), 64)"),
        "blocking randomize should snapshot non-target runtime state into the solver; got:\n{cpp}",
    );
}

#[test]
fn blocking_randomize_snapshots_scalar_let_dependencies() {
    let queued = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test QueuedScalarDepTest
    let dut : DummyDut
    run
        let t : T
        let prev : T
        randomize(prev)
        let target_len : uint<8> = prev.len
        randomize(t) with
            t.len == target_len
        end randomize
    end run
end test QueuedScalarDepTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&queued).unwrap_err();
    assert!(
        err.0.contains("queued randomize(T)")
            && err.0.contains("runtime state `target_len`")
            && err.0.contains("blocking randomize"),
        "queued randomize should reject scalar runtime state with guidance; got: {}",
        err.0,
    );

    let blocking = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test BlockingScalarDepTest
    let dut : DummyDut
    run
        let t : T
        let prev : T
        randomize(prev)
        let target_len : uint<8> = prev.len
        blocking randomize(t) with
            t.len == target_len
        end randomize
    end run
end test BlockingScalarDepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&blocking).expect("blocking scalar dependency emits");
    assert!(
        cpp.contains("_z_len == _ctx.bv_val((uint64_t)(target_len), 64)"),
        "blocking randomize should snapshot typed scalar lets into the solver; got:\n{cpp}",
    );
}

#[test]
fn blocking_randomize_snapshots_non_target_field_paths() {
    let queued = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test QueuedFieldPathDepTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.len <= dut.max_len
        end randomize
    end run
end test QueuedFieldPathDepTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&queued).unwrap_err();
    assert!(
        err.0.contains("queued randomize(T)")
            && err.0.contains("runtime state `dut.max_len`")
            && err.0.contains("blocking randomize"),
        "queued randomize should reject non-target field paths with guidance; got: {}",
        err.0,
    );

    let blocking = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test BlockingFieldPathDepTest
    let dut : DummyDut
    run
        let t : T
        blocking randomize(t) with
            t.len <= dut.max_len
        end randomize
    end run
end test BlockingFieldPathDepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&blocking).expect("blocking field-path dependency emits");
    assert!(
        cpp.contains(
            "z3::ule(_z_len, _ctx.bv_val((uint64_t)(harc_rt::harc_read(dut->max_len)), 64))"
        ),
        "blocking randomize should snapshot non-target field paths into the solver; got:\n{cpp}",
    );
}

#[test]
fn randomize_dist_directives_follow_runtime_dependency_mode() {
    let queued = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test QueuedDistRuntimeDepTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.len dist { [1..dut.max_len] :/ 10 }
        end randomize
    end run
end test QueuedDistRuntimeDepTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&queued).unwrap_err();
    assert!(
        err.0.contains("queued randomize(T)")
            && err.0.contains("runtime state `dut.max_len`")
            && err.0.contains("blocking randomize"),
        "queued randomize should reject runtime-dependent dist directives; got: {}",
        err.0,
    );

    let blocking = parse_source(
        r#"transaction T
    len : uint<8>
end transaction T

test BlockingDistRuntimeDepTest
    let dut : DummyDut
    run
        let t : T
        blocking randomize(t) with
            t.len dist { [1..dut.max_len] :/ 10 }
        end randomize
    end run
end test BlockingDistRuntimeDepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&blocking).expect("blocking dist runtime dependency emits");
    assert!(
        cpp.contains(
            "harc_rt::random::harc_prefer_dist(_harc_rt_seed, 0, {{(int64_t)(1), (int64_t)(harc_rt::harc_read(dut->max_len)), (int64_t)(10)}})"
        ),
        "blocking randomize should snapshot runtime-dependent dist entries; got:\n{cpp}",
    );
}

#[test]
fn randomize_dist_directives_reject_non_random_targets() {
    let parsed = parse_source(
        r#"transaction T
    !mode : uint<8>
    items : list<uint<8>>
    len : uint<8>
end transaction T

test BadDistPolicyTargetTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.mode dist { 1 :/ 10 }
            t.items dist { 1 :/ 10 }
        end randomize
    end run
end test BadDistPolicyTargetTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("dist")
            && err
                .0
                .contains("target `mode` must be a random scalar field")
            && err
                .0
                .contains("target `items` must be a random scalar field"),
        "expected dist policy target diagnostics; got: {}",
        err.0,
    );
}

#[test]
fn randomize_field_attrs_reject_runtime_dependencies() {
    let parsed = parse_source(
        r#"transaction T
    len : uint<8> with [range(1, dut.max_len)] [dist {[1..dut.max_len] :/ 10}]
end transaction T

test AttrRuntimeDepTest
    let dut : DummyDut
    run
        let t : T
        blocking randomize(t) with
            t.len > 0
        end randomize
    end run
end test AttrRuntimeDepTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("field attribute `[range]`")
            && err.0.contains("T.len")
            && err.0.contains("runtime state `dut.max_len`")
            && err.0.contains("field attribute `[dist]`")
            && err.0.contains("blocking randomize"),
        "runtime-dependent field attrs should be rejected as type-level metadata; got: {}",
        err.0,
    );
}

/// `keep f != WRAP` where `WRAP` is an enum variant resolves via
/// the global `enum_variants` map. Without this lookup the
/// constraint translator would error with "unknown name `WRAP`".
#[test]
fn keep_with_enum_variant_resolves_to_numeric_index() {
    let parsed = parse_source(
        r#"enum BurstType { FIXED, INCR, WRAP }

transaction T
    burst : BurstType
    keep burst != WRAP
end transaction T

test EnumKeepTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test EnumKeepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // WRAP is the 3rd variant (index 2). The constraint lowers to
    // _z_burst != bv_val(2, 64). The != comparison emits as plain `!=`
    // (no z3::ult-family wrapper needed for equality).
    assert!(
        cpp.contains("_z_burst != _ctx.bv_val((uint64_t)2, 64)"),
        "expected `burst != WRAP` to lower with WRAP resolved to index 2; got:\n{cpp}"
    );
}

#[test]
fn signed_keep_constraints_use_signed_solver_ops_and_domain() {
    let parsed = parse_source(
        r#"transaction T
    delta : sint<12>
    keep delta >= -8
    keep delta < 8
end transaction T

test SignedKeepTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test SignedKeepTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    assert!(
        cpp.contains(
            "_s.add(_z_delta >= harc_z3_bv_signed_value(_ctx, (int64_t)(-(1LL << 11)), 12, 64));",
        ) && cpp.contains(
            "_s.add(_z_delta <= harc_z3_bv_signed_value(_ctx, (int64_t)((1LL << 11) - 1), 12, 64));",
        ),
        "sint<12> should get a signed 64-bit domain projection; got:\n{cpp}"
    );
    assert!(
        cpp.contains("_z_delta >= -_ctx.bv_val((uint64_t)8, 64)")
            && cpp.contains("_z_delta < _ctx.bv_val((uint64_t)8, 64)"),
        "signed comparisons should use Z3's signed infix operators, not z3::uge/ult; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::expr _eval_delta = _m.eval(_z_delta, true).simplify();")
            && cpp.contains("uint64_t _raw_delta = harc_z3_bv_low_u64(_ctx, _eval_delta);")
            && cpp.contains("int64_t _val_delta = (int64_t)_raw_delta;"),
        "signed fields should materialize model values as int64_t; got:\n{cpp}"
    );
}

#[test]
fn enum_fields_get_solver_domain_constraints() {
    let parsed = parse_source(
        r#"enum Color { RED, GREEN, BLUE }

transaction T
    color : Color
    keep color != RED
end transaction T

test EnumDomainTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test EnumDomainTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    assert!(
        cpp.contains("_s.add(z3::ule(_z_color, _ctx.bv_val((uint64_t)2, 64)));"),
        "3-variant enum should constrain solver values to 0..2; got:\n{cpp}"
    );
    assert!(
        cpp.contains("_z_color != _ctx.bv_val((uint64_t)0, 64)"),
        "enum variant constraints should still resolve variant indexes; got:\n{cpp}"
    );
}

#[test]
fn constraint_solver_pins_non_random_fields_to_current_value() {
    let parsed = parse_source(
        r#"transaction T
    !mode : uint<4> default 3
    val : uint<8>
    keep val > mode
end transaction T

test NonRandomSolverTest
    let dut : DummyDut
    run
        let t : T
        t.mode = 5
        randomize(t)
    end run
end test NonRandomSolverTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    assert!(
        cpp.contains("_s.add(_z_mode == harc_z3_bv_value(_ctx, t.mode, 64));"),
        "non-random fields should be pinned to the current transaction value in the solver; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("_solver_site_") || !cpp.contains("_mode;"),
        "non-random fields should not get unique-history declarations; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("uint64_t _val_mode =")
            && !cpp.contains("int64_t _val_mode =")
            && !cpp.contains("t.mode = _val_mode;")
    );
    assert!(
        !cpp.contains("_val_mode"),
        "non-random fields should not be assigned from the solver model or unique history; got:\n{cpp}"
    );
    assert!(
        cpp.contains("uint64_t _val_val ="),
        "random fields should still be assigned from the model; got:\n{cpp}"
    );
}

#[test]
fn when_subtype_keeps_lower_as_guarded_solver_constraints() {
    let parsed = parse_source(
        r#"enum Op { READ, WRITE }

transaction T
    op : Op
    len : uint<8>

    when op == WRITE
        keep len > 4
    end when
end transaction T

test WhenKeepTest
    let dut : DummyDut
    run
        let t : T
        randomize(t)
    end run
end test WhenKeepTest"#,
    )
    .unwrap();
    assert!(
        cpp_tb::uses_constraint_solver(&parsed),
        "bare randomize should link Z3 when keeps live under a when subtype"
    );
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    assert!(
        cpp.contains("!((_z_op == _ctx.bv_val((uint64_t)1, 64)))")
            && cpp.contains(" || z3::ugt(_z_len, _ctx.bv_val((uint64_t)4, 64))"),
        "when keep should lower as `!guard || keep`; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("randomize_T(&t);"),
        "bare randomize should not bypass guarded keeps; got:\n{cpp}"
    );
}

#[test]
fn when_subtype_fields_assign_only_when_branch_active() {
    let parsed = parse_source(
        r#"enum Op { READ, WRITE }

transaction T
    op : Op

    when op == WRITE
        wdata : uint<4>
        keep wdata > 20
    end when

    when op == READ
        rdata : uint<4>
        keep rdata == 3
    end when
end transaction T

test WhenSubtypeFieldTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.op == READ
        end randomize
    end run
end test WhenSubtypeFieldTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("uint64_t wdata = 0;")
            && cpp.contains("uint64_t rdata = 0;")
            && cpp.contains("if (t.op == 0) {   // active when-subtype field rdata")
            && cpp.contains("t.rdata = _val_rdata;")
            && cpp.contains("if (t.op == 1) {   // active when-subtype field wdata")
            && cpp.contains("t.wdata = _val_wdata;"),
        "when subtype fields should exist but assign only under their active guard; got:\n{cpp}",
    );
    assert!(
        cpp.contains("!((_z_op == _ctx.bv_val((uint64_t)1, 64)))")
            && cpp.contains(" || z3::ugt(_z_wdata, _ctx.bv_val((uint64_t)20, 64))")
            && cpp.contains("!((_z_op == _ctx.bv_val((uint64_t)0, 64)))")
            && cpp.contains(" || _z_rdata == _ctx.bv_val((uint64_t)3, 64)"),
        "when subtype keeps should remain branch-guarded so inactive impossible constraints do not make the solve UNSAT; got:\n{cpp}",
    );
}

#[test]
fn when_subtype_unsat_diagnostics_name_branch_guards() {
    let parsed = parse_source(
        r#"enum Op { READ, WRITE }

transaction T
    op : Op
    len : uint<4>

    when op == WRITE
        keep len > 20
    end when
end transaction T

test WhenSubtypeUnsatDiagnosticTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.op == WRITE
        end randomize
    end run
end test WhenSubtypeUnsatDiagnosticTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("active when subtype guard `op == WRITE` participated in the solve"),
        "UNSAT diagnostics should name when subtype guards that contributed branch constraints; got:\n{cpp}",
    );
}

#[test]
fn randomize_unsat_diagnostics_name_constraint_origins() {
    let parsed = parse_source(
        r#"transaction T
    len : uint<4> with [range(0, 3)]
    keep len > 4
end transaction T

test UnsatOriginDiagnosticTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.len == 2
        end randomize
    end run
end test UnsatOriginDiagnosticTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("harc_rt::random::harc_solve_status_unsat")
            && cpp.contains("return _harc_rt_status;")
            && cpp.contains("return harc_rt::random::harc_solve_status_ok();")
            && cpp.contains("harc_rt::random::harc_handle_solve_status(_harc_rt_solve_status);")
            && cpp.contains(
                "sim_log_line(\"FAIL\", \"%s\", _harc_rt_status.message ? _harc_rt_status.message : \"randomize(t) with: constraint UNSAT\");"
            )
            && cpp.contains("randomize(t) with: constraint UNSAT")
            && cpp.contains("z3::context _ctx;"),
        "UNSAT should construct a runtime status while preserving inline Z3 and message; got:\n{cpp}",
    );
    assert!(
        cpp.contains("constraint `len > 4` participated in the solve")
            && cpp.contains("constraint `t.len == 2` participated in the solve")
            && cpp.contains("field attribute `[range]` on `T.len` participated in the solve"),
        "UNSAT diagnostics should list participating keep/with/range origins; got:\n{cpp}",
    );
}

#[test]
fn when_subtype_field_range_attributes_are_branch_guarded() {
    let parsed = parse_source(
        r#"enum Op { READ, WRITE }

transaction T
    op : Op

    when op == WRITE
        wdata : uint<4> with [range(8, 15)]
    end when
end transaction T

test WhenSubtypeRangeAttrTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.op == READ
        end randomize
    end run
end test WhenSubtypeRangeAttrTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("_s.add(!(_z_op == _ctx.bv_val((uint64_t)1, 64)) || z3::uge(_z_wdata, _ctx.bv_val((uint64_t)8, 64)) && z3::ule(_z_wdata, _ctx.bv_val((uint64_t)15, 64)));")
            && !cpp.contains("_s.add(z3::uge(_z_wdata, _ctx.bv_val((uint64_t)8, 64)) && z3::ule(_z_wdata, _ctx.bv_val((uint64_t)15, 64)));"),
        "when subtype [range] attributes should only constrain the active branch; got:\n{cpp}",
    );
}

#[test]
fn when_subtype_list_fields_assign_only_when_branch_active() {
    let parsed = parse_source(
        r#"transaction T
    enabled : bool

    when enabled
        items : list<uint<8>>
        keep items.len() <= 2
        keep for item in items
            item > 7
        end for
    end when
end transaction T

test WhenSubtypeListFieldTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with
            t.enabled == false
        end randomize
    end run
end test WhenSubtypeListFieldTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("std::vector<uint64_t> items = {};")
            && cpp.contains("if (t.enabled) {   // active when-subtype field items")
            && cpp.contains("t.items.resize((size_t)_raw_items_len);")
            && cpp.contains("t.items[0] = (uint64_t)_raw_items_0;"),
        "when subtype list fields should exist but assign only under their active guard; got:\n{cpp}",
    );
}

#[test]
fn solver_include_detection_walks_tseq_and_component_bodies() {
    let tseq_parsed = parse_source(
        r#"transaction T
    addr : uint<8>
end transaction T

tseq Gen(n: int) -> TSeq<T>
    for _ in 0 .. n
        let t : T
        randomize(t)
        yield t
    end for
end tseq Gen"#,
    )
    .unwrap();
    assert!(
        cpp_tb::uses_constraint_solver(&tseq_parsed),
        "natural numeric endpoint auto-coverage makes bare randomize in tseq require Z3"
    );

    let component_parsed = parse_source(
        r#"transaction T
    addr : uint<8>
end transaction T

agent A
    hookable drive()
        let t : T
        randomize(t)
    end drive
end agent A"#,
    )
    .unwrap();
    assert!(
        cpp_tb::uses_constraint_solver(&component_parsed),
        "Z3 include detection should walk component hookable/function bodies"
    );
}

/// `randomize(t) with R(t)` inlines `R`'s body into the Z3 solver
/// block (spec §4.2). Block-form relations contribute one constraint
/// per body expression; the formal parameter substitutes for the
/// actual call argument so the constraints reference the right
/// fields.
#[test]
fn block_relation_inlines_into_randomize_with() {
    let parsed = parse_source(
        r#"transaction T
    addr : uint<32>
    len  : uint<8>
end transaction T

relation Bounded(x: T)
    x.len in [1..16]
    x.addr % 4 == 0
end relation Bounded

test BlockRelTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with Bounded(t) end randomize
    end run
end test BlockRelTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Both relation body expressions reach the solver.
    assert!(
        cpp.contains("z3::uge(_z_len") && cpp.contains("z3::ule(_z_len"),
        "expected `x.len in [1..16]` to lower to uge/ule pair after inlining; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::urem(_z_addr"),
        "expected `x.addr % 4 == 0` to lower with urem after inlining; got:\n{cpp}"
    );
}

/// Alias-form relations (`relation A(t) = expr`) contribute their
/// single expression as one constraint, with parameter substitution.
/// Also exercises recursive expansion when the alias body itself
/// calls another relation.
#[test]
fn alias_relation_inlines_and_recurses_through_other_relations() {
    let parsed = parse_source(
        r#"transaction T
    addr : uint<32>
end transaction T

relation Aligned(x: T) = x.addr % 4 == 0
relation HighHalf(x: T) = x.addr >= 0x80000000
relation BothAlignedAndHigh(x: T) = Aligned(x) && HighHalf(x)

test AliasRelTest
    let dut : DummyDut
    run
        let t : T
        randomize(t) with BothAlignedAndHigh(t) end randomize
    end run
end test AliasRelTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // The alias `BothAlignedAndHigh(t)` should expand to
    // `(Aligned(t) && HighHalf(t))`, then each sub-relation expands
    // to its body. The Aligned check uses urem; HighHalf uses uge.
    // Both should reach the solver in the same _s.add call (since
    // the alias produces ONE constraint expression that is the &&
    // of the two sub-relation bodies).
    assert!(
        cpp.contains("z3::urem(_z_addr"),
        "expected recursively-inlined `Aligned(x)` to add urem; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::uge(_z_addr"),
        "expected recursively-inlined `HighHalf(x)` to add uge; got:\n{cpp}"
    );
    // The inlined alias still appears as a single constraint joined
    // by &&, not two separate ones — that's the alias-form
    // contract (Block form would produce two _s.add calls).
    let urem_count = cpp.matches("z3::urem(_z_addr").count();
    assert_eq!(
        urem_count, 1,
        "expected exactly one urem from the alias-form `Aligned`; got {urem_count} in:\n{cpp}"
    );
}

/// Parameter substitution works when the relation's formal parameter
/// has a different name than the randomize target. `randomize(pkt) with
/// Bounded(pkt)` — inside `Bounded`, the parameter is `x`, and
/// references to `x.<field>` should substitute to `pkt.<field>` …
/// which, after the substitution, the constraint translator handles
/// like any other field access on the randomize target.
#[test]
fn relation_inlining_substitutes_formal_param_for_argument() {
    let parsed = parse_source(
        r#"transaction Pkt
    size : uint<8>
end transaction Pkt

relation Small(x: Pkt)
    x.size <= 4
end relation Small

test SubstTest
    let dut : DummyDut
    run
        let pkt : Pkt
        randomize(pkt) with Small(pkt) end randomize
    end run
end test SubstTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // After substitution, `x.size` becomes `pkt.size`, which the
    // constraint translator lowers to `_z_size` (the Z3 var of the
    // transaction's size field). No spurious `_z_x_size` symbol.
    assert!(
        cpp.contains("z3::ule(_z_size,"),
        "expected substituted-then-translated `size <= 4` constraint; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("_z_x"),
        "no Z3 var named after the formal param should appear; got:\n{cpp}"
    );
}

/// `extern function name(params) -> ret` (spec §9) emits a C-linkage
/// forward declaration at file scope wrapped in `extern "C" { ... }`,
/// so the user's `--ref-src <file>` implementation links against it.
/// Call sites use the existing function-call lowering path.
#[test]
fn extern_function_emits_extern_c_forward_decl() {
    let parsed = parse_source(
        r#"extern function ref_crc8_step(crc: uint<8>, byte: uint<8>) -> uint<8>

test ExternTest
    let dut : DummyDut
    run
        let c = ref_crc8_step(0xFF, 0x42)
        assert c == ref_crc8_step(0xFF, 0x42)
    end run
end test ExternTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Forward declaration block at file scope.
    assert!(
        cpp.contains("extern \"C\" {"),
        "expected `extern \"C\" {{` wrapper for extern fns; got:\n{cpp}"
    );
    // Signature: HARC widens narrow ints to uint64_t at the FFI boundary.
    assert!(
        cpp.contains("uint64_t ref_crc8_step(uint64_t crc, uint64_t byte);"),
        "expected widened C-linkage forward decl; got:\n{cpp}"
    );
    // The forward decl appears OUTSIDE main() (before `int main(`).
    let extern_pos = cpp.find("uint64_t ref_crc8_step(uint64_t").unwrap();
    let main_pos = cpp.find("int main(").unwrap();
    assert!(extern_pos < main_pos,
        "extern fn decl must be at file scope (before main); got extern at {extern_pos}, main at {main_pos}");
    // Call sites lower as plain function calls — no special wrapping.
    assert!(
        cpp.contains("ref_crc8_step(255, 66)")
            || cpp.contains("ref_crc8_step(0xFF, 0x42)")
            || cpp.contains("ref_crc8_step(") && cpp.contains(")"),
        "expected plain function-call lowering at call sites; got:\n{cpp}"
    );
}

/// A file with no `extern function` declarations emits no `extern "C" {`
/// block — the wrapper only appears when needed.
#[test]
fn no_extern_function_means_no_extern_c_block() {
    let parsed = parse_source(
        r#"test T
    let dut : DummyDut
    run
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        !cpp.contains("extern \"C\" {"),
        "no extern fns should mean no extern \"C\" block; got:\n{cpp}"
    );
}

/// Smoke-sweep every fixture under `tests/fixtures/` through
/// `cpp_tb::emit`. Fixtures missing a sibling `_sim.harc` half are
/// auto-paired (e.g. `counter_test.harc` + `counter_test_sim.harc`);
/// the rest go through `emit` standalone. Anything that emits without
/// error must continue to emit without error after the heartbeat-
/// foundation changes — this catches any case where the new bump
/// sites accidentally reference an out-of-scope instance.
///
/// Failures are reported as a single aggregated panic at the end so
/// one bad fixture doesn't mask issues in others.
#[test]
fn all_fixtures_emit_cleanly() {
    let fixtures = std::path::Path::new("tests/fixtures");
    let mut paths: Vec<_> = std::fs::read_dir(fixtures)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("harc"))
        .collect();
    paths.sort();

    let mut failures: Vec<String> = Vec::new();
    let mut emitted = 0usize;
    for path in &paths {
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
        // Skip the `_sim.harc` halves — they're picked up via their
        // sibling base file's merge.
        if name.ends_with("_sim.harc") || name.ends_with("_domains.harc") {
            continue;
        }
        let src = std::fs::read_to_string(path).unwrap();
        let parsed = match parse_source(&src) {
            Ok(p) => p,
            Err(e) => {
                failures.push(format!("[parse] {name}: {e:?}"));
                continue;
            }
        };
        // Try sibling sim half if present.
        let sim_sibling =
            path.with_file_name(format!("{}_sim.harc", name.trim_end_matches(".harc")));
        let parsed_units = if sim_sibling.exists() {
            let sim_src = std::fs::read_to_string(&sim_sibling).unwrap();
            match parse_source(&sim_src) {
                Ok(sim) => vec![parsed.clone(), sim],
                Err(_) => vec![parsed.clone()],
            }
        } else {
            vec![parsed.clone()]
        };
        let to_emit = match merge::merge_for_sim(parsed_units, None) {
            Ok(m) => m,
            Err(_) => parsed.clone(),
        };
        match cpp_tb::emit(&to_emit) {
            Ok(_) => emitted += 1,
            // Fixtures that legitimately error (no test / no sim impl /
            // missing DUT) are skipped silently — those error paths
            // aren't part of what this sweep is checking.
            Err(e) => {
                let msg = e.0;
                // Benign error classes — these fixtures depend on
                // external declarations (`use BusAxiLite` brings in a
                // sibling bus decl, multi-clock fixtures rely on a
                // separate `domain` file) that aren't in scope when
                // emitting the fixture standalone.
                let benign = msg.contains("no `test` declaration")
                    || msg.contains("let dut")
                    || msg.contains("only non-sim impls")
                    || msg.contains("no `impl sim`")
                    || msg.contains("multiple tests")
                    || msg.contains("is not a known bus binding")
                    || msg.contains("no `domain") && msg.contains("declaration was found")
                    || msg.contains("randomize(") && msg.contains("no `transaction")
                    // axi_agent.harc references enum variants (READ /
                    // WRITE / WRAP / INCR / FIXED) declared in
                    // arc.stdlib.BusAxi4. Standalone emit-sweep
                    // can't resolve them; the real `harc sim`
                    // invocation imports them via `use`.
                    || msg.contains("constraint references unknown name")
                    // A suspending bus/TLM method call inside a `log`/`fail`
                    // message interpolation is a TB-IR-only capability
                    // (#494 P2d follow-up): the legacy v1 emitter cannot
                    // resolve a bus-method call in a message and fails with
                    // "bus ... has no signal or channel named <method>".
                    // These fixtures are exercised via the default (tbir)
                    // backend in tests/run_fixtures.sh; the v1 emit sweep
                    // legitimately can't emit them.
                    || (msg.contains("has no signal or channel named")
                        && name.starts_with("msg_suspending_call"))
                    // A direct non-fork call to an out_of_order TLM method is
                    // intentionally TBIR-only while v1 is retired. The
                    // default fixture runner exercises it through TBIR; this
                    // legacy-v1 standalone emit sweep must not treat that
                    // documented backend gap as a fixture regression.
                    || (name == "tlm_direct_ooo_bus_test.harc"
                        && msg.contains("supports only `blocking` tlm_method calls"));
                if !benign {
                    failures.push(format!("[emit] {name}: {msg}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "fixture sweep: {} emitted, {} failed:\n{}",
        emitted,
        failures.len(),
        failures.join("\n")
    );
    // Sanity: at least a substantial fraction of fixtures should have
    // gone through emit; otherwise the skip filter is too aggressive.
    assert!(
        emitted >= 20,
        "fixture sweep only emitted {emitted} files — skip filter too aggressive?"
    );
}

/// Casts to non-Builtin types (struct, named) drop to identity at
/// codegen time. The cast is purely a HARC-level type assertion;
/// the C++ representation doesn't change.
#[test]
fn cast_to_named_type_is_identity_in_cpp() {
    let parsed = parse_source(
        r#"struct Pkt
    addr : uint<32>
    data : uint<32>
end struct Pkt

test T
    let dut : DummyDut
    run
        let raw : uint<64> = 0xDEAD_BEEF_CAFE_BABE
        let pkt = raw as Pkt
        dut.X = pkt.addr
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");

    // No `(Pkt)` C++ cast should appear — the user-level cast to a
    // struct is identity at the HARC C++ TB layer (struct field
    // access still uses `.addr`, which works on the underlying value).
    assert!(
        !cpp.contains("(Pkt)("),
        "expected NO `(Pkt)(...)` C++ cast for struct-targeted `as`; got:\n{cpp}"
    );
}

#[test]
fn structs_and_transactions_share_record_lowering() {
    let parsed = parse_source(
        r#"struct Header
    addr : uint<8>
    tag : uint<4>
end struct Header

transaction Packet
    hdr : Header
    len : uint<8>
end transaction Packet

test SharedRecordLoweringTest
    let dut : DummyDut
    run
        let h : Header
        randomize(h)
        let p : Packet
        randomize(p)
        dut.addr = h.addr + p.hdr.addr
    end run
end test SharedRecordLoweringTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("struct Header {") && cpp.contains("struct Packet {"),
        "structs and transactions should both emit value records; got:\n{cpp}"
    );
    assert!(
        cpp.contains("Header hdr = {};"),
        "transaction fields should use emitted struct record types; got:\n{cpp}"
    );
    assert!(
        cpp.contains("inline bool operator==(const Header& a, const Header& b)")
            && cpp.contains("inline bool operator==(const Packet& a, const Packet& b)"),
        "shared record lowering should emit equality for both structs and transactions; got:\n{cpp}"
    );
    assert!(
        cpp.contains("static void randomize_Header(Header* t, harc_rt::random::HarcRng& harc_rng)")
            && cpp.contains("randomize_Header(&t->hdr, harc_rng);"),
        "record randomization should be shared and recurse into nested record fields; got:\n{cpp}"
    );
}

#[test]
fn nested_record_constraints_flatten_into_solver_fields() {
    let parsed = parse_source(
        r#"struct Header
    addr : uint<8>
    keep addr % 4 == 0
end struct Header

transaction Packet
    hdr : Header
    len : uint<8>
    keep hdr.addr < 64
end transaction Packet

test NestedRecordConstraintTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p) with
            p.hdr.addr >= 16
        end randomize
    end run
end test NestedRecordConstraintTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_hdr_addr = _ctx.bv_const(\"hdr_addr\", 64);"),
        "nested record scalar should be flattened to a Z3 field; got:\n{cpp}"
    );
    assert!(
        cpp.contains("z3::urem(_z_hdr_addr, _ctx.bv_val((uint64_t)4, 64))")
            && cpp.contains("z3::ult(_z_hdr_addr, _ctx.bv_val((uint64_t)64, 64))")
            && cpp.contains("z3::uge(_z_hdr_addr, _ctx.bv_val((uint64_t)16, 64))"),
        "struct keep, transaction keep, and randomize-with constraints should all target the flattened field; got:\n{cpp}"
    );
    assert!(
        cpp.contains("p.hdr.addr = _val_hdr_addr;"),
        "solver model should write back into the nested record field; got:\n{cpp}"
    );
}

#[test]
fn nested_record_list_constraints_use_flattened_solver_paths() {
    let parsed = parse_source(
        r#"struct Payload
    items : list<uint<8>>
    keep items.len() >= 1
    keep items.len() <= 4
    keep for item in items
        item > 0
    end for
end struct Payload

transaction Packet
    payload : Payload
    total : uint<10>
    keep sum(payload.items[0..payload.items.len()]) == total
end transaction Packet

test NestedRecordListConstraintTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p) with
            p.total == 7
        end randomize
    end run
end test NestedRecordListConstraintTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("z3::expr _z_payload_items_len = _ctx.bv_const(\"payload_items_len\", 64);")
            && cpp
                .contains("z3::expr _z_payload_items_3 = _ctx.bv_const(\"payload_items_3\", 64);"),
        "nested list fields should use flattened, C-safe solver variables; got:\n{cpp}"
    );
    assert!(
        cpp.contains(
            "z3::ule(_z_payload_items_len, _ctx.bv_val((uint64_t)0, 64)) || (z3::ugt(_z_payload_items_0"
        ) && cpp.contains("z3::ugt(_z_payload_items_len, _ctx.bv_val((uint64_t)0, 64))"),
        "nested foreach and list-sum constraints should lower through flattened list paths; got:\n{cpp}"
    );
    assert!(
        cpp.contains("p.payload.items.resize((size_t)_raw_payload_items_len);"),
        "solver model should write nested list length back through the record path; got:\n{cpp}"
    );
}

#[test]
fn nested_record_solve_order_and_pins_use_dotted_field_paths() {
    let parsed = parse_source(
        r#"struct Header
    tag : uint<8> with [unique within test]
end struct Header

transaction Packet
    hdr : Header
    other : uint<8>
end transaction Packet

test NestedRecordSolveOrderPinTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p) with
            solve_order(p.hdr.tag, p.other)
            p.hdr.tag == 3
        end randomize
    end run
end test NestedRecordSolveOrderPinTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("solve_order")
            && err.0.contains("field `hdr.tag` is equality-pinned")
            && err.0.contains("cannot affect sampling order"),
        "pinned nested solve_order fields should get a clear diagnostic; got: {}",
        err.0,
    );
}

#[test]
fn nested_record_auto_coverage_uses_c_safe_identifiers_and_dotted_labels() {
    let parsed = parse_source(
        r#"struct Header
    tag : uint<8> with [unique within test]
end struct Header

transaction Packet
    hdr : Header
end transaction Packet

test NestedRecordAutoCoverageTest
    let dut : DummyDut
    run
        let p : Packet
        randomize(p)
    end run
end test NestedRecordAutoCoverageTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("_auto_cov_labels_") && cpp.contains("_hdr_tag[]"),
        "nested auto coverage should use flattened C identifiers; got:\n{cpp}"
    );
    assert!(
        cpp.contains("Packet.hdr.tag=0") && !cpp.contains("hdr.tag["),
        "nested auto coverage reports should keep dotted field labels while generated identifiers stay C-safe; got:\n{cpp}"
    );
}

#[test]
fn connect_event_to_hookable_sinks_emits_direct_fanout_calls() {
    let parsed = parse_source(
        r#"transactor Source
    observed : out event<uint<8>>

    hookable publish(v: uint<8>)
        emit observed(v)
    end publish
end transactor Source

scoreboard AnalysisSb
    count : uint<32> default 0

    hookable write_obs(v: uint<8>)
        count = count + 1
    end write_obs
end scoreboard AnalysisSb

scoreboard AnalysisCov
    samples : uint<32> default 0

    hookable sample_obs(v: uint<8>)
        samples = samples + 1
    end sample_obs
end scoreboard AnalysisCov

env AnalysisEnv
    source : Source passive
    sb     : AnalysisSb
    cov    : AnalysisCov

    connect
        source.observed -> sb.write_obs
        source.observed -> cov.sample_obs
    end connect
end env AnalysisEnv

test AnalysisSinkConnectTest
    let dut : Top
    let env : AnalysisEnv
    run
        env.source.publish(3)
    end run
end test AnalysisSinkConnectTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains(
            "env.source.observed.push_back([&](auto _t) { AnalysisSb_write_obs(env.sb, _t); });"
        ),
        "connect should lower event -> scoreboard sink as a direct method bridge; got:\n{cpp}"
    );
    assert!(
        cpp.contains(
            "env.source.observed.push_back([&](auto _t) { AnalysisCov_sample_obs(env.cov, _t); });"
        ),
        "connect should lower event -> coverage-style sink as a direct method bridge; got:\n{cpp}"
    );
}

#[test]
fn connect_to_when_active_hookable_on_passive_instance_errors_clearly() {
    let parsed = parse_source(
        r#"transactor Source
    observed : out event<uint<8>>
end transactor Source

transactor Sink
    when active
        hookable write_obs(v: uint<8>)
        end write_obs
    end when
end transactor Sink

env AnalysisEnv
    source : Source passive
    sink   : Sink passive

    connect
        source.observed -> sink.write_obs
    end connect
end env AnalysisEnv

test AnalysisSinkConnectPassiveErrorTest
    let dut : Top
    let env : AnalysisEnv
    run
        wait 1 cycle
    end run
end test AnalysisSinkConnectPassiveErrorTest"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("connect")
            && err.0.contains("env.sink.write_obs")
            && err.0.contains("when active")
            && err.0.contains("transactor `Sink`")
            && err.0.contains("Sink active"),
        "expected connect passive error naming env.sink.write_obs, when active, Sink, and the fix; got: {}",
        err.0,
    );
}

// ── Passive-transactor enforcement ──────────────────────────────────────
//
// A transactor's always-on body (anything NOT under `when active`) must
// not drive DUT signals. Drive-side hookables / on-handlers must live
// inside `when active { ... }` so a passive instance — whose
// `when_active` body is elided at codegen — cannot end up driving the
// bus. See spec §8.1 and src/codegen/cpp_tb.rs
// `check_transactor_no_drive_in_always_on_body`.

/// Direct DUT-pointer drive (`dut.<port> = ...`) inside an always-on
/// hookable surfaces a HARC-level error naming the transactor, the
/// hookable, the offending signal, and the recommended fix.
#[test]
fn transactor_always_on_dut_write_errors_clearly() {
    let parsed = parse_source(
        r#"transactor X
    dut : SomeDut

    hookable write(v : uint<32>)
        dut.addr = v
    end write
end transactor X

test T
    let dut : SomeDut
    let x : X passive
    run
    end run
end test T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("transactor `X`")
            && err.0.contains("hookable `write`")
            && err.0.contains("dut.addr")
            && err.0.contains("when active"),
        "expected drive-in-always-on error pointing at X.write + dut.addr + when active; got: {}",
        err.0,
    );
}

/// Same enforcement for `bound to BusType` transactors: writing
/// `bus.<ch>.<sig>` or calling `bus.<ch>.send(...)` in the always-on
/// body is a drive.
#[test]
fn transactor_always_on_bus_send_errors_clearly() {
    let parsed = parse_source(
        r#"bus B
    handshake_channel ch: send kind: valid_ready
        data: uint<32>;
    end handshake_channel ch
end bus B

transactor X bound to B
    hookable write(v : uint<32>)
        bus.ch.send(v)
    end write
end transactor X

test T
    let dut : SomeDut
    let b : B = bind dut
    let x : X passive = bind b
    run
    end run
end test T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("transactor `X`")
            && err.0.contains("hookable `write`")
            && err.0.contains("bus.ch.send")
            && err.0.contains("when active"),
        "expected drive-in-always-on error pointing at X.write + bus.ch.send + when active; got: {}",
        err.0,
    );
}

/// Positive case: identical transactor with the drive code moved into
/// `when active { ... }` emits cleanly. Proves the fix the error
/// message recommends actually works.
#[test]
fn transactor_when_active_drive_emits_cleanly() {
    let parsed = parse_source(
        r#"transactor X
    dut : SomeDut

    when active
        hookable write(v : uint<32>)
            dut.addr = v
        end write
    end when
end transactor X

test T
    let dut : SomeDut
    let x : X active
    run
    end run
end test T"#,
    )
    .unwrap();
    cpp_tb::emit(&parsed)
        .expect("drive code inside `when active` should emit cleanly under any mode");
}

/// Call-site enforcement (spec §8.1, Phase B): even after the
/// structural check moves drive code into `when active`, a passive
/// instance still has those C++ functions emitted (only the actor
/// coroutine is gated by mode). A direct call like
/// `passive_inst.write(...)` would silently dispatch into orphan code.
/// This test pins the error message.
#[test]
fn passive_instance_calling_when_active_hookable_errors_clearly() {
    let parsed = parse_source(
        r#"transactor X
    dut : SomeDut

    when active
        hookable write(v : uint<32>)
            dut.addr = v
        end write
    end when
end transactor X

test T
    let dut : SomeDut
    let x : X passive
    run
        x.write(42)
    end run
end test T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("x.write")
            && err.0.contains("when active")
            && err.0.contains("transactor `X`")
            && err.0.contains("X active"),
        "expected call-site passive error naming x.write, when active, X, and the fix; got: {}",
        err.0,
    );
}

/// Mode inheritance through env composition: `let e : E passive`
/// where `E { drv : X }` makes `e.drv` passive even though no field-
/// level annotation is present. A call to `e.drv.write(...)` (where
/// `write` lives in `X.when_active`) must surface the same error.
#[test]
fn passive_instance_through_env_inheritance_errors_clearly() {
    let parsed = parse_source(
        r#"transactor X
    dut : SomeDut

    when active
        hookable write(v : uint<32>)
            dut.addr = v
        end write
    end when
end transactor X

env E
    drv : X
end env E

test T
    let dut : SomeDut
    let e : E passive
    run
        e.drv.write(42)
    end run
end test T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("e.drv.write")
            && err.0.contains("when active")
            && err.0.contains("transactor `X`"),
        "expected env-inherited passive error naming e.drv.write + when active + X; got: {}",
        err.0,
    );
}

/// Positive case: the same call on an `active` instance compiles
/// cleanly. Proves the fix the error message recommends actually
/// works end-to-end (active mode dispatches into `when_active`
/// methods normally).
#[test]
fn active_instance_calling_when_active_hookable_emits_cleanly() {
    let parsed = parse_source(
        r#"transactor X
    dut : SomeDut

    when active
        hookable write(v : uint<32>)
            dut.addr = v
        end write
    end when
end transactor X

test T
    let dut : SomeDut
    let x : X active
    run
        x.write(42)
    end run
end test T"#,
    )
    .unwrap();
    cpp_tb::emit(&parsed)
        .expect("active instance calling a when-active hookable should emit cleanly");
}

/// A `passive` instance calling an *always-on* hookable (one declared
/// in `T.items`, not under `when active`) must keep working — that's
/// the observer-helper shape (e.g. a scoreboard-mutating helper
/// callable from observation handlers). Only `when active` methods
/// are gated by the call-site check.
#[test]
fn passive_instance_calling_always_on_hookable_emits_cleanly() {
    let parsed = parse_source(
        r#"scoreboard S
    count : uint<32> default 0
end scoreboard S

transactor X
    sb : S

    hookable bump()
        sb.count = sb.count + 1
    end bump
end transactor X

test T
    let dut : SomeDut
    let x : X passive
    run
        x.bump()
    end run
end test T"#,
    )
    .unwrap();
    cpp_tb::emit(&parsed)
        .expect("passive instance calling a non-drive always-on hookable should emit cleanly");
}

// ── Testbench-block `function` methods ─────────────────────────────────
//
// docs/test-ergonomics.md §3. A `testbench` body now accepts
// `function name(...) [-> T] ... end function name` declarations
// (non-hookable methods). Codegen reuses the existing hookable lambda
// path but suppresses per-method pre/post hook vectors and the
// corresponding fan-out — i.e. no `<Type>_<method>_pre` /
// `<Type>_<method>_post` symbols in the emitted C++.

#[test]
fn testbench_function_emits_method_lambda_without_hook_vectors() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top

    function reset()
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
    end function reset
end testbench Tb

test T
    let dut : Top
    let tb  : Tb
    run
        tb.dut = dut
        tb.reset()
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("testbench + function lowers cleanly");
    assert!(
        cpp.contains("Tb_reset"),
        "expected `Tb_reset` method lambda; got: {}",
        &cpp[..400.min(cpp.len())],
    );
    assert!(
        !cpp.contains("Tb_reset_pre"),
        "non-hookable `function` should NOT emit Tb_reset_pre hook vector; got:\n{}",
        cpp,
    );
    assert!(
        !cpp.contains("Tb_reset_post"),
        "non-hookable `function` should NOT emit Tb_reset_post hook vector; got:\n{}",
        cpp,
    );
}

/// `impl <name> for <Tb>` (docs/test-ergonomics.md §3.3) binds a
/// test to a testbench. The pre-emission desugaring synthesizes
/// `let dut : <SVType>` + `let _tb : <TbType>` at test scope, wires
/// `_tb.dut = dut`, and rewrites bare-name references to testbench
/// fields / methods into `_tb.<x>` accesses / `<TbType>_<m>(_tb,...)`
/// dispatches. Result: the bound test threads through the same
/// codegen as a classic `test T { ... }` after desugaring.
#[test]
fn impl_for_testbench_emits_per_test_tb_instance_and_wires_dut() {
    let parsed = parse_source(
        r#"testbench TopTb
    dut : Top

    function reset()
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
    end function reset
end testbench TopTb

impl Smoke for TopTb
    run
        reset()
        assert dut.count_out == 0
    end run
end impl Smoke"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("impl form lowers cleanly");
    // The synthesized `_tb` instance must appear in main()'s scope
    // (default-constructed TopTb struct).
    assert!(
        cpp.contains("TopTb _tb"),
        "expected `TopTb _tb;` instantiation; got first 600 chars:\n{}",
        &cpp[..600.min(cpp.len())],
    );
    // The DUT auto-wire (`_tb.dut = dut`) lands as the first stmt of
    // the run block.
    assert!(
        cpp.contains("_tb.dut = dut"),
        "expected `_tb.dut = dut` wire-up; got:\n{}",
        cpp,
    );
    // Bare `reset()` rewrote to `TopTb_reset(_tb)` via the
    // testbench-method-dispatch path.
    assert!(
        cpp.contains("TopTb_reset(_tb"),
        "expected `TopTb_reset(_tb...)` method dispatch; got:\n{}",
        cpp,
    );
    // Bare `dut.count_out` stayed bare (refers to the synthesized
    // test-scope `let dut : Top`, lowered through the existing
    // pointer-var path as `dut->count_out`).
    assert!(
        cpp.contains("dut->count_out"),
        "expected `dut->count_out` from bare `dut.count_out`; got:\n{}",
        cpp,
    );
}

#[test]
fn impl_for_testbench_does_not_alias_fields_over_generated_locals() {
    let parsed = parse_source(
        r#"scoreboard ResponseScoreboard
    count : uint<32> default 0
end scoreboard ResponseScoreboard

testbench Tb
    dut : DummyDut
    errors : ResponseScoreboard
end testbench Tb

impl AliasCollisionTest for Tb
    run
        wait 1 cycle
        assert errors.count == 0 else fail("unexpected scoreboard count")
    end run
end impl AliasCollisionTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("int errors = 0;"),
        "expected generated error counter to keep its existing name; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("auto& errors = _tb.errors;"),
        "testbench fields must not alias over generated locals; got:\n{cpp}"
    );
    assert!(
        cpp.contains("_tb.errors.count"),
        "testbench field references should be rewritten through _tb; got:\n{cpp}"
    );
}

#[test]
fn component_method_returns_struct_value() {
    let parsed = parse_source(
        r#"struct ReadResponse
    matched : uint<1>
    data : uint<64>
end struct ReadResponse

transactor ProtocolModel
    function predict_read(addr: uint<64>) -> ReadResponse
        let r : ReadResponse
        r.matched = 1
        r.data = addr + 16
        return r
    end predict_read
end transactor ProtocolModel

testbench Tb
    dut : DummyDut
    model : ProtocolModel active
end testbench Tb

impl ComponentMethodStructReturnTest for Tb
    run
        let r : ReadResponse = model.predict_read(32)
        assert r.matched != 0 else fail("no match")
        assert r.data == 48 else fail("bad data")
    end run
end impl ComponentMethodStructReturnTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("auto ProtocolModel_predict_read = [&](ProtocolModel& self, uint64_t addr) -> ReadResponse"),
        "expected struct return value in component method; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("-> VReadResponse*"),
        "struct returns must not lower as Verilator module pointers; got:\n{cpp}"
    );
}

#[test]
fn record_vec_field_lowers_to_array_and_pack_helpers() {
    let parsed = parse_source(
        r#"struct BurstResp
    data : Vec<uint<32>, 4>
    len : uint<3>
    resp : uint<2>
end struct BurstResp

test VecRecordTest
    let dut : DummyDut
    run
        let r : BurstResp
        r.data[0] = 0x10
        r.data[1] = 0x20
        r.len = 2
        r.resp = 0
        assert r.data[0] == 0x10 else fail("bad d0")
    end run
end test VecRecordTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("std::array<uint64_t, 4> data = {};"),
        "Vec record field should lower to fixed C++ array; got:\n{cpp}"
    );
    assert!(
        cpp.contains("static harc_rt::HarcWide<5> harc_pack_BurstResp"),
        "bounded response records should get a packed bridge helper; got:\n{cpp}"
    );
    assert!(
        cpp.contains("value.data[3] = (uint64_t)harc_rt::harc_bits(_packed, 132, 101);"),
        "bounded response records should unpack Vec lanes at ARCH packed offsets; got:\n{cpp}"
    );
}

#[test]
fn impl_for_testbench_preserves_testbench_dut_probes() {
    let parsed = parse_source(
        r#"testbench ProbeDutTb
    let dut : CpuPipe
        probe alu_a : uint<32> at alu0.a
        probe force inject_rs1 : uint<32> at decode_rs1_val
    end let dut

    function reset()
        dut.rst = 1
        wait 2 cycles
        dut.rst = 0
        wait 1 cycle
    end function reset
end testbench ProbeDutTb

impl TestbenchProbeDutTest for ProbeDutTb
    run
        reset()
        dut.inject_rs1 = 3735928559
        wait 1 cycle
        assert dut.alu_a == 3735928559
        release dut.inject_rs1
    end run
end impl TestbenchProbeDutTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("testbench-owned DUT probes lower cleanly");
    assert!(
        cpp.contains("_tb.dut = dut"),
        "expected impl desugaring to keep DUT wire-up; got:\n{}",
        cpp,
    );
    assert!(
        cpp.contains("alu_a") && cpp.contains("inject_rs1_drv") && cpp.contains("inject_rs1_en"),
        "expected read and force probe accessors to be preserved; got:\n{}",
        cpp,
    );
    let (dut_ty, probes) = cpp_tb::dut_probes(&parsed)
        .expect("probe catalog validates")
        .expect("testbench-owned probes should emit a bind stub");
    assert_eq!(dut_ty, "CpuPipe");
    assert_eq!(probes.len(), 2);
    assert!(probes.iter().any(|p| p.name.name == "alu_a" && !p.force));
    assert!(probes
        .iter()
        .any(|p| p.name.name == "inject_rs1" && p.force));
}

#[test]
fn probe_paths_preserve_sv_bracket_selectors() {
    let parsed = parse_source(
        r#"testbench ProbeArrayTb
    let dut : Top
        probe lane0 : uint<8> at block.array_sig[0]
        probe gen_state : uint<3> at gen_stage[2].u_stage.state_q
    end let dut
end testbench ProbeArrayTb

impl ProbeArrayTest for ProbeArrayTb
    run
        assert dut.lane0 == 0
    end run
end impl ProbeArrayTest"#,
    )
    .unwrap();
    let (_, probes) = cpp_tb::dut_probes(&parsed)
        .expect("probe catalog validates")
        .expect("array selector probes should parse as DUT probes");
    assert!(probes
        .iter()
        .any(|p| p.name.name == "lane0" && p.path == "block.array_sig[0]"));
    assert!(probes
        .iter()
        .any(|p| p.name.name == "gen_state" && p.path == "gen_stage[2].u_stage.state_q"));

    let stub = harc::codegen::sv_stub::emit_stub("Top", &probes).unwrap();
    assert!(stub.contains("assign lane0 = Top.block.array_sig[0];"));
    assert!(stub.contains("assign gen_state = Top.gen_stage[2].u_stage.state_q;"));
}

#[test]
fn probe_catalog_and_stub_preserve_scalar_kinds() {
    let parsed = parse_source(
        r#"test T
    let dut : Top
        probe bit_flag : bit at core.bit_flag
        probe bool_flag : bool at core.bool_flag
        probe unsigned_value : uint<8> at core.unsigned_value
        probe signed_value : sint<8> at core.signed_value
        probe raw_bits : bits<8> at core.raw_bits
    end let dut
    run
        wait 1 cycle
    end run
end test T"#,
    )
    .unwrap();
    let catalog = harc::codegen::sv_stub::collect_suite_probes(&parsed).unwrap();
    use harc::ir::ProbeScalarType;
    assert_eq!(catalog.probe_types["bit_flag"], ProbeScalarType::Bit);
    assert_eq!(catalog.probe_types["bool_flag"], ProbeScalarType::Bool);
    assert_eq!(
        catalog.probe_types["unsigned_value"],
        ProbeScalarType::UInt(8)
    );
    assert_eq!(
        catalog.probe_types["signed_value"],
        ProbeScalarType::SInt(8)
    );
    assert_eq!(catalog.probe_types["raw_bits"], ProbeScalarType::Bits(8));

    let stub = harc::codegen::sv_stub::emit_stub("Top", &catalog.probes).unwrap();
    assert!(stub.contains("logic signed [7:0] signed_value"), "{stub}");
    assert!(stub.contains("logic [7:0] unsigned_value"), "{stub}");
    assert!(stub.contains("logic [7:0] raw_bits"), "{stub}");
    assert!(stub.contains("logic bit_flag"), "{stub}");
    assert!(stub.contains("logic bool_flag"), "{stub}");
}

#[test]
fn suite_probe_catalog_deduplicates_identical_declarations() {
    let parsed = parse_source(
        r#"test A
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    run
        wait 1 cycle
    end run
end test A

test B
    let dut : Top
        probe force inject : UInt<32> at core.inject
    end let dut
    run
        wait 1 cycle
    end run
end test B"#,
    )
    .unwrap();
    let (dut, probes) = cpp_tb::dut_probes(&parsed)
        .expect("identical suite declarations validate")
        .expect("suite emits one bind stub");
    assert_eq!(dut, "Top");
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].name.name, "inject");

    let catalog = harc::codegen::sv_stub::collect_suite_probes(&parsed).unwrap();
    assert_eq!(catalog.shared_component_probes.len(), 1);
}

#[test]
fn suite_probe_catalog_keeps_test_local_union_out_of_shared_components() {
    let parsed = parse_source(
        r#"test A
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    run
        wait 1 cycle
    end run
end test A

test B
    let dut : Top
    run
        wait 1 cycle
    end run
end test B"#,
    )
    .unwrap();
    let catalog = harc::codegen::sv_stub::collect_suite_probes(&parsed).unwrap();
    assert_eq!(
        catalog.probes.len(),
        1,
        "the suite bind stub uses the union"
    );
    assert!(
        catalog.shared_component_probes.is_empty(),
        "a declaration missing from one test cannot authorize shared code"
    );
    assert!(catalog.partial_component_probe_names.contains("inject"));
}

#[test]
fn suite_probe_catalog_rejects_conflicting_declarations() {
    for (label, second) in [
        ("path", "probe force inject : uint<32> at other.inject"),
        ("type", "probe force inject : uint<16> at core.inject"),
        ("force", "probe inject : uint<32> at core.inject"),
    ] {
        let src = format!(
            r#"test A
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    run
        wait 1 cycle
    end run
end test A

test B
    let dut : Top
        {second}
    end let dut
    run
        wait 1 cycle
    end run
end test B"#
        );
        let parsed = parse_source(&src).unwrap();
        let err = cpp_tb::dut_probes(&parsed).expect_err("conflict must fail closed");
        let msg = err.to_string();
        assert!(msg.contains("conflicting declarations"), "{label}: {msg}");
        assert!(msg.contains("inject"), "{label}: {msg}");
    }
}

#[test]
fn suite_probe_conflict_retains_both_source_sites() {
    let first = harc::parser::parse_source_named(
        "first_probe.harc",
        r#"test A
    let dut : Top
        probe inject : sint<8> at core.inject
    end let dut
    run
        wait 1 cycle
    end run
end test A"#,
    )
    .unwrap();
    let second = harc::parser::parse_source_named(
        "second_probe.harc",
        r#"test B
    let dut : Top
        probe inject : uint<8> at core.inject
    end let dut
    run
        wait 1 cycle
    end run
end test B"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![second, first], None).unwrap();
    let err = harc::codegen::sv_stub::collect_suite_probes(&merged)
        .expect_err("conflicting scalar kind must fail");
    let related = err
        .related_site()
        .expect("first declaration source is retained");
    assert_eq!(
        &*merged.source_for_id(related.source_id).unwrap().name,
        "first_probe.harc"
    );
    assert_eq!(
        &*merged.source_for_id(err.source_id()).unwrap().name,
        "second_probe.harc"
    );
    let msg = err.to_string();
    assert!(msg.contains("first_probe.harc:3:"), "{msg}");
    assert!(msg.contains("second_probe.harc:3:"), "{msg}");
    assert!(msg.contains("sint<8>") && msg.contains("uint<8>"), "{msg}");
}

#[test]
fn suite_probe_catalog_rejects_generated_names_and_force_path_aliases() {
    for (label, declarations, expected) in [
        (
            "generated name",
            "probe force inject : uint<32> at core.inject\n        probe inject_drv : uint<32> at core.other",
            "generated bind-stub signal",
        ),
        (
            "force path alias",
            "probe force inject_a : uint<32> at core.inject\n        probe force inject_b : uint<32> at core.inject",
            "competing force controllers",
        ),
        (
            "force path overlap",
            "probe force state : uint<32> at core.state\n        probe force state_bit : uint<1> at core.state[0]",
            "overlapping SV paths",
        ),
    ] {
        let src = format!(
            r#"test T
    let dut : Top
        {declarations}
    end let dut
    run
        wait 1 cycle
    end run
end test T"#
        );
        let parsed = parse_source(&src).unwrap();
        let err = cpp_tb::dut_probes(&parsed).expect_err("unsafe probe aliases must fail");
        assert!(err.to_string().contains(expected), "{label}: {err}");
    }
}

#[test]
fn v1_common_split_uses_the_validated_probe_catalog_for_shared_methods() {
    let parsed = parse_source(
        r#"agent ProbeUser
    function read_probe() -> uint<32>
        return dut.inject
    end function read_probe
    function force_probe(value: uint<32>)
        dut.inject = value
    end function force_probe
    function release_probe()
        release dut.inject
    end function release_probe
end agent ProbeUser

test A
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    let user : ProbeUser
    run
        let before : uint<32> = user.read_probe()
        user.force_probe(before + 1)
        user.release_probe()
    end run
end test A

test B
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    let user : ProbeUser
    run
        wait 1 cycle
    end run
end test B"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let emitted = cpp_tb::emit_common_split(&merged, cpp_tb::EmitOpts::default(), "suite__", &[])
        .expect("v1 common-object emission accepts the shared probe contract");
    let all_cpp = emitted
        .files
        .iter()
        .map(|file| file.contents.as_str())
        .collect::<String>();
    let accessor = "Top__DOT__harc_probes__DOT__inject";
    assert!(all_cpp.contains(accessor), "{all_cpp}");
    assert!(all_cpp.contains(&format!("{accessor}_drv")), "{all_cpp}");
    assert!(all_cpp.contains(&format!("{accessor}_en")), "{all_cpp}");
}

#[test]
fn v1_common_split_rejects_partial_probes_in_shared_methods() {
    for (label, method) in [
        (
            "read",
            "function use_probe() -> uint<32>\n        return dut.inject\n    end function use_probe",
        ),
        (
            "write",
            "function use_probe()\n        dut.inject = 1\n    end function use_probe",
        ),
        (
            "release",
            "function use_probe()\n        release dut.inject\n    end function use_probe",
        ),
    ] {
        let parsed = parse_source(&format!(
            r#"agent ProbeUser
    {method}
end agent ProbeUser

test A
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    let user : ProbeUser
    run
        wait 1 cycle
    end run
end test A

test B
    let dut : Top
    let user : ProbeUser
    run
        wait 1 cycle
    end run
end test B"#
        ))
        .unwrap();
        let err = cpp_tb::emit_common_split(
            &parsed,
            cpp_tb::EmitOpts::default(),
            "suite__",
            &[],
        )
        .expect_err("v1 common shared methods must not inherit another test's probe");
        let msg = err.to_string();
        assert!(msg.contains("shared DUT access `dut.inject`"), "{label}: {msg}");
    }
}

#[test]
fn v1_shared_probe_recognition_uses_the_component_dut_receiver_name() {
    for (kind, mode) in [("agent", ""), ("transactor", " active")] {
        let component = if kind == "transactor" {
            r#"transactor ProbeUser
    chip : Top
    when active
        function read_probe() -> uint<32>
            return chip.inject
        end function read_probe
        function force_probe(value: uint<32>)
            chip.inject = value
        end function force_probe
        function release_probe()
            release chip.inject
        end function release_probe
    end when
end transactor ProbeUser"#
        } else {
            r#"agent ProbeUser
    chip : Top
    function read_probe() -> uint<32>
        return chip.inject
    end function read_probe
    function force_probe(value: uint<32>)
        chip.inject = value
    end function force_probe
    function release_probe()
        release chip.inject
    end function release_probe
end agent ProbeUser"#
        };
        let parsed = parse_source(&format!(
            r#"{component}

test A
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    let user : ProbeUser{mode}
    run
        user.chip = dut
        let before : uint<32> = user.read_probe()
        user.force_probe(before + 1)
        user.release_probe()
    end run
end test A

test B
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    let user : ProbeUser{mode}
    run
        user.chip = dut
        wait 1 cycle
    end run
end test B"#,
        ))
        .unwrap_or_else(|error| panic!("{kind} source parses: {error:?}"));
        let common =
            cpp_tb::emit_common_split(&parsed, cpp_tb::EmitOpts::default(), "suite__", &[])
                .unwrap_or_else(|error| panic!("{kind} common emission succeeds: {error}"))
                .files
                .into_iter()
                .map(|file| file.contents)
                .collect::<String>();
        let legacy = cpp_tb::emit(&parsed)
            .unwrap_or_else(|error| panic!("{kind} legacy emission succeeds: {error}"));
        let accessor = "Top__DOT__harc_probes__DOT__inject";
        for (layout, all_cpp) in [("common", common), ("legacy", legacy)] {
            assert!(
                all_cpp.contains(&format!("dut->rootp->{accessor}")),
                "{kind}/{layout}: {all_cpp}"
            );
            assert!(
                all_cpp.contains(&format!("dut->rootp->{accessor}_drv")),
                "{kind}/{layout}: {all_cpp}"
            );
            assert!(
                all_cpp.contains(&format!("dut->rootp->{accessor}_en")),
                "{kind}/{layout}: {all_cpp}"
            );
        }
    }
}

#[test]
fn v1_probe_recognition_respects_param_and_local_shadowing() {
    let parsed = parse_source(
        r#"struct Snapshot
    inject : uint<32>
end struct Snapshot

agent ProbeUser
    function from_param(dut: Snapshot) -> uint<32>
        return dut.inject
    end function from_param
    function from_local() -> uint<32>
        let dut : Snapshot
        dut.inject = 9
        return dut.inject
    end function from_local
end agent ProbeUser

test A
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    let user : ProbeUser
    run
        let snapshot : Snapshot
        let a : uint<32> = user.from_param(snapshot)
        let b : uint<32> = user.from_local()
        assert a == b
    end run
end test A

test B
    let dut : Top
        probe force inject : uint<32> at core.inject
    end let dut
    let user : ProbeUser
    run
        wait 1 cycle
    end run
end test B"#,
    )
    .expect("source parses");

    for (label, emitted) in [
        ("legacy", cpp_tb::emit(&parsed).map(|cpp| vec![cpp])),
        (
            "common",
            cpp_tb::emit_common_split(&parsed, cpp_tb::EmitOpts::default(), "suite__", &[])
                .map(|output| output.files.into_iter().map(|file| file.contents).collect()),
        ),
    ] {
        let all_cpp = emitted
            .unwrap_or_else(|error| panic!("{label} emission succeeds: {error}"))
            .join("\n");
        assert!(all_cpp.contains("return dut.inject;"), "{label}: {all_cpp}");
        assert!(all_cpp.contains("dut.inject = 9;"), "{label}: {all_cpp}");
        assert!(
            !all_cpp.contains("rootp->Top__DOT__harc_probes__DOT__inject"),
            "{label}: shadowed host values must not become probes: {all_cpp}"
        );
    }
}

#[test]
fn v1_self_contained_shards_use_the_full_suite_probe_contract() {
    let parsed = parse_source(
        r#"agent ProbeUser
    function read_probe() -> uint<32>
        return dut.inject
    end function read_probe
end agent ProbeUser

test A
    let dut : Top
        probe inject : uint<32> at core.inject
    end let dut
    let user : ProbeUser
    run
        let value : uint<32> = user.read_probe()
    end run
end test A

test B
    let dut : Top
    let user : ProbeUser
    run
        wait 1 cycle
    end run
end test B"#,
    )
    .expect("source parses");

    let one = cpp_tb::emit_split_tests_with_file_prefix(
        &parsed,
        cpp_tb::EmitOpts::default(),
        "suite__",
        1,
    )
    .expect_err("group size one must reject the partial shared probe")
    .to_string();
    let bundled = cpp_tb::emit_split_tests_with_file_prefix(
        &parsed,
        cpp_tb::EmitOpts::default(),
        "suite__",
        2,
    )
    .expect_err("bundled emission must reject the same partial shared probe")
    .to_string();
    let expected = "shared DUT access `dut.inject` names a probe that is not declared identically by every test in the suite";
    assert!(one.lines().all(|line| line == expected), "{one}");
    assert!(bundled.lines().all(|line| line == expected), "{bundled}");
}

/// Classic `test T { ... }` form keeps building in this PR — the
/// parser-entry removal + the inline-source-test sweep lands as a
/// follow-up. Mirrors PR #91 → #92's staged inline-`run` migration.
#[test]
fn classic_test_form_still_emits() {
    let parsed = parse_source(
        r#"test Smoke
    let dut : Top
    run
        dut.rst = 1
    end run
end test Smoke"#,
    )
    .unwrap();
    cpp_tb::emit(&parsed).expect("classic `test` form should still lower cleanly");
}

#[test]
fn testbench_hookable_still_emits_hook_vectors() {
    // Companion: `hookable name(...)` keeps its pre/post vectors. The
    // discriminator is the AST flag `is_hookable`, set from the
    // introducing keyword.
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top

    hookable reset()
        dut.rst = 1
    end reset
end testbench Tb

test T
    let dut : Top
    let tb  : Tb
    run
        tb.dut = dut
        tb.reset()
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("testbench + hookable lowers cleanly");
    assert!(
        cpp.contains("Tb_reset_pre"),
        "hookable should emit pre vector"
    );
    assert!(
        cpp.contains("Tb_reset_post"),
        "hookable should emit post vector"
    );
}

/// Negative case for the genuine-observer shape we want to keep
/// working: an always-on `on bus.<ch>.handshake(t)` handler that
/// only pushes into a scoreboard field (no DUT write, no bus send)
/// is fine even on a `passive` instance.
#[test]
fn transactor_always_on_observer_emits_cleanly() {
    let parsed = parse_source(
        r#"bus B
    handshake_channel ch: receive kind: valid_ready
        data: uint<32>;
    end handshake_channel ch
end bus B

scoreboard S
    seen : queue<uint<32>>
end scoreboard S

transactor Mon bound to B
    sb : S

    on bus.ch.handshake(d)
        sb.seen.push(d)
    end on
end transactor Mon

test T
    let dut : SomeDut
    let b : B = bind dut
    let mon : Mon passive = bind b
    run
    end run
end test T"#,
    )
    .unwrap();
    cpp_tb::emit(&parsed)
        .expect("observer-only handler in always-on body should emit cleanly under `passive`");
}

#[test]
fn target_tlm_thread_lowers_to_responder_actor() {
    let parsed = parse_source(
        r#"bus B
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus B

transactor Target bound to B
    thread bus.read(addr: uint<8>)
        wait 1 cycle
        return 256 + addr
    end thread
end transactor Target

test T
    let dut : SomeDut
    let b : B = bind dut
    let target : Target passive = bind b
    run
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("target TLM thread should lower");
    let capture_pos = cpp
        .find("uint64_t addr = static_cast<uint64_t>(harc_rt::harc_trunc_u128")
        .expect("target responder should capture request args");
    assert!(
        cpp[capture_pos..].starts_with(
            "uint64_t addr = static_cast<uint64_t>(harc_rt::harc_trunc_u128(static_cast<_harc_u128>(harc_rt::harc_read(dut->b_read_addr)), 8));"
        ),
        "target responder must capture the exact declared uint<8> request type:\n{cpp}"
    );
    let edge_wait_pos = capture_pos
        + cpp[capture_pos..]
            .find("co_await harc_rt::wait_cycles(_slot, 1);")
            .expect("target responder should keep req_ready high through a sampling edge");
    let ready_low_pos = edge_wait_pos
        + cpp[edge_wait_pos..]
            .find("dut->b_read_req_ready = 0;")
            .expect("target responder should deassert req_ready after the sampling edge");
    let body_wait_pos = ready_low_pos
        + cpp[ready_low_pos..]
            .find("co_await harc_rt::wait_cycles(_slot, 1);")
            .expect("target responder should preserve body wait statements after request capture");
    assert!(
        cpp.contains("_target_read_target_slot")
            && cpp.contains("dut->b_read_req_ready = 1;")
            && cpp.contains("harc_rt::harc_assign(_tlm_rsp_value, 256 + addr);")
            && cpp.contains("harc_rt::harc_assign(dut->b_read_rsp_data, _tlm_rsp_value);")
            && cpp.contains("trace.tlm_call(cycle_count, \"target\", \"bus\", \"read\", \"request\", \"target\");")
            && cpp.contains("trace.tlm_call(cycle_count, \"target\", \"bus\", \"read\", \"response\", \"target\");")
            && cpp.contains("dut->b_read_rsp_valid = 1;")
            && capture_pos < edge_wait_pos
            && edge_wait_pos < ready_low_pos
            && ready_low_pos < body_wait_pos,
        "expected target responder actor shape; got:\n{cpp}"
    );
}

#[test]
fn target_tlm_thread_allows_terminal_if_returns() {
    let parsed = parse_source(
        r#"bus B
    tlm_method read(addr: uint<8>) -> uint<32>: blocking;
end bus B

transactor Target bound to B
    thread bus.read(addr: uint<8>)
        if addr < 8
            return 256 + addr
        else
            return 512 + addr
        end if
    end thread
end transactor Target

test T
    let dut : SomeDut
    let b : B = bind dut
    let target : Target passive = bind b
    run
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("target TLM terminal if returns should lower");
    assert!(
        cpp.contains("if (addr < 8)")
            && cpp.contains("harc_rt::harc_assign(_tlm_rsp_value, 256 + addr);")
            && cpp.contains("harc_rt::harc_assign(_tlm_rsp_value, 512 + addr);"),
        "expected target responder terminal if lowering; got:\n{cpp}"
    );
}

#[test]
fn target_tlm_thread_allows_runtime_loop_before_return() {
    let parsed = parse_source(
        r#"bus B
    tlm_method read(addr: uint<8>, len: uint<4>) -> uint<32>: blocking;
end bus B

transactor Target bound to B
    prep_acc : uint<32> default 0

    thread bus.read(addr: uint<8>, len: uint<4>)
        prep_acc = 0
        for i in 0 .. len
            prep_acc = prep_acc + addr + i
        end for
        return prep_acc
    end thread
end transactor Target

test T
    let dut : SomeDut
    let b : B = bind dut
    let target : Target passive = bind b
    run
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("target TLM runtime loop before return should lower");
    assert!(
        cpp.contains("for (int64_t i = 0; i <= len; i++)")
            && cpp.contains("target.prep_acc = target.prep_acc + addr + i;")
            && cpp.contains("harc_rt::harc_assign(_tlm_rsp_value, target.prep_acc);"),
        "expected target responder runtime loop lowering; got:\n{cpp}"
    );
}

#[test]
fn target_tlm_thread_lowers_early_return_inside_runtime_loop() {
    let parsed = parse_source(
        r#"bus B
    tlm_method read(addr: uint<8>, len: uint<4>) -> uint<32>: blocking;
end bus B

transactor Target bound to B
    prep_acc : uint<32> default 0

    thread bus.read(addr: uint<8>, len: uint<4>)
        prep_acc = 0
        for i in 0 .. len
            prep_acc = prep_acc + addr + i
            if i == 2
                return prep_acc
            end if
        end for
        return 4096 + prep_acc
    end thread
end transactor Target

test T
    let dut : SomeDut
    let b : B = bind dut
    let target : Target passive = bind b
    run
    end run
end test T"#,
    )
    .unwrap();
    let cpp =
        cpp_tb::emit(&parsed).expect("target TLM early return inside runtime loop should lower");
    assert!(
        cpp.contains("bool _tlm_returned = false;")
            && cpp.contains("if (!_tlm_returned)")
            && cpp.contains("harc_rt::harc_assign(_tlm_rsp_value, target.prep_acc);")
            && cpp.contains("_tlm_returned = true;")
            && cpp.contains("if (_tlm_returned) break;"),
        "expected target responder early-return lowering; got:\n{cpp}"
    );
}

#[test]
fn target_tlm_thread_out_of_order_lowers_lanes() {
    let parsed = parse_source(
        r#"bus B
    tlm_method read(addr: uint<8>) -> uint<32>: out_of_order tags 2;
end bus B

transactor Target bound to B
    thread bus.read(addr: uint<8>)
        wait 1 cycle
        return 256 + addr
    end thread
end transactor Target

test T
    let dut : SomeDut
    let b : B = bind dut
    let target : Target passive = bind b
    run
    end run
end test T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("target TLM OOO responder lanes should lower");
    assert!(
        cpp.contains("std::array<std::atomic<bool>, 2> _target_read_target_ooo_lane_busy{};")
            && cpp.contains("_post_eval_services.push_back([&]()")
            && cpp.contains("_target_read_target_ooo_lane0_slot")
            && cpp.contains("_target_read_target_ooo_lane1_slot")
            && cpp.contains("_target_read_target_ooo_arbiter_slot")
            && cpp.contains("trace.tlm_call(cycle_count, \"target\", \"bus\", \"read\", \"request\", \"target\", (int64_t)(_tag));")
            && cpp.contains("trace.tlm_call(cycle_count, \"target\", \"bus\", \"read\", \"response\", \"target\", (int64_t)(_sel));")
            && cpp.contains("dut->b_read_rsp_tag = _sel;"),
        "expected OOO target responder lane lowering; got:\n{cpp}"
    );
}

// ── Width-method intrinsics (.trunc/.zext/.sext/.resize) ────────────
//
// Ported from arch-com's surface (src/parser.rs:5757 +
// src/sim_codegen/mod.rs:2688). Spec: each method takes one constant
// width arg, parser recognizes `.<name><W>()` shape only when `<name>`
// is one of `trunc` / `zext` / `sext` / `resize`. Codegen emits the
// corresponding C++ narrow/extend, with a wrong-direction check when
// the source width is statically known (via a typed let or an
// explicit `as uint<W>` / `as sint<W>` cast).

#[test]
fn trunc_narrows_with_mask() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<64> = 0x123456789ABCDEF0
        let n = v.trunc<32>()
        log(info, "${n:08x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Expect a mask to 32 bits, then cast to uint64_t storage.
    assert!(
        cpp.contains("0xFFFFFFFFULL"),
        "expected 0xFFFFFFFFULL mask in trunc emit; got:\n{cpp}",
    );
}

#[test]
fn trunc_wrong_direction_errors() {
    // `.trunc<N>()` where N >= source width is a no-op or widens —
    // both wrong. The error suggests `.zext<N>()` as the fix.
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<8> = 0xFF
        let bad = v.trunc<16>()
        log(info, "${bad}")
    end run
end impl T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("trunc<16>") && err.0.contains("8-bit") && err.0.contains("zext"),
        "expected widen-direction error pointing to zext; got: {}",
        err.0,
    );
}

#[test]
fn zext_widens_via_cast() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<8> = 0xAB
        let w = v.zext<32>()
        log(info, "${w:08x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Plain `(uint64_t)(...)` widening — no mask, no shift.
    assert!(
        cpp.contains("(uint64_t)(") && !cpp.contains("0xFFFFFFFFULL"),
        "expected plain widening cast for zext; got:\n{cpp}",
    );
}

#[test]
fn zext_narrowing_direction_errors() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<32> = 0xFFFF
        let bad = v.zext<8>()
        log(info, "${bad}")
    end run
end impl T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("zext<8>") && err.0.contains("32-bit") && err.0.contains("trunc"),
        "expected narrow-direction error pointing to trunc; got: {}",
        err.0,
    );
}

#[test]
fn sext_uses_shift_arithmetic_idiom() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<8> = 0xFF
        let s = v.sext<32>()
        log(info, "${s:08x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    // Sign-extend idiom: cast to (int64_t), shift left by (64-sw),
    // arith-shift right back. Look for the shift pair.
    assert!(
        cpp.contains("(int64_t)") && cpp.contains(") << 56") && cpp.contains(") >> 56"),
        "expected (int64_t)(... << 56) >> 56 shift-fill idiom; got:\n{cpp}",
    );
}

#[test]
fn resize_narrows_or_widens_by_direction() {
    // Same source width as dest → plain cast (no mask).
    let parsed_widen = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<8> = 0xFF
        let w = v.resize<32>()
        log(info, "${w:08x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp_widen = cpp_tb::emit(&parsed_widen).expect("emit");
    // Widening path: plain cast.
    assert!(
        !cpp_widen.contains("0xFFULL"),
        "expected no narrowing mask on widen; got:\n{cpp_widen}"
    );

    let parsed_narrow = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<32> = 0xDEADBEEF
        let w = v.resize<16>()
        log(info, "${w:04x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp_narrow = cpp_tb::emit(&parsed_narrow).expect("emit");
    // Narrowing path: 16-bit mask.
    assert!(
        cpp_narrow.contains("0xFFFFULL"),
        "expected 0xFFFFULL narrowing mask; got:\n{cpp_narrow}",
    );
}

#[test]
fn width_method_on_anonymous_expression_works() {
    // No source-width inference, but emission still succeeds (the
    // type-direction check just skips, matching arch-com).
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let s = (0xDEADBEEFFFFF as uint<48>).trunc<32>()
        log(info, "${s:08x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("0xFFFFFFFFULL"),
        "expected 32-bit mask from .trunc<32>(); got:\n{cpp}",
    );
}

#[test]
fn issue_215_wide_zext_128_arithmetic_emits() {
    let parsed = parse_source(
        r#"function calc(a: uint<48>, b: uint<16>, c: uint<32>, d: uint<48>) -> uint<32>
    let n : uint<128> = a.zext<128>() * b.zext<128>() * c.zext<128>()
    let q : uint<128> = n / d.zext<128>()
    if (q >> 32) != 0
        return 0xffffffff
    end if
    return q.trunc<32>()
end function calc

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let r : uint<32> = calc(3, 5, 7, 2)
        log(info, "${r:08x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("issue #215 wide arithmetic should emit");
    assert!(
        cpp.contains("_harc_u128"),
        "expected _harc_u128 in emitted C++:\n{cpp}"
    );
    assert!(
        !cpp.contains("width must be in 1..=64"),
        "stale width limit leaked into output:\n{cpp}"
    );
}

#[test]
fn wide_zext_128_minimal_repro_emits_and_truncates() {
    let parsed = parse_source(
        r#"function wide_zext_128_repro(a: uint<48>, b: uint<16>) -> uint<64>
    let product : uint<128> = a.zext<128>() * b.zext<128>()
    return product.trunc<64>()
end function wide_zext_128_repro

testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let r : uint<64> = wide_zext_128_repro(0x1234, 0x10)
        log(info, "${r:016x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("wide zext repro should emit");
    assert!(
        cpp.contains("_harc_u128"),
        "expected _harc_u128 lowering:\n{cpp}"
    );
    assert!(
        cpp.contains("uint64_t") || cpp.contains("0xFFFFFFFFFFFFFFFF"),
        "expected trunc<64> path:\n{cpp}"
    );
}

#[test]
fn wide_128_width_methods_emit_masks_and_sign_extension() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let narrow : uint<64> = 0xffffffffffffffff
        let wide : uint<128> = narrow.zext<128>()
        let resized : uint<128> = narrow.resize<128>()
        let low96 : uint<96> = wide.trunc<96>()
        let signed128 : uint<128> = (0xff as uint<8>).sext<128>()
        log(info, "${low96:032x} ${resized:032x} ${signed128:032x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("128-bit width methods should emit");
    assert!(
        cpp.contains("_harc_u128"),
        "expected _harc_u128 lowering:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_sext") || cpp.contains("<< 120") || cpp.contains("0xFFFFFFFF"),
        "expected sign-extension or mask code:\n{cpp}"
    );
}

#[test]
fn wide_128_wrong_direction_checks_still_error() {
    let bad_trunc = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<64> = 1
        let bad = v.trunc<128>()
        log(info, "${bad}")
    end run
end impl T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&bad_trunc).unwrap_err();
    assert!(
        err.0.contains("trunc<128>") && err.0.contains("zext"),
        "expected trunc wrong-direction diagnostic, got: {}",
        err.0
    );

    let bad_zext = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let v : uint<128> = 1
        let bad = v.zext<64>()
        log(info, "${bad}")
    end run
end impl T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&bad_zext).unwrap_err();
    assert!(
        err.0.contains("zext<64>") && err.0.contains("trunc"),
        "expected zext wrong-direction diagnostic, got: {}",
        err.0
    );
}

#[test]
fn uint256_typed_local_uses_harcwide_storage() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let x : uint<256> = 1
        log(info, "${x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("uint<256> local should emit");
    assert!(
        cpp.contains("harc_rt::HarcWide<8> x"),
        "expected HarcWide<8> local:\n{cpp}"
    );
}

#[test]
fn uint256_width_methods_emit_harcwide_helpers() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let small : uint<64> = 0xffffffffffffffff
        let wide : uint<256> = small.zext<256>()
        let low130 : uint<130> = wide.trunc<130>()
        let sign130 : uint<130> = (0x100 as uint<9>).sext<130>()
        log(info, "${low130} ${sign130}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("uint<256> width methods should emit");
    assert!(
        cpp.contains("harc_rt::HarcWide<8>"),
        "expected HarcWide<8> for zext<256>:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_wide_trunc") && cpp.contains("harc_wide_sext"),
        "expected wide trunc/sext helpers:\n{cpp}"
    );
}

#[test]
fn v1_rejects_width_method_above_language_limit() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let value : uint<1025> = (1 as uint<64>).zext<1025>()
        log(info, "${value}")
    end run
end impl T"#,
    )
    .unwrap();
    let err = cpp_tb::emit(&parsed).unwrap_err();
    assert!(
        err.0.contains("zext<1025>") && err.0.contains("1024-bit language limit"),
        "expected language-limit diagnostic, got: {}",
        err.0
    );
}

#[test]
fn v1_normalizes_known_wide_sources_for_extensions() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let source : uint<130> = 1
        let zext : uint<256> = source.zext<256>()
        let resized : uint<256> = source.resize<256>()
        let same_sext : sint<130> = source.sext<130>()
        log(info, "${zext} ${resized} ${same_sext}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("known-width wide extensions should emit");
    assert_eq!(
        cpp.matches("harc_rt::harc_wide_zext<8>(source, 130)")
            .count(),
        2,
        "zext and widening resize must normalize source padding:\n{cpp}"
    );
    assert!(
        cpp.contains("harc_rt::harc_wide_trunc<5>(source, 130)"),
        "same-width sext must normalize source padding:\n{cpp}"
    );
}

#[test]
fn uint256_hex_literal_local_uses_harcwide_value_expression() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let x : uint<256> = 0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
        log(info, "${x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("uint<256> literal local should emit");
    assert!(
        cpp.contains("harc_rt::HarcWide<8> x"),
        "expected HarcWide<8> local:\n{cpp}"
    );
    assert!(
        cpp.contains("0x1c1d1e1f") || cpp.contains("0x1C1D1E1F"),
        "expected split 32-bit words in literal:\n{cpp}"
    );
}

#[test]
fn harcwide_mask_and_sign_extension_runtime() {
    compile_and_run_runtime_cpp(
        "mask_sext",
        r#"
        harc_rt::HarcWide<5> v;
        for (auto& w : v.words) w = 0xffffffffu;
        auto m = harc_rt::harc_wide_mask_bits(v, 130);
        assert(m.words[0] == 0xffffffffu);
        assert(m.words[1] == 0xffffffffu);
        assert(m.words[2] == 0xffffffffu);
        assert(m.words[3] == 0xffffffffu);
        assert(m.words[4] == 0x00000003u);
        auto t129 = harc_rt::harc_wide_trunc<5>(v, 129);
        auto t130 = harc_rt::harc_wide_trunc<5>(v, 130);
        assert(t129.words[4] == 0x00000001u);
        assert(t130.words[4] == 0x00000003u);
        auto s129 = harc_rt::harc_wide_sext<5>(0x100u, 9, 129);
        auto s130 = harc_rt::harc_wide_sext<5>(0x100u, 9, 130);
        assert(s129.words[0] == 0xffffff00u);
        assert(s129.words[1] == 0xffffffffu);
        assert(s129.words[2] == 0xffffffffu);
        assert(s129.words[3] == 0xffffffffu);
        assert(s129.words[4] == 0x00000001u);
        assert(s130.words[0] == 0xffffff00u);
        assert(s130.words[1] == 0xffffffffu);
        assert(s130.words[2] == 0xffffffffu);
        assert(s130.words[3] == 0xffffffffu);
        assert(s130.words[4] == 0x00000003u);
        "#,
    );
}

#[test]
fn harcwide_sign_extension_accepts_harcwide_source() {
    compile_and_run_runtime_cpp(
        "wide_source_sext",
        r#"
        auto negative130 = harc_rt::harc_wide_sext<5>(0x100u, 9, 130);
        auto negative256 = harc_rt::harc_wide_sext<8>(negative130, 130, 256);
        assert(negative256.words[0] == 0xffffff00u);
        for (std::size_t i = 1; i < 8; ++i) assert(negative256.words[i] == 0xffffffffu);

        auto positive130 = harc_rt::harc_wide_sext<5>(0x0ffu, 9, 130);
        auto positive256 = harc_rt::harc_wide_sext<8>(positive130, 130, 256);
        assert(positive256.words[0] == 0x000000ffu);
        for (std::size_t i = 1; i < 8; ++i) assert(positive256.words[i] == 0u);
        "#,
    );
}

#[test]
fn harcwide_zero_extension_masks_source_padding() {
    compile_and_run_runtime_cpp(
        "wide_zext_padding",
        r#"
        harc_rt::HarcWide<5> dirty;
        for (auto& word : dirty.words) word = 0xffffffffu;
        auto extended = harc_rt::harc_wide_zext<8>(dirty, 130);
        assert(extended.words[0] == 0xffffffffu);
        assert(extended.words[3] == 0xffffffffu);
        assert(extended.words[4] == 0x00000003u);
        for (std::size_t i = 5; i < 8; ++i) assert(extended.words[i] == 0u);
        "#,
    );
}

#[test]
fn harcwide_arithmetic_runtime() {
    compile_and_run_runtime_cpp(
        "arith",
        r#"
        harc_rt::HarcWide<8> a;
        a.words[0] = 0xffffffffu;
        auto one = harc_rt::HarcWide<8>(1u);
        auto sum = a + one;
        assert(sum.words[0] == 0u);
        assert(sum.words[1] == 1u);
        auto diff = sum - one;
        assert(diff.words[0] == 0xffffffffu);
        assert(diff.words[1] == 0u);
        auto mul = harc_rt::HarcWide<8>(0x10000u) * harc_rt::HarcWide<8>(0x10000u);
        assert(mul.words[0] == 0u);
        assert(mul.words[1] == 1u);
        "#,
    );
}

#[test]
fn harcwide_shift_compare_div_mod_runtime() {
    compile_and_run_runtime_cpp(
        "shift_div",
        r#"
        auto one = harc_rt::HarcWide<8>(1u);
        auto s32 = one << 32;
        assert(s32.words[0] == 0u && s32.words[1] == 1u);
        auto s127 = one << 127;
        assert(s127.words[3] == 0x80000000u);
        auto back = s127 >> 127;
        assert(back.words[0] == 1u);
        assert(s127 > s32);
        auto n = harc_rt::HarcWide<8>(1000u);
        auto d = harc_rt::HarcWide<8>(37u);
        auto q = n / d;
        auto r = n % d;
        auto check = q * d + r;
        assert(check == n);
        assert(r < d);
        "#,
    );
}

#[test]
fn harcwide_extract_and_declared_width_shift_runtime() {
    compile_and_run_runtime_cpp(
        "extract_shift",
        r#"
        harc_rt::HarcWide<12> packed;
        harc_rt::HarcWide<8> field;
        field.words[0] = 0x89abcdefu;
        field.words[3] = 0x76543210u;
        field.words[7] = 0x80000000u;
        harc_rt::harc_wide_write_bits(packed, 37, 256, field);
        auto extracted = harc_rt::harc_wide_extract_bits<8>(packed, 37, 256);
        assert(extracted == field);

        harc_rt::HarcWide<5> dirty;
        for (auto& word : dirty.words) word = 0xffffffffu;
        harc_rt::harc_wide_write_bits(packed, 3, 130, dirty);
        auto odd = harc_rt::harc_wide_extract_bits<5>(packed, 3, 130);
        assert(odd.words[4] == 3u);

        auto signed65 = harc_rt::harc_wide_sext<3>(1u, 1, 65);
        auto shifted = harc_rt::harc_wide_ashr(signed65, 64, 65);
        assert(shifted.words[0] == 0xffffffffu);
        assert(shifted.words[1] == 0xffffffffu);
        assert(shifted.words[2] == 1u);
        assert(harc_rt::harc_ashr_u128((_harc_u128{1} << 64), 64, 65) == harc_rt::harc_mask_u128(65));
        "#,
    );
}

#[test]
fn uint256_arithmetic_expression_uses_harcwide_operators() {
    let parsed = parse_source(
        r#"testbench Tb
    dut : Top
end testbench Tb
impl T for Tb
    run
        let a : uint<256> = 0x100000000000000000000000000000000
        let b : uint<256> = 0x25
        let c : uint<256> = ((a + b) * b) / b
        let r : uint<256> = c % b
        assert r == 0 else fail("wide modulo")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("uint<256> arithmetic should emit");
    assert!(
        cpp.contains("harc_rt::HarcWide<8>"),
        "expected HarcWide<8> arithmetic:\n{cpp}"
    );
    assert!(
        cpp.contains(" / ") && cpp.contains(" % "),
        "expected division and modulo operators in emitted C++:\n{cpp}"
    );
}

#[test]
fn wide_vectors_up_to_1024_bits_lower_to_word_values() {
    let parsed = parse_source(
        r#"bus WideBus
    tlm_method send(data: uint<1024>) -> uint<1024>: blocking;
end bus WideBus

testbench Tb
    dut : WideTop
end testbench Tb

impl T for Tb
    let wide : WideBus = bind dut with {
        send.req_valid: "send_req_valid",
        send.req_ready: "send_req_ready",
        send.data: "send_data",
        send.rsp_valid: "send_rsp_valid",
        send.rsp_ready: "send_rsp_ready",
        send.rsp_data: "send_rsp_data"
    }

    run
        dut.payload = 0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
        let got = wide.send(dut.payload)
        assert got == dut.payload else fail("wide mismatch got=${got:0256x}")
    end run
end impl T"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("harc_rt::HarcWide<32>"),
        "expected uint<1024> to lower to HarcWide<32>; got:\n{cpp}",
    );
    assert!(
        cpp.contains("harc_rt::harc_assign(dut->send_data, harc_rt::harc_read(dut->payload));"),
        "expected TLM wide arg to route through harc_assign/harc_read; got:\n{cpp}",
    );
    assert!(
        cpp.contains("harc_rt::HarcHexBufWide("),
        "expected 1024-bit hex interpolation to use HarcHexBufWide; got:\n{cpp}",
    );
}

/// Regression for issue #301: a transactor whose field type is another
/// transactor declared later in the source list previously emitted
/// undeclared C++ symbols. `harc check` accepted the forward reference,
/// but the C++ struct definition for the owning transactor put a
/// by-value member of the as-yet-undeclared type, and the corresponding
/// hookable-method lambda referenced the referenced transactor's
/// `<Type>_<method>` helper before its `[&]`-captured declaration.
///
/// The fixtures live at
/// `tests/fixtures/transactor_forward_ref_consumer_test.harc` (declares
/// `ConsumerXact` with `src : ProducerXact`) and
/// `tests/fixtures/transactor_forward_ref_producer_test.harc`. The
/// consumer fixture is loaded FIRST so the regression's source-order
/// trigger is preserved.
///
/// Acceptance: the emitted C++ declares `ProducerXact` before
/// `ConsumerXact`, and `ProducerXact_count` before
/// `ConsumerXact_observed_count`. The fix is a topological sort over
/// component / transactor structs and their hookable-method lambdas
/// keyed on by-value field-type references; see
/// `topo_sort_component_indices` in `src/codegen/cpp_tb.rs`.
#[test]
fn transactor_field_forward_reference_emits_in_dependency_order() {
    use std::fs;
    use std::path::PathBuf;

    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let consumer_src =
        fs::read_to_string(fixtures.join("transactor_forward_ref_consumer_test.harc"))
            .expect("read consumer fixture");
    let producer_src =
        fs::read_to_string(fixtures.join("transactor_forward_ref_consumer_test_sim.harc"))
            .expect("read producer fixture");
    // Consumer FIRST — that's the source-order trigger from the bug.
    let consumer = parse_source(&consumer_src).expect("parse consumer");
    let producer = parse_source(&producer_src).expect("parse producer");
    let merged = merge::merge_for_sim(vec![consumer, producer], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    let producer_struct = cpp
        .find("struct ProducerXact")
        .expect("ProducerXact struct missing from emitted C++");
    let consumer_struct = cpp
        .find("struct ConsumerXact")
        .expect("ConsumerXact struct missing from emitted C++");
    assert!(
        producer_struct < consumer_struct,
        "struct order regressed: ProducerXact must precede ConsumerXact \
         so the by-value field `src : ProducerXact` resolves to a \
         complete type. ProducerXact at {producer_struct}, ConsumerXact \
         at {consumer_struct}",
    );

    let producer_method = cpp
        .find("auto ProducerXact_count = ")
        .expect("ProducerXact_count lambda missing from emitted C++");
    let consumer_method = cpp
        .find("auto ConsumerXact_observed_count = ")
        .expect("ConsumerXact_observed_count lambda missing from emitted C++");
    assert!(
        producer_method < consumer_method,
        "method-lambda order regressed: ProducerXact_count must be \
         declared before ConsumerXact_observed_count, which calls \
         ProducerXact_count from its `[&]`-captured body. \
         ProducerXact_count at {producer_method}, \
         ConsumerXact_observed_count at {consumer_method}",
    );

    // Sanity: the call site inside ConsumerXact_observed_count's body
    // should resolve to ProducerXact_count(self.src) — confirms the
    // ordering above isn't a hollow win against a different bug path.
    assert!(
        cpp.contains("ProducerXact_count(self.src)"),
        "expected `source.count()` to lower to \
         `ProducerXact_count(self.src)`; got:\n{cpp}",
    );
}

/// arch-com#447 §8 regression: `topo_sort_component_indices` used
/// to walk only by-value field-type edges, which under-approximated
/// the real cross-transactor dependency graph. A hookable-call
/// target that wasn't ALSO reachable via a field-type chain produced
/// no edge — source order was the only thing keeping the emitted
/// C++ in dependency order, and any rename or interleave silently
/// reintroduced the issue arch-com#301 / harc-com#309 already
/// solved for the field case.
///
/// This fixture pins down the call-edge rule directly. Both
/// transactors share a referee field (`HelperCommon`) so the
/// field-type rule is symmetric. The only asymmetry is that
/// `CallerXact.observed_count` calls `produce_value()`, a hookable
/// owned solely by `ProducerXact`. The call-edge rule must add an
/// edge `CallerXact -> ProducerXact` even though Caller has no
/// field of type ProducerXact, so the sort returns ProducerXact
/// before CallerXact despite their source order being reversed.
#[test]
fn transactor_topo_sort_honors_hookable_call_edges() {
    use harc::ast::Item;
    use std::fs;
    use std::path::PathBuf;

    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let src = fs::read_to_string(fixtures.join("transactor_hookable_call_graph_edge_test.harc"))
        .expect("read call-graph fixture");
    let parsed = parse_source(&src).expect("parse call-graph fixture");

    // Sanity: CallerXact (the one that calls produce_value) must
    // appear in source BEFORE ProducerXact (the hookable owner).
    // If a future edit reorders the fixture this test would become
    // a tautology — guard with an assertion.
    let name_for = |it: &Item| match it {
        Item::Transactor(t) => Some(t.name.name.clone()),
        _ => None,
    };
    let source_names: Vec<String> = parsed.items.iter().filter_map(name_for).collect();
    let src_caller = source_names
        .iter()
        .position(|n| n == "CallerXact")
        .expect("CallerXact missing");
    let src_producer = source_names
        .iter()
        .position(|n| n == "ProducerXact")
        .expect("ProducerXact missing");
    assert!(
        src_caller < src_producer,
        "fixture lost its source-order trigger: CallerXact must come \
         before ProducerXact in the .harc file. CallerXact at \
         source position {src_caller}, ProducerXact at {src_producer}",
    );

    let order = cpp_tb::topo_sort_component_indices(&parsed);
    let sorted_names: Vec<&str> = order
        .iter()
        .map(|&i| match &parsed.items[i] {
            Item::Transactor(t) => t.name.name.as_str(),
            _ => "<non-transactor>",
        })
        .collect();
    let sort_caller = sorted_names
        .iter()
        .position(|n| *n == "CallerXact")
        .expect("CallerXact missing from sort output");
    let sort_producer = sorted_names
        .iter()
        .position(|n| *n == "ProducerXact")
        .expect("ProducerXact missing from sort output");
    assert!(
        sort_producer < sort_caller,
        "topo sort regressed: ProducerXact must be ordered BEFORE \
         CallerXact because CallerXact.observed_count calls \
         produce_value() (a hookable declared on ProducerXact). \
         Sort produced: {sorted_names:?}. Under-approximated graph \
         would leave them in source order [CallerXact, ProducerXact].",
    );
}

/// Pins the *current conservative behaviour* when a called hookable
/// name has multiple external owners: the call-edge rule (rule 2 in
/// `topo_sort_component_indices`) silently does not add the edge,
/// and the sort falls back to field-edges + source order.
///
/// This locks the test gap §8 in arch-com#463 flagged: without this
/// fixture, a future visitor change that started adding edges to
/// ALL ambiguous owners — or to none at all — would slip through
/// silently. With this fixture in place, both directions trip the
/// test.
///
/// The open design decision (widen dispatch by receiver-type vs
/// emit a compile error on ambiguous unrooted calls) is intentionally
/// left undecided here — the in-source TODO at the dispatch site is
/// the tracking marker. This test only pins what the codegen does
/// *today*, so the decision can be made deliberately later instead
/// of being forced by a regression.
#[test]
fn transactor_topo_sort_skips_ambiguous_hookable_call_edges() {
    use harc::ast::Item;
    use std::fs;
    use std::path::PathBuf;

    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let src =
        fs::read_to_string(fixtures.join("transactor_hookable_call_ambiguous_owner_test.harc"))
            .expect("read ambiguous-owner fixture");
    let parsed = parse_source(&src).expect("parse ambiguous-owner fixture");

    let order = cpp_tb::topo_sort_component_indices(&parsed);
    let sorted_names: Vec<&str> = order
        .iter()
        .map(|&i| match &parsed.items[i] {
            Item::Transactor(t) => t.name.name.as_str(),
            _ => "<non-transactor>",
        })
        .collect();

    // Source order is: CallerXact, HelperA, HelperB. Caller's body
    // calls `produce_value()`, owned by *both* HelperA and HelperB.
    // The call-edge rule skips ambiguous owners. Caller has a field
    // of type HelperA (a field-edge), so the field rule still puts
    // HelperA before Caller. There is NO edge from Caller to
    // HelperB (no field, no resolved call-edge), so HelperB is free
    // to sort in source order — which puts it last.
    let pos = |name: &str| -> usize {
        sorted_names
            .iter()
            .position(|n| *n == name)
            .unwrap_or_else(|| panic!("{name} missing from sort output: {sorted_names:?}"))
    };
    let caller = pos("CallerXact");
    let helper_a = pos("HelperA");
    let helper_b = pos("HelperB");

    // Locked invariant 1: field-edge from Caller to HelperA is
    // honoured (HelperA appears before Caller).
    assert!(
        helper_a < caller,
        "field-edge rule should still order HelperA before CallerXact \
         (Caller has `helper : HelperA`); got: {sorted_names:?}",
    );

    // Locked invariant 2: NO edge from Caller to HelperB. The
    // ambiguous call-edge is silently dropped (current behaviour).
    // If a future change started adding both edges, HelperB would
    // move before Caller and this would fail. If a future change
    // emitted a compile error instead, `topo_sort_component_indices`
    // would never be called and this test would have a different
    // shape — bump it then.
    assert!(
        caller < helper_b,
        "ambiguous-owner call-edge should NOT be added today \
         (rule 2 requires exactly one external owner). With no \
         edge from Caller to HelperB, source order should keep \
         HelperB after CallerXact. Got: {sorted_names:?}. If you \
         intentionally widened the call-edge rule to emit edges \
         for ALL ambiguous owners, see the TODO at \
         `cpp_tb.rs::topo_sort_component_indices` and bump this \
         test deliberately.",
    );
}

#[test]
fn regblock_record_write_read_and_callbacks_lower() {
    // Feature (b): passive address-keyed record API. Feature (c):
    // per-register write callbacks. See docs/ral-support.md §3.2.
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
    register B @ 0x04 access rw
end regblock R

test T
    let dut : SomeDut
    let regs : R = bind helper
    run
        on regs.A
            log(info, "A written")
        end on
        regs.record_write(0x04, 123)
        let x = regs.record_read(0x00)
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    // (b) file-scope decode artifacts: callback holder struct +
    // passive record_read free function owning the address ladder.
    assert!(
        cpp.contains("struct R_Callbacks {")
            && cpp.contains("std::function<void(uint64_t)> A;")
            && cpp.contains("std::function<void(uint64_t)> B;"),
        "expected R_Callbacks struct with a slot per register; got:\n{cpp}"
    );
    assert!(
        cpp.contains("static inline uint64_t R_record_read(const R_Mirror& m, uint64_t addr)"),
        "expected generated R_record_read decode function; got:\n{cpp}"
    );

    // Callback holder instantiated next to the mirror at the binding.
    assert!(
        cpp.contains("R_Callbacks regs_cbs;"),
        "expected per-binding callback holder; got:\n{cpp}"
    );

    // (c) `on regs.A ... end on` registers a void(uint64_t data) closure.
    assert!(
        cpp.contains("regs_cbs.A = [&](uint64_t data) {"),
        "expected per-register write callback registration; got:\n{cpp}"
    );

    // (b) record_write decodes the address and updates the matching
    // mirror cell (masked to width), then fires the register's callback.
    assert!(
        cpp.contains("regs.B = (uint32_t)(_rec_data & 0xffffffffull);")
            && cpp.contains("if (regs_cbs.B) regs_cbs.B(_rec_data);"),
        "expected record_write mirror update + callback dispatch for B; got:\n{cpp}"
    );

    // (b) record_read lowers to a call into the generated decode.
    assert!(
        cpp.contains("R_record_read(regs, (uint64_t)("),
        "expected record_read to call the generated decode; got:\n{cpp}"
    );
}

#[test]
fn regblock_active_frontdoor_write_does_not_dispatch_callback() {
    // Locks the documented active/passive asymmetry from silent
    // drift: `regs.NAME = expr` (active path) updates the mirror and
    // writes to the bus, but does NOT fire `on regs.NAME` callbacks.
    // Only `regs.record_write(addr, data)` (passive path) dispatches
    // them. See docs/ral-support.md §3.2 and the comments at the two
    // active sites in `cpp_tb.rs::emit_assign`.
    //
    // Repro pattern: register a callback AND drive an active write
    // to the same register. The emitted active write must NOT
    // contain `_cbs.<name>` callback dispatch. Closes §6 from
    // arch-com#463.
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
end regblock R

test T
    let dut : SomeDut
    let regs : R = bind helper
    run
        on regs.A
            log(info, "A written")
        end on
        regs.A = 1
        regs.record_write(0x00, 2)
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    // Sanity: the callback is registered, the active write updates
    // the mirror, the passive record_write dispatches the callback.
    assert!(
        cpp.contains("regs_cbs.A = [&](uint64_t data) {"),
        "expected `on regs.A` to register a callback closure; got:\n{cpp}"
    );
    assert!(
        cpp.contains("regs.A = 1;"),
        "expected the active write `regs.A = 1` to emit a mirror \
         update; got:\n{cpp}"
    );
    assert!(
        cpp.contains("if (regs_cbs.A) regs_cbs.A(_rec_data);"),
        "expected the passive record_write to dispatch the A callback; got:\n{cpp}"
    );

    // The asymmetry: count the `regs_cbs.A(...)` dispatch occurrences.
    // There must be exactly one — fired by `record_write`, NOT by
    // the active assign. If a future codegen edit accidentally adds
    // dispatch to the active path, this count rises to 2 and the
    // test fails loud. (The registration site `regs_cbs.A = [&]...`
    // uses `=`, not `(`, so it is structurally different and not
    // counted here.)
    let dispatch_count = cpp.matches("regs_cbs.A(_rec_data)").count();
    assert_eq!(
        dispatch_count, 1,
        "expected exactly one `regs_cbs.A(_rec_data)` dispatch site \
         (the passive record_write decode); found {dispatch_count}. \
         The active frontdoor write path must NOT dispatch callbacks \
         per docs/ral-support.md §3.2. Full output:\n{cpp}"
    );
}

#[test]
fn regblock_record_write_emits_recursion_guard() {
    // record_write callbacks can re-enter the decode block. The
    // codegen wraps each decode in a per-binding depth counter and
    // aborts (FATAL) if `HARC_RAL_CB_MAX_DEPTH` is exceeded. Guards
    // a self-write (`on regs.A { regs.record_write(0x00, ...) }`)
    // from blowing the stack. See docs/ral-support.md §3.2.
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
    register B @ 0x04 access rw
end regblock R

test T
    let dut : SomeDut
    let regs : R = bind helper
    run
        on regs.A
            regs.record_write(0x00, data)
        end on
        regs.record_write(0x00, 1)
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    // Depth-limit constant is emitted once per file when any
    // regblock exists. Guarded with #ifndef so a TU with multiple
    // generated TBs doesn't redefine.
    assert!(
        cpp.contains("#ifndef HARC_RAL_CB_MAX_DEPTH")
            && cpp.contains("static constexpr uint32_t HARC_RAL_CB_MAX_DEPTH = 16;"),
        "expected HARC_RAL_CB_MAX_DEPTH constant emitted with #ifndef guard; got:\n{cpp}"
    );

    // Per-binding depth counter initialized alongside the cbs holder.
    assert!(
        cpp.contains("uint32_t regs_cb_depth = 0;"),
        "expected per-binding `regs_cb_depth` counter; got:\n{cpp}"
    );

    // Decode block bumps the counter, runs the chain, decrements,
    // and FATALs when the limit is hit.
    assert!(
        cpp.contains("if (regs_cb_depth >= HARC_RAL_CB_MAX_DEPTH)"),
        "expected depth-limit check before decode; got:\n{cpp}"
    );
    assert!(
        cpp.contains("regs_cb_depth++;") && cpp.contains("regs_cb_depth--;"),
        "expected depth-counter bump/unbump around decode; got:\n{cpp}"
    );
    assert!(
        cpp.contains("sim_log_line(\"FATAL\", \"RAL record_write callback recursion exceeded HARC_RAL_CB_MAX_DEPTH"),
        "expected FATAL log on depth overflow; got:\n{cpp}"
    );
    assert!(
        cpp.contains("errors++; _fatal = true;"),
        "expected errors++ / _fatal = true on overflow path (mirrors log(fatal, ...) semantics); got:\n{cpp}"
    );
}

/// Structural counterpart to `regblock_record_write_emits_recursion_guard`.
/// The substring checks in that test confirm the right pieces appear
/// *somewhere* in the emitted C++, but they don't pin down how the
/// pieces fit together — two regression classes the substring view
/// silently accepts:
///
/// 1. **Off-by-one in the comparison**: flipping `>=` to `>` keeps
///    every substring intact but raises the effective cap from 16 to
///    17 (depth 16 no longer aborts; depth 17 does). Pinned here by
///    matching the literal `regs_cb_depth >= HARC_RAL_CB_MAX_DEPTH`
///    token — any rewrite of the operator breaks the assertion.
///
/// 2. **Unbump escaping the else arm**: moving `regs_cb_depth--;`
///    out of the `else { ... }` body makes the decrement run on the
///    FATAL path too, leaving the counter in a stale state after a
///    legitimate-but-recursive callback chain aborts mid-flight.
///    Pinned here by asserting that the FATAL line's `} else {` head
///    sits *before* the `regs_cb_depth--;` in the source, and that
///    after the FATAL block there is no top-level `regs_cb_depth--;`
///    floating outside an `else` arm.
///
/// Compile-to-string only — paired with the
/// `regblock_record_recursion_test` fixture (run via
/// `tests/run_negative_fixtures.sh`) which exercises the same path
/// at runtime.
#[test]
fn regblock_record_write_recursion_guard_structure() {
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
end regblock R

test T
    let dut : SomeDut
    let regs : R = bind helper
    run
        on regs.A
            regs.record_write(0x00, data)
        end on
        regs.record_write(0x00, 1)
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    // (1) Comparison operator is exactly `>=`. Catches `>` (cap
    // shifts from 16 to 17) and `<` / `==` / etc.
    assert!(
        cpp.contains("if (regs_cb_depth >= HARC_RAL_CB_MAX_DEPTH)"),
        "expected literal `>=` comparison against the depth cap; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("if (regs_cb_depth > HARC_RAL_CB_MAX_DEPTH)"),
        "found `>` comparison — should be `>=`; got:\n{cpp}"
    );

    // (2) Unbump (`regs_cb_depth--;`) must sit *strictly inside* the
    // success `else { ... }` arm — not at the same indentation as
    // the `if/else` header (which would mean it runs on both the
    // FATAL path and the success path, leaking decrements onto the
    // abort path and breaking the counter after a legitimate
    // recursive cascade aborts).
    //
    // We pin this by lining up indentations: for every line that
    // contains `regs_cb_depth--;`, its leading-whitespace prefix
    // must be *strictly deeper* than the leading-whitespace prefix
    // of the closest preceding line containing the
    // `if (regs_cb_depth >= HARC_RAL_CB_MAX_DEPTH)` guard header
    // (which is also the `} else {` line in single-line emission).
    let total_unbumps = cpp.matches("regs_cb_depth--;").count();
    let total_bumps = cpp.matches("regs_cb_depth++;").count();
    assert_eq!(
        total_unbumps, total_bumps,
        "bump/unbump must be balanced; got bumps={total_bumps} unbumps={total_unbumps}:\n{cpp}"
    );
    assert!(
        total_unbumps >= 1,
        "expected at least one decode site with bump/unbump:\n{cpp}"
    );

    fn leading_ws(s: &str) -> usize {
        s.chars().take_while(|c| *c == ' ' || *c == '\t').count()
    }

    let lines: Vec<&str> = cpp.lines().collect();
    let mut current_guard_indent: Option<usize> = None;
    let mut current_guard_line: Option<usize> = None;
    let mut last_bump_line: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.contains("if (regs_cb_depth >= HARC_RAL_CB_MAX_DEPTH)") {
            current_guard_indent = Some(leading_ws(line));
            current_guard_line = Some(i);
            last_bump_line = None;
        }
        if line.contains("regs_cb_depth++;") {
            last_bump_line = Some(i);
            let g_ind = current_guard_indent.expect(
                "saw `regs_cb_depth++;` before any guard header — bump leaked above the guard",
            );
            let b_ind = leading_ws(line);
            assert!(
                b_ind > g_ind,
                "bump on line {} (indent {}) is not deeper than guard header on line {} (indent {}) — \
                 bump leaked out of the success else arm:\n{}",
                i + 1,
                b_ind,
                current_guard_line.unwrap() + 1,
                g_ind,
                cpp
            );
        }
        if line.contains("regs_cb_depth--;") {
            let g_ind = current_guard_indent.expect(
                "saw `regs_cb_depth--;` before any guard header — unbump leaked above the guard",
            );
            let u_ind = leading_ws(line);
            assert!(
                u_ind > g_ind,
                "unbump on line {} (indent {}) is not deeper than guard header on line {} (indent {}) — \
                 unbump leaked out of the success else arm (would also run on FATAL path):\n{}",
                i + 1,
                u_ind,
                current_guard_line.unwrap() + 1,
                g_ind,
                cpp
            );
            let bump_line = last_bump_line.expect(
                "saw `regs_cb_depth--;` without a preceding `regs_cb_depth++;` in the same decode block",
            );
            assert!(
                bump_line < i,
                "bump on line {} must precede unbump on line {} within the same decode block:\n{}",
                bump_line + 1,
                i + 1,
                cpp
            );
        }
    }
}

#[test]
fn regblock_record_write_callback_cross_register_chain() {
    // A legal cross-register cascade (A -> B -> C) at depth 3
    // should compile cleanly: each callback simply invokes the
    // same decode (now guarded by the depth counter) on a
    // different address. Compile-to-string only — running the
    // sim end-to-end is out of scope here.
    let parsed = parse_source(
        r#"regblock R via H width 32
    register A @ 0x00 access rw
    register B @ 0x04 access rw
    register C @ 0x08 access rw
end regblock R

test T
    let dut : SomeDut
    let regs : R = bind helper
    run
        on regs.A
            regs.record_write(0x04, data)
        end on
        on regs.B
            regs.record_write(0x08, data)
        end on
        on regs.C
            log(info, "C settled")
        end on
        regs.record_write(0x00, 7)
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    // All three callbacks register against the single binding.
    assert!(
        cpp.contains("regs_cbs.A = [&](uint64_t data) {")
            && cpp.contains("regs_cbs.B = [&](uint64_t data) {")
            && cpp.contains("regs_cbs.C = [&](uint64_t data) {"),
        "expected three callback registrations; got:\n{cpp}"
    );

    // One depth counter shared across the chain (per-binding, not
    // per-register) — confirms the guard scope is correct so that
    // a legal depth-3 cascade against the same binding all bumps
    // the same counter and stays under the 16-cap.
    let counter_decls = cpp.matches("uint32_t regs_cb_depth = 0;").count();
    assert_eq!(
        counter_decls, 1,
        "expected exactly one depth counter for the binding; got {counter_decls}:\n{cpp}"
    );

    // Bump/unbump pairs appear once per decode call site. The body
    // has 3 callback bodies each containing a `record_write`, plus
    // the top-level `record_write`, so 4 bump+unbump pairs.
    let bumps = cpp.matches("regs_cb_depth++;").count();
    let unbumps = cpp.matches("regs_cb_depth--;").count();
    assert_eq!(
        bumps, unbumps,
        "bump/unbump counts must balance; got bumps={bumps} unbumps={unbumps}:\n{cpp}"
    );
    assert!(
        bumps >= 3,
        "expected at least 3 decode-site bumps for the A->B->C chain; got {bumps}:\n{cpp}"
    );
}

/// Exercises the `w >= 64` branch in `try_emit_record_write` at
/// `cpp_tb.rs:14436`. For widths < 64 the mask is `(1u64 << w) - 1`;
/// for width 64 the shift would overflow, so the codegen branches to
/// `u64::MAX` (`0xffffffffffffffffull`). Without a width-64 register
/// anywhere in the test corpus this branch was unexercised.
///
/// Also pins the mirror-field C type: a 64-bit register lowers to
/// `uint64_t` storage (`mirror_field_c_type` width buckets at
/// `cpp_tb.rs:17431`).
#[test]
fn regblock_record_write_width_64_uses_u64_max_mask() {
    let parsed = parse_source(
        r#"regblock R via H width 32
    register WIDE @ 0x10 width 64 access rw
end regblock R

test T
    let dut : SomeDut
    let regs : R = bind helper
    run
        regs.record_write(0x10, 0xdeadbeefcafebabe)
    end run
end test T"#,
    )
    .unwrap();
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let cpp = cpp_tb::emit(&merged).expect("emit");

    assert!(
        cpp.contains("regs.WIDE = (uint64_t)(_rec_data & 0xffffffffffffffffull);"),
        "expected width-64 record_write mirror update with u64::MAX mask; got:\n{cpp}"
    );
}

// ── v1 ↔ TB-IR parity fixes (review of #543) ─────────────────────────
//
// Four defects where the legacy v1 emitter and the TB-IR emitter
// disagreed. Every one produced a wrong value, a non-compiling
// translation unit, or an `internal error` for a program `harc check`
// accepted, so each is pinned against the shape both backends now emit.
// The TB-IR side of each lives in `tests/tbir.rs`.

fn v1_cpp(src: &str) -> String {
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    cpp_tb::emit(&merged).expect("emit")
}

/// The v1 emitter's diagnostic for a source it refuses. Same path as
/// `v1_cpp`, asserting the emit FAILS rather than succeeds.
fn v1_cpp_err(src: &str) -> String {
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    cpp_tb::emit(&merged)
        .expect_err("emit should have been rejected")
        .0
}

/// The constraint text the **TB-IR** backend emits for each randomize
/// site. The §2.4 wrap mask lives in a randomize emitter both backends
/// call, but they construct it differently — the TB-IR one is built
/// per-site with no per-test statement state. A v1-only assertion
/// therefore proves nothing about the default backend, which is how a
/// blocking-operand regression reached the default path unnoticed.
fn tbir_constraint_snippets(src: &str) -> Result<String, String> {
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let prog = harc::ir::lower::lower_program(&merged).map_err(|e| e.to_string())?;
    let opts = harc::codegen::cpp_tb::EmitOpts::default();
    cpp_tb::emit_randomize_snippets(&merged, &opts, &prog.constraint_sites, 5)
        .map(|snippets| snippets.join("\n"))
        .map_err(|e| e.0.to_string())
}

fn v1_emit_err(src: &str) -> String {
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    match cpp_tb::emit(&merged) {
        Ok(_) => panic!("expected emission to fail"),
        Err(e) => e.0.to_string(),
    }
}

/// `a +% b` masks to `max(W(a), W(b))` bits (harc#473). v1 treated the
/// wrapping operators as pass-through sugar for `+ - *`, so a `uint<8>`
/// overflow produced 300 where TB-IR produced 44.
#[test]
fn v1_wrapping_operators_mask_to_the_operand_width() {
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 200
        let b = a +% 100
        let c = a -% 250
        let d = a *% 3
        log(info, "${b} ${c} ${d}")
    end run
end test T"#,
    );
    for want in [
        "((uint64_t)(((uint64_t)((a + 100)) & 0xFFULL)))",
        "((uint64_t)(((uint64_t)((a - 250)) & 0xFFULL)))",
        "((uint64_t)(((uint64_t)((a * 3)) & 0xFFULL)))",
    ] {
        assert!(cpp.contains(want), "expected `{want}`; got:\n{cpp}");
    }
}

/// The mask is the unsigned low-W residue — check the arithmetic, not
/// just the spelling, by compiling what the emitter actually produced.
/// Extracting the expressions from the emitted C++ (rather than
/// hard-coding them) is what makes this fail if the emitter regresses;
/// a hard-coded copy would pass against any emitter at all.
#[test]
fn v1_wrapping_mask_computes_the_wrapped_value() {
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 200
        let b = a +% 100
        let c = a -% 250
        let d = a *% 3
        log(info, "${b} ${c} ${d}")
    end run
end test T"#,
    );
    let expr = |lhs: &str| {
        let needle = format!("{lhs} = ");
        let start = cpp.find(&needle).expect("assignment present") + needle.len();
        let rest = &cpp[start..];
        rest[..rest.find(';').expect("statement ends")].to_string()
    };
    compile_and_run_runtime_cpp(
        "wrap_mask",
        &format!(
            "uint64_t a = 200;\n\
             assert(({}) == 44);\n\
             assert(({}) == 206);\n\
             assert(({}) == 88);",
            expr("b"),
            expr("c"),
            expr("d"),
        ),
    );
}

/// A plain top-level port is width-erased, so a wrap on one has no
/// defined mask width and is rejected rather than silently not wrapping.
/// Asserted for BOTH backends: a rejection only v1 makes would be an
/// accepted-set divergence, which is the thing these fixes exist to close.
#[test]
fn both_backends_reject_an_unknown_width_wrapping_operand() {
    let src = r#"test T
    let dut : Top
    run
        let x = dut.count +% 1
        log(info, "${x}")
    end run
end test T"#;
    let v1_err = v1_emit_err(src);
    assert!(
        v1_err.contains("statically known bit-width"),
        "v1: expected the unknown-width wrap diagnostic, got: {v1_err}"
    );
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let tbir_err = harc::ir::lower::lower_program(&merged)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        tbir_err.contains("statically known bit-width"),
        "tbir: expected the same diagnostic, got: {tbir_err}"
    );
}

/// A `probe` read is the one `dut.<field>` shape that carries a static
/// width. TB-IR reads it off `PortRef::width`; v1 must resolve the same
/// width or it rejects a wrap the default backend accepts.
#[test]
fn v1_wraps_a_probe_operand_at_its_declared_width() {
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
        probe c : uint<8> at ctr.count_r
    end let dut
    run
        let x = dut.c +% 1
        log(info, "${x}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("+ 1)) & 0xFFULL"),
        "expected the wrap to mask at the probe's 8-bit width; got:\n{cpp}"
    );
}

/// `sext<N>` with nothing to extend is still a signed relabel. Casting
/// through `uint64_t` made `s.sext<64>() > 0` true for a negative
/// `sint<64>` — the opposite of TB-IR's verdict on the same source.
#[test]
fn v1_sext_without_extension_keeps_the_value_signed() {
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let s : sint<64> = 0 - 1
        assert s.sext<64>() > 0 else fail("negative")
    end run
end test T"#,
    );
    // The inner `(uint64_t)` narrows before the signed relabel so a
    // `HarcWide` receiver is not an ambiguous conversion; the outer
    // `int64_t` is what makes the result signed.
    assert!(
        cpp.contains("((int64_t)((uint64_t)(s)))"),
        "expected a signed relabel; got:\n{cpp}"
    );
}

/// A narrowing initializer is rejected instead of emitting
/// `HarcWide<7> b = a;`, which does not compile.
#[test]
fn v1_rejects_narrowing_assignment() {
    for (src, want) in [
        (
            r#"test T
    let dut : Top
    run
        let a : uint<256> = 5
        let b : uint<200> = a
        log(info, "${b}")
    end run
end test T"#,
            "use `.trunc<200>()`",
        ),
        (
            r#"test T
    let dut : Top
    run
        let a : uint<32> = 5
        let b : uint<8> = 0
        b = a
        log(info, "${b}")
    end run
end test T"#,
            "use `.trunc<8>()`",
        ),
    ] {
        let err = v1_emit_err(src);
        assert!(
            err.contains("narrows") && err.contains(want),
            "expected a narrowing diagnostic containing `{want}`, got: {err}"
        );
    }
}

/// The full-width sign fill must be spelled `int64_t`, not `uint64_t`.
/// v1 binds it to `auto`, so an unsigned spelling deduced the opposite
/// signedness from TB-IR's `int64_t` local and flipped `> 0`, `/`, and
/// `>>` on the same source. Nothing else in the suite pins this.
#[test]
fn v1_full_width_sign_fill_is_signed() {
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let p : uint<32> = 0xAB
        let x = p[7:0].sext<64>()
        assert x > 0 else fail("neg")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("((int64_t)(((int64_t)("),
        "the width-64 fill must be signed so `auto` deduces int64_t; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("((uint64_t)(((int64_t)("),
        "an unsigned outer cast makes `auto` deduce uint64_t; got:\n{cpp}"
    );
}

/// A bare integer literal is not a narrowing source: v1 must not reject
/// `let b : uint<8> = 300` while TB-IR (which types a bare literal as
/// widthless) accepts it. Reverting the exclusion re-introduces exactly
/// that accepted-set divergence, silently.
#[test]
fn a_bare_literal_initializer_is_not_a_narrowing_error() {
    for src in [
        r#"test T
    let dut : Top
    run
        let b : uint<8> = 300
        log(info, "${b}")
    end run
end test T"#,
        r#"test T
    let dut : Top
    run
        let z : uint<0> = 0
        log(info, "${z}")
    end run
end test T"#,
    ] {
        let parsed = parse_source(src).expect("parses");
        let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
        assert!(
            cpp_tb::emit(&merged).is_ok(),
            "v1 must accept a bare-literal initializer, as TB-IR does"
        );
        assert!(
            harc::ir::lower::lower_program(&merged).is_ok(),
            "TB-IR must accept it too — the two accepted sets have to match"
        );
    }
}

/// `probe_widths` is keyed by bare probe name, so the wrap-operand lookup
/// must check the base is `dut`. Without that guard a same-named record
/// field masked at the probe's width — a silently wrong value — and the
/// map must also be cleared per test or it leaks into a probe-less one.
#[test]
fn probe_width_lookup_does_not_capture_same_named_fields() {
    let collide = r#"transaction Txn
    count : uint<32> default 0
end transaction Txn
test ProbeCollide
    let dut : Top
        probe count : uint<8> at inner.count
    end let dut
    run
        let t : Txn
        t.count = 250
        let s : uint<32> = t.count +% 10
        log(info, "${s}")
    end run
end test ProbeCollide"#;
    let err = v1_emit_err(collide);
    assert!(
        err.contains("statically known bit-width"),
        "a record field must not resolve to a same-named probe's width; got: {err}"
    );

    // Second test has no probes: the first test's entry must not leak.
    let leak = r#"test AProbe
    let dut : Top
        probe count : uint<8> at inner.count
    end let dut
    run
        let x = dut.count +% 1
        log(info, "${x}")
    end run
end test AProbe

test BPort
    let dut : Top
    run
        let y = dut.count +% 1
        log(info, "${y}")
    end run
end test BPort"#;
    let err = v1_emit_err(leak);
    assert!(
        err.contains("statically known bit-width"),
        "probe widths must not leak across tests; got: {err}"
    );
}

/// A literal too wide for 64 bits is a genuine narrowing source, unlike
/// one that fits. v1 used to accept `let b : uint<8> = <80-bit literal>`
/// and store the low 64 bits; TB-IR reported it. Both must reject, while
/// still accepting a literal that merely exceeds the declared width.
#[test]
fn wide_literal_initializer_is_a_narrowing_error_in_both_backends() {
    let src = r#"test T
    let dut : Top
    run
        let b : uint<8> = 0xFFFFFFFFFFFFFFFFFFFF
        log(info, "${b}")
    end run
end test T"#;
    let err = v1_emit_err(src);
    assert!(
        err.contains("narrows"),
        "v1 must reject an 80-bit literal into a uint<8>; got: {err}"
    );
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let tbir = harc::ir::lower::lower_program(&merged)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        tbir.contains("narrows"),
        "TB-IR must reject it too; got: {tbir}"
    );
}

/// v1's wrap-operand inference must recognise every shape TB-IR's does,
/// or the "reject the same operands" claim in spec.md §2.4 is false. Each
/// of these was a v1-only rejection.
#[test]
fn v1_wrap_operand_inference_matches_tbir_on_cast_and_bit_probes() {
    // A width-less scalar cast is 64 bits, not unknown.
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 200
        let x = (a as uint) +% 100
        log(info, "${x}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("+ 100)))"),
        "a width-less cast operand must wrap at 64 bits, not be rejected; got:\n{cpp}"
    );

    // The capitalised ARCH spellings count too — this half was previously
    // unpinned, and deleting `UIntCap | SIntCap` left the whole suite green.
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 200
        let x = (a as UInt<8>) +% 100
        log(info, "${x}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("& 0xFFULL"),
        "a capitalised cast operand must wrap at its declared 8 bits; got:\n{cpp}"
    );

    // A non-literal width argument is unknown, NOT 64 — `cast_relabel_width`
    // distinguishes "no argument" from "argument we cannot fold", and
    // collapsing both to 64 made v1 wrap at 64 where TB-IR rejects.
    let err = v1_emit_err(
        r#"const W : uint<32> = 8
test T
    let dut : Top
    run
        let a : uint<8> = 200
        let x = (a as uint<W>) +% 100
        log(info, "${x}")
    end run
end test T"#,
    );
    assert!(
        err.contains("statically known bit-width"),
        "a non-literal cast width must be unknown, matching TB-IR; got: {err}"
    );

    // A `Bit` probe is one bit wide, not width-less.
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
        probe flag : Bit at inner.f
    end let dut
    run
        let x = dut.flag +% 1
        log(info, "${x}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("& 0x1ULL"),
        "a Bit probe operand must wrap at 1 bit; got:\n{cpp}"
    );
}

/// ARCH sized literals (`8'hFF`) are a v1-only form — TB-IR rejects them
/// and names `--codegen v1` as the escape hatch — so the narrowing check
/// must not decode them as some invented width. An earlier version
/// returned a hard-coded 129 bits for any text it could not parse, which
/// rejected every one of them.
#[test]
fn sized_literals_are_not_narrowing_sources() {
    for lit in ["8'hFF", "4'b1010", "8'd42"] {
        let src = format!(
            r#"test T
    let dut : Top
    run
        let b : uint<8> = {lit}
        log(info, "${{b}}")
    end run
end test T"#
        );
        let parsed = parse_source(&src).expect("parses");
        let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
        assert!(
            cpp_tb::emit(&merged).is_ok(),
            "v1 must still accept the sized literal `{lit}`"
        );
    }
}

/// The reported width must be the literal's true bit count, and must match
/// TB-IR's, or the same program yields two different numbers in the same
/// diagnostic. The earlier 129-bit sentinel also blinded the check to a
/// >128-bit literal narrowing into a wide-but-smaller local.
#[test]
fn wide_literal_widths_agree_between_backends() {
    for (lit, decl, bits) in [
        ("0xFFFFFFFFFFFFFFFFFFFF", 8u32, "80-bit"),
        ("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF", 8, "160-bit"),
        ("0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF", 140, "160-bit"),
    ] {
        let src = format!(
            r#"test T
    let dut : Top
    run
        let b : uint<{decl}> = {lit}
        log(info, "${{b}}")
    end run
end test T"#
        );
        let parsed = parse_source(&src).expect("parses");
        let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
        let v1 = match cpp_tb::emit(&merged) {
            Ok(_) => panic!("v1 must reject {lit} into uint<{decl}>"),
            Err(e) => e.0.to_string(),
        };
        let tbir = harc::ir::lower::lower_program(&merged)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            v1.contains(bits) && tbir.contains(bits),
            "both backends must report {bits}; v1: {v1}\ntbir: {tbir}"
        );
    }
}

/// The wrap-operand width rule (capped at 128, 0 rejected) must NOT leak
/// into the direction checks or the `sext` emission shape, which need the
/// raw declared width. Delegating all three to `cast_relabel_width` turned
/// `(big as uint<200>).sext<300>()` into a zero-extension — a silent value
/// change with no diagnostic — and dropped the wrong-direction rejection.
#[test]
fn casts_above_128_bits_keep_their_raw_width_for_sext_and_direction_checks() {
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let big : uint<200> = 1
        let x = (big as uint<200>).sext<300>()
        log(info, "${x}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("harc_wide_sext<10>") && cpp.contains(", 200, 300)"),
        "a >128-bit cast source must still sign-fill from its declared width; got:\n{cpp}"
    );

    // Same width, wrong direction: the raw width is what makes this an error.
    let err = v1_emit_err(
        r#"test T
    let dut : Top
    run
        let big : uint<200> = 1
        let y = (big as uint<129>).zext<100>()
        log(info, "${y}")
    end run
end test T"#,
    );
    assert!(
        err.contains("on a 129-bit value"),
        "a >128-bit cast source must still drive the direction check; got: {err}"
    );
}

/// v1's `let_widths` is keyed by bare name with no scoping, so an inner
/// shadow clobbers the outer name's recorded width. The direction checks
/// have always lived with that; the narrowing check must not, or a legal
/// `let b : uint<8> = a` after an inner `let a : uint<64>` is rejected on
/// a width `a` never had at that point — which TB-IR, keyed by local id,
/// accepts.
#[test]
fn a_shadowed_name_does_not_trigger_the_narrowing_check() {
    for src in [
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 1
        if a == 1
            let a : uint<64> = 2
            log(info, "inner=${a}")
        end if
        let b : uint<8> = a
        log(info, "${b}")
    end run
end test T"#,
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 1
        if a == 1
            let a : uint<64> = 2
        end if
        let b : uint<8> = 0
        b = a
        log(info, "${b}")
    end run
end test T"#,
    ] {
        let parsed = parse_source(src).expect("parses");
        let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
        assert!(
            cpp_tb::emit(&merged).is_ok(),
            "v1 must not reject on a shadowed name's width"
        );
        assert!(
            harc::ir::lower::lower_program(&merged).is_ok(),
            "TB-IR accepts it, so v1 must too"
        );
    }
}

/// The wrap-operand path uses the CAPPED cast rule while the direction
/// checks use the raw one. Only the capped side rejects a zero-width
/// cast, so without this the half of the split that differs from raw is
/// untested — and it is the half most likely to be re-collapsed.
#[test]
fn wrap_operand_cast_width_uses_the_capped_rule() {
    let err = v1_emit_err(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 200
        let x = (a as uint<0>) +% 1
        log(info, "${x}")
    end run
end test T"#,
    );
    assert!(
        err.contains("statically known bit-width"),
        "a zero-width cast operand must be unknown for wrapping, matching TB-IR; got: {err}"
    );
}

/// A top-level non-test `extend` survives `merge_for_sim` intact.
///
/// `merge_for_sim` moves items out of the parsed files rather than cloning
/// them, and the `Item::Extend` arm has to split by body kind: a
/// `ExtendBody::Test` is folded into its base test, anything else passes
/// through untouched. No fixture reaches that pass-through branch — the
/// only non-test `extend` in the tree (`axi_agent.harc`) is nested inside
/// a `package`, so it arrives as an `Item::Package` and takes the
/// catch-all arm instead. This covers the branch directly.
#[test]
fn top_level_non_test_extend_passes_through_merge() {
    let src = r#"
transaction Txn
    addr : uint<8>
end transaction Txn

extend Txn
    data : uint<8>
end extend Txn

test T
    let dut : Top
    run
        wait 1 cycle
    end run
end test T
"#;
    let parsed = parse_source(src).expect("parses");
    let extends_before = parsed
        .items
        .iter()
        .filter(|i| matches!(i, harc::ast::Item::Extend(_)))
        .count();
    assert_eq!(
        extends_before, 1,
        "source should declare one top-level extend"
    );

    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let extends_after: Vec<_> = merged
        .items
        .iter()
        .filter_map(|i| match i {
            harc::ast::Item::Extend(e) => Some(e),
            _ => None,
        })
        .collect();

    assert_eq!(
        extends_after.len(),
        1,
        "a non-test extend must survive the merge, not be folded or dropped"
    );
    assert_eq!(
        extends_after[0]
            .target
            .segments
            .last()
            .map(|s| s.name.as_str()),
        Some("Txn"),
        "the pass-through must preserve the extend's target"
    );
    assert!(
        !matches!(extends_after[0].body, harc::ast::ExtendBody::Test(_)),
        "body kind must be preserved"
    );
}

// ── #550: residual v1 / TB-IR divergences ────────────────────────────

/// A wrap's residue is unsigned (spec §2.4). v1 defaulted an untyped
/// `let` to `int64_t`, reinterpreting it as signed, so `let y = a -% 1`
/// on a `uint<64>` made `y > 0` false under v1 and true under TB-IR.
#[test]
fn untyped_let_bound_to_a_wrap_is_unsigned() {
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let a : uint<64> = 0xFFFFFFFFFFFFFFFF
        let y = a -% 1
        log(info, "${y}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("auto y = ((uint64_t)(") && !cpp.contains("int64_t y ="),
        "an untyped wrap destination must deduce unsigned; got:\n{cpp}"
    );
}

/// The wrap's residue is `max(W(lhs), W(rhs))` bits, so a narrower typed
/// destination narrows. TB-IR rejected it; v1 stored the unmasked residue.
#[test]
fn narrower_destination_for_a_wrap_is_rejected() {
    let err = v1_emit_err(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 200
        let x : uint<4> = a +% 1
        log(info, "${x}")
    end run
end test T"#,
    );
    assert!(
        err.contains("narrows") && err.contains(".trunc<4>()"),
        "an 8-bit residue into a 4-bit local must be a narrowing error; got: {err}"
    );
}

/// The residue is unsigned, so a signed destination is a signedness
/// mismatch. TB-IR surfaced this as `internal error: TB-IR failed
/// verification after lowering`; v1 accepted it silently. Both now give
/// the same diagnostic.
#[test]
fn signed_destination_for_a_wrap_is_rejected_in_both_backends() {
    let src = r#"test T
    let dut : Top
    run
        let a : sint<8> = 100
        let b : sint<8> = 100
        let s : sint<8> = a +% b
        log(info, "${s}")
    end run
end test T"#;
    let v1 = v1_emit_err(src);
    assert!(
        v1.contains("Signedness must match"),
        "v1: expected a signedness diagnostic; got: {v1}"
    );
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let tbir = harc::ir::lower::lower_program(&merged)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        tbir.contains("Signedness must match"),
        "tbir: expected the same diagnostic, not an internal error; got: {tbir}"
    );
}

/// The wrap arm of the narrowing check reads `let_widths` through
/// `wrap_result_width`, so it must honour the shadowed-name guard too —
/// otherwise it simply bypasses it and re-opens the false rejection the
/// guard exists to prevent.
#[test]
fn a_shadowed_name_in_a_wrap_operand_does_not_trigger_the_narrowing_check() {
    let src = r#"test T
    let dut : Top
    run
        let a : uint<8> = 1
        if a == 1
            let a : uint<64> = 2
        end if
        let b : uint<8> = a +% 1
        log(info, "${b}")
    end run
end test T"#;
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    assert!(
        cpp_tb::emit(&merged).is_ok(),
        "v1 must not reject a wrap on a shadowed name"
    );
    assert!(
        harc::ir::lower::lower_program(&merged).is_ok(),
        "TB-IR accepts it, so v1 must too"
    );
}

/// The signed-destination rejection must see through parentheses and fire
/// for the capitalised spelling — both escaped the first attempt while
/// TB-IR rejected them.
///
/// The ASSIGNMENT form (`s = a +% b`) is deliberately not covered: doing
/// so needs the destination's declared signedness at the assignment site,
/// and the only way v1 can supply that today is another flat name-keyed
/// table — which false-rejected on a shadowed name and on an unrelated
/// same-named local in any function emitted before the run body. A missed
/// rejection is preferable to rejecting valid source; tracked in spec
/// §2.4 and #548.
#[test]
fn signed_wrap_destination_is_rejected_in_every_spelling() {
    for (label, src) in [
        (
            "parenthesised",
            r#"test T
    let dut : Top
    run
        let a : sint<8> = 100
        let s : sint<8> = (a +% a)
        log(info, "${s}")
    end run
end test T"#,
        ),
        (
            "capitalised SInt",
            r#"test T
    let dut : Top
    run
        let a : sint<8> = 100
        let s : SInt<8> = a +% a
        log(info, "${s}")
    end run
end test T"#,
        ),
    ] {
        let err = v1_emit_err(src);
        assert!(
            err.contains("Signedness must match"),
            "[{label}] v1 must reject; got: {err}"
        );
    }
}

/// The suggested relabel must actually parse — `as sint<N>()` with
/// parentheses is a syntax error.
#[test]
fn the_suggested_signedness_relabel_parses() {
    let src = r#"test T
    let dut : Top
    run
        let a : sint<8> = 100
        let s : sint<8> = (a +% a) as sint<8>
        log(info, "${s}")
    end run
end test T"#;
    let parsed = parse_source(src).expect("the suggested spelling must parse");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    assert!(cpp_tb::emit(&merged).is_ok(), "and must emit");
}

/// An untyped `let` bound to a *parenthesised* wrap must also deduce the
/// unsigned type — one pair of parentheses defeated the first attempt,
/// leaving the width-64 signedness divergence reachable.
#[test]
fn untyped_let_bound_to_a_parenthesised_wrap_is_unsigned() {
    let cpp = v1_cpp(
        r#"test T
    let dut : Top
    run
        let a : uint<64> = 0xFFFFFFFFFFFFFFFF
        let z = (a -% 1)
        log(info, "${z}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("auto z =") && !cpp.contains("int64_t z ="),
        "a parenthesised wrap destination must deduce unsigned; got:\n{cpp}"
    );
}

/// The shadowed-name guard must stop at a CAST: `wrap_operand_width`
/// takes a cast operand's width from the cast target, so a shadowed name
/// underneath contributes nothing and must not suppress the check.
#[test]
fn a_cast_operand_is_checked_even_under_a_shadowed_name() {
    let err = v1_emit_err(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = 1
        if a == 1
            let a : uint<64> = 2
        end if
        let b : uint<4> = (a as uint<8>) +% 1
        log(info, "${b}")
    end run
end test T"#,
    );
    assert!(
        err.contains("narrows"),
        "a cast operand's width is independent of the shadowed name; got: {err}"
    );
}

/// The signedness diagnostic must quote the VALUE's width and read
/// grammatically — v1 printed the destination's width for both, and
/// TB-IR rendered "a unsigned".
#[test]
fn signedness_diagnostics_quote_the_value_width() {
    let err = v1_emit_err(
        r#"test T
    let dut : Top
    run
        let a : uint<64> = 1
        let s : sint<8> = (a as uint<64>) +% 1
        log(info, "${s}")
    end run
end test T"#,
    );
    assert!(
        err.contains("an unsigned 64-bit value") && err.contains("declared signed 8 bits"),
        "must quote the value's width and the destination's separately; got: {err}"
    );
}

/// Guards the other backout in `5bad8ab`. A `let_signed_widths` table was
/// briefly added so the assignment form `s = a +% b` would get the same
/// signedness rejection as the initializer form. Being flat and unscoped,
/// it was poisoned by a signed local in ANY function or tseq — those are
/// emitted inside the per-test loop before the run body — so an unrelated
/// same-named local made v1 reject valid source. Any table-based revival
/// must fail here.
#[test]
fn a_signed_local_elsewhere_does_not_poison_an_unsigned_assignment() {
    let src = r#"function helper() -> uint<8>
    let s : sint<8> = 1
    return 3
end function
test T
    let dut : Top
    run
        let a : uint<8> = 200
        let s : uint<8> = 0
        s = a +% a
        log(info, "${s}")
    end run
end test T"#;
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    assert!(
        cpp_tb::emit(&merged).is_ok(),
        "a signed local in an unrelated function must not reject an unsigned assignment"
    );
    assert!(
        harc::ir::lower::lower_program(&merged).is_ok(),
        "TB-IR accepts it, so v1 must too"
    );
}

// ── harc#551: backend subset gaps, pinned so they stay declared ──────

/// The exact strings `tests/run_emit_parity.sh` greps to decide that a
/// TB-IR rejection is a real escape hatch rather than a divergence. Five
/// diagnostics name `--codegen v1` and only these two mean "use v1
/// instead"; the other three say v1 is broken on the construct. If a
/// rendering here changes wording, the gate silently reclassifies a
/// declared gap as a divergence (spurious red) — so pin the phrases, not
/// merely the flag.
#[test]
fn the_escape_hatch_phrases_the_parity_gate_greps_are_stable() {
    // Pin the diagnostic class directly. A concrete source gap is a poor
    // rendering fixture because successful burn-down intentionally makes
    // each such program stop producing this error.
    let err = harc::ir::lower::LowerError::Unsupported {
        construct: "a v1-supported construct".to_string(),
        detail: "".to_string(),
    }
    .to_string();
    assert!(
        err.contains("re-run with `--codegen v1`"),
        "run_emit_parity.sh greps this exact phrase; got: {err}"
    );
}

#[test]
fn wide_shifts_emit_in_both_backends() {
    let src = r#"test WideShiftHatch
    let dut : Top
    run
        let a : uint<128> = 1
        let b : uint<128> = a << 3
        assert b == 8 else fail("x")
    end run
end test WideShiftHatch"#;
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    // Both backends now route the operation through their wide-value model.
    let v1 = cpp_tb::emit(&merged).expect("v1 implements wide shifts");
    assert!(
        v1.contains("harc_rt::harc_shl_u128"),
        "v1 must emit the declared-width shift helper: {v1}"
    );
    let prog = harc::ir::lower::lower_program(&merged).expect("TB-IR lowers");
    let opts = harc::codegen::cpp_tb::EmitOpts::default();
    let cpp = harc::codegen::tbir::emit(&prog, &merged, &opts).expect("TB-IR emits");
    assert!(
        cpp.contains("harc_rt::harc_shl_u128"),
        "TB-IR must emit the declared-width shift helper: {cpp}"
    );
}

/// The other side of the same contract: a `NotImplemented` rendering must
/// NOT look like an escape hatch. All three of its clauses mention
/// `--codegen v1`, and two of them mean v1 accepts the construct and
/// produces bad output — the shape the gate most needs to flag. An
/// earlier version of the gate matched the bare flag and auto-exempted
/// exactly these.
#[test]
fn a_not_implemented_rendering_is_not_mistakable_for_an_escape_hatch() {
    let src = r#"test NotImpl
    let dut : Top
    run
        let big : uint<200> = 1
        let w = (big as uint<200>).zext<300>()
        assert w == 1 else fail("x")
    end run
end test NotImpl"#;
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    // v1 accepts this one, which is precisely why TB-IR refusing it is a
    // divergence worth reporting rather than a gap worth exempting.
    assert!(cpp_tb::emit(&merged).is_ok(), "v1 accepts a >128-bit cast");
    let err = harc::ir::lower::lower_program(&merged)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        err.contains("--codegen v1"),
        "the diagnostic should still say what v1 does; got: {err}"
    );
    assert!(
        !err.contains("re-run with `--codegen v1`"),
        "a NotImplemented rendering must not carry the escape-hatch phrase, or \
         the parity gate will exempt a case where v1's output is known bad; \
         got: {err}"
    );
}

/// `let s : sint<8> = a +% b` — harc#551 records this as an *internal
/// error* under TB-IR, which was true when that issue was filed. Both
/// backends now reject it with a real diagnostic. Pinned so it cannot
/// regress back into the compiler-bug channel, and so the two backends
/// keep agreeing.
#[test]
fn a_signed_wrap_destination_is_a_real_diagnostic_in_both_backends() {
    let src = r#"test SignedWrapDest
    let dut : Top
    run
        let a : uint<8> = 200
        let s : sint<8> = a +% 1
        log(info, "${s}")
    end run
end test SignedWrapDest"#;
    let v1 = v1_emit_err(src);
    assert!(
        !v1.contains("internal error") && v1.contains("signed"),
        "v1 must reject with a real diagnostic; got: {v1}"
    );
    let parsed = parse_source(src).expect("parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge");
    let tbir = harc::ir::lower::lower_program(&merged)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        !tbir.is_empty() && !tbir.contains("internal error"),
        "TB-IR must reject with a real diagnostic, not the compiler-bug \
         channel; got: {tbir}"
    );
}

// ── #552: wrapping operators inside `keep` constraints ───────────────

/// The solver variable is a 64-bit bitvector with the field's declared
/// width carried as a separate range assumption, so a `+%` does NOT wrap
/// on its own — the §2.4 mask has to be applied when the constraint is
/// lowered. Both backends previously dropped it, turning
/// `keep len +% 10 == 5` on a `uint<8>` field into `len + 10 == 5` with
/// `len < 256`, which is unsatisfiable though `len = 251` solves it.
#[test]
fn wrapping_operators_are_masked_inside_keep_constraints() {
    let cpp = v1_cpp(
        r#"transaction Txn
    len : uint<8> default 0
    keep len +% 10 == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    // C++ binds `&` looser than `==`, so the masked value needs its own
    // parens or `(a + b) & mask == 5` parses as `(a + b) & (mask == 5)`.
    assert!(
        cpp.contains(
            "((_z_len + _ctx.bv_val((uint64_t)10, 64)) \
             & harc_z3_bv_value(_ctx, (uint64_t)0x00000000000000ffULL, 64)) =="
        ),
        "the wrap must be masked to the field's 8 bits, parenthesised against `==`; got:\n{cpp}"
    );
}

/// The mask comes from the field the emitter actually solves for, which is
/// the *dotted* path — `hdr.len` is `_z_hdr_len`. Keying the width lookup
/// on the leaf name instead would find the unrelated top-level `len` here
/// and mask a `uint<16>` operand to 8 bits, and a too-narrow mask does not
/// fail loudly: the solver just returns one of the extra values it admits.
#[test]
fn nested_field_wrap_masks_to_the_nested_fields_own_width() {
    let cpp = v1_cpp(
        r#"struct Hdr
    len : uint<16>
end struct Hdr
transaction Txn
    len : uint<8> default 0
    hdr : Hdr
    keep hdr.len +% 10 == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains(
            "((_z_hdr_len + _ctx.bv_val((uint64_t)10, 64)) \
             & harc_z3_bv_value(_ctx, (uint64_t)0x000000000000ffffULL, 64)) =="
        ),
        "`hdr.len` is uint<16> and must mask to 16 bits, not to the top-level \
         `len`'s 8; got:\n{cpp}"
    );
}

/// `solver_width` is `max(field widths).max(64)` — at least 64, not always
/// 64. A transaction carrying any field wider than 64 bits solves every
/// constraint at that wider rank, so a narrower wrap there still needs its
/// mask, and that mask no longer fits a `uint64_t`.
#[test]
fn wrap_mask_tracks_a_solver_width_above_64_bits() {
    let cpp = v1_cpp(
        r#"transaction Txn
    m : bits<96> default 0
    big : bits<128> default 0
    keep m +% 3 == 1
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.big}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains(
            "& harc_z3_bv_value(_ctx, (((_harc_u128)0xffffffffULL << 0) \
             | ((_harc_u128)0xffffffffULL << 32) \
             | ((_harc_u128)0xffffffffULL << 64)), 128))"
        ),
        "a 96-bit wrap at a 128-bit solver width needs a 96-bit mask; got:\n{cpp}"
    );
}

/// A `foreach` clause is unrolled into `items[i]`, and the loop variable is
/// never a transaction field, so the element width has to come from the
/// list. Without it every `foreach` constraint containing a wrap would be
/// rejected for an unknown width.
#[test]
fn foreach_list_item_wrap_masks_to_the_element_width() {
    let cpp = v1_cpp(
        r#"transaction Txn
    items : list<uint<8>>
    keep items.len() >= 1
    keep items.len() <= 2
    keep for item in items
        item +% 250 == 4
    end for
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "x")
    end run
end test T"#,
    );
    assert!(
        cpp.contains(
            "((_z_items_0 + _ctx.bv_val((uint64_t)250, 64)) \
             & harc_z3_bv_value(_ctx, (uint64_t)0x00000000000000ffULL, 64)) =="
        ),
        "an unrolled list item must mask to the list's element width; got:\n{cpp}"
    );
}

/// The literal arm shares `parse_int_literal` with the statement path, so a
/// `0b` literal sizes the same in both positions. Hand-rolling decimal and
/// `0x` only made `let c = a +% 0b1010` emit while `keep len +% 0b1010 == 5`
/// hard-failed.
#[test]
fn non_decimal_literal_wrap_operands_size_in_constraints() {
    let cpp = v1_cpp(
        r#"transaction Txn
    len : uint<8> default 0
    keep len +% 0b100000000 == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    // 0b100000000 is 256 — nine bits, wider than the field, so the mask
    // proves the literal's own width was computed, not merely that it
    // failed to defeat the field's.
    assert!(
        cpp.contains("& harc_z3_bv_value(_ctx, (uint64_t)0x00000000000001ffULL, 64)) =="),
        "a 0b literal operand must size to its own width; got:\n{cpp}"
    );
}

/// An ARCH sized literal states its width outright, so it is the one
/// operand shape needing no inference at all — but `parse_int_literal`
/// refuses it, so a dedicated arm has to read the declared width.
#[test]
fn sized_literal_wrap_operands_use_their_declared_width() {
    let cpp = v1_cpp(
        r#"transaction Txn
    len : uint<8> default 0
    keep len +% 16'hAB == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("& harc_z3_bv_value(_ctx, (uint64_t)0x000000000000ffffULL, 64)) =="),
        "16'hAB declares 16 bits, so the wrap masks to 16 not to the field's 8; got:\n{cpp}"
    );
}

/// The emitter's `Ident` arm falls back from field to `const` to enum
/// variant to a blocking `let`. The width oracle has to resolve every one
/// of those, or a constraint the emitter would have emitted becomes a hard
/// build error instead.
#[test]
fn const_and_enum_variant_wrap_operands_carry_a_width() {
    let cpp = v1_cpp(
        r#"enum Color { RED, GREEN, BLUE }
const BUMP : uint<16> = 10
transaction Txn
    len : uint<8> default 0
    keep len +% BUMP == 5
    keep len +% BLUE == 4
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    // The const is declared 16 bits and initialised to 10, and the field
    // is 8 — so the mask distinguishes sizing the const by its declared
    // type (16, correct per §2.4) from sizing it by its value (4, which
    // would let the field's 8 bits win and solve to a value the source
    // constraint rejects).
    assert!(
        cpp.contains(
            "((_z_len + _ctx.bv_val((uint64_t)10, 64)) \
             & harc_z3_bv_value(_ctx, (uint64_t)0x000000000000ffffULL, 64)) =="
        ),
        "a `const` operand must size to its declared type; got:\n{cpp}"
    );
    assert!(
        cpp.contains(
            "((_z_len + _ctx.bv_val((uint64_t)2, 64)) \
             & harc_z3_bv_value(_ctx, (uint64_t)0x00000000000000ffULL, 64)) =="
        ),
        "an enum-variant operand must resolve a width; got:\n{cpp}"
    );
    // Both emitter constructors populate `const_widths`; dropping it from
    // the TB-IR one is invisible to any v1-only assertion.
    let tbir = tbir_constraint_snippets(
        r#"enum Color { RED, GREEN, BLUE }
const BUMP : uint<16> = 10
transaction Txn
    len : uint<8> default 0
    keep len +% BUMP == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    )
    .expect("tbir lowers");
    assert!(
        tbir.contains("harc_z3_bv_value(_ctx, (uint64_t)0x000000000000ffffULL, 64)"),
        "the default backend must size the const identically; got:\n{tbir}"
    );
}

#[test]
fn derived_signed_and_boolean_constants_reach_both_constraint_paths() {
    let src = r#"const BASE : uint<8> = 2
const LIMIT : uint<8> = BASE + 3
const NEG : sint<8> = -1
const ENABLE : bool = LIMIT == 5

transaction Txn
    u : uint<8>
    s : sint<8>
    keep u <= LIMIT
    keep s >= NEG
    keep ENABLE
end transaction Txn

test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.u}")
    end run
end test T"#;

    for (backend, cpp) in [
        ("v1", v1_cpp(src)),
        (
            "tbir",
            tbir_constraint_snippets(src).expect("typed constants lower through TB-IR"),
        ),
    ] {
        assert!(
            cpp.contains("_ctx.bv_val((uint64_t)5, 64)"),
            "{backend}: derived uint constant must fold to 5:\n{cpp}"
        );
        assert!(
            cpp.contains("_ctx.bv_val((uint64_t)-1, 64)"),
            "{backend}: unary-negative signed constant must retain -1:\n{cpp}"
        );
        assert!(
            cpp.contains("_s.add(_ctx.bool_val(true));"),
            "{backend}: typed bool constant must emit as a Z3 Bool:\n{cpp}"
        );
    }
}

#[test]
fn equal_randomize_offsets_in_different_files_keep_distinct_problem_state() {
    let defs = harc::parser::parse_source_named(
        "defs.harc",
        r#"transaction Txn
    value : uint<8>
end transaction Txn
"#,
    )
    .expect("definitions parse");
    let alpha = harc::parser::parse_source_named(
        "alpha.harc",
        r#"test Alpha
    let dut : Top
    run
        let t : Txn
        randomize(t) with
            t.value != 0
        end randomize
    end run
end test Alpha
"#,
    )
    .expect("alpha parses");
    let bravo = harc::parser::parse_source_named(
        "bravo.harc",
        r#"test Bravo
    let dut : Top
    run
        let t : Txn
        randomize(t) with
            t.value != 0
        end randomize
    end run
end test Bravo
"#,
    )
    .expect("bravo parses");
    let merged = merge::merge_for_sim(vec![defs, alpha, bravo], None).expect("merge");
    let table = harc::solver::problem_table::build_typed_solver_problem_table(&merged);
    let sites: Vec<_> = table
        .entries
        .iter()
        .filter_map(|entry| {
            let harc::solver::problem_table::TypedSolverProblemSource::RandomizeSite {
                source_id,
                span,
                ..
            } = entry.source
            else {
                return None;
            };
            let problem_id = match &entry.build {
                harc::solver::problem_table::TypedSolverProblemBuild::Z3 { typed, .. } => {
                    typed.problem_id.0
                }
                other => panic!("randomize site must be Z3-ready: {other:?}"),
            };
            Some((source_id, span, problem_id))
        })
        .collect();
    assert_eq!(sites.len(), 2);
    assert_eq!(
        sites[0].1, sites[1].1,
        "fixture must use equal byte offsets"
    );
    assert_ne!(
        sites[0].0, sites[1].0,
        "source identity must remain distinct"
    );
    assert_ne!(sites[0].2, sites[1].2, "problem ids must remain distinct");

    let cpp = cpp_tb::emit(&merged).expect("v1 emits both test cases");
    let prog = harc::ir::lower::lower_program(&merged).expect("TB-IR lowers both test cases");
    assert_eq!(prog.constraint_sites.len(), 2);
    assert_ne!(
        prog.constraint_sites[0].source_id,
        prog.constraint_sites[1].source_id
    );
    let tbir_snippets = cpp_tb::emit_randomize_snippets(
        &merged,
        &cpp_tb::EmitOpts::default(),
        &prog.constraint_sites,
        0,
    )
    .expect("TB-IR randomize snippets emit")
    .join("\n");
    for (_, _, problem_id) in sites {
        assert!(
            cpp.contains(&format!("_solver_site_{problem_id}")),
            "v1 randomize problem {problem_id} must own distinct generated state:\n{cpp}"
        );
        assert!(
            tbir_snippets.contains(&format!("_solver_site_{problem_id}")),
            "TB-IR randomize problem {problem_id} must retain its own generated state:\n{tbir_snippets}"
        );
    }
}

#[test]
fn component_randomize_sites_at_equal_offsets_keep_distinct_runtime_cells() {
    let alpha = harc::parser::parse_source_named(
        "alpha.harc",
        r#"agent Alpha
    hookable draw()
        let ctx : TxA
        randomize(ctx)
    end draw
end agent Alpha

transaction TxA
    value : uint<8> with [unique within test]
end transaction TxA
"#,
    )
    .expect("alpha parses");
    let bravo = harc::parser::parse_source_named(
        "bravo.harc",
        r#"agent Bravo
    hookable draw()
        let ctx : TxB
        randomize(ctx)
    end draw
end agent Bravo

transaction TxB
    value : uint<130> with [unique within test]
end transaction TxB
"#,
    )
    .expect("bravo parses");
    let test = harc::parser::parse_source_named(
        "test.harc",
        r#"test EqualOffsetComponents
    let dut : Top
    let alpha : Alpha
    let bravo : Bravo
    run
        alpha.draw()
        bravo.draw()
    end run
end test EqualOffsetComponents
"#,
    )
    .expect("test parses");
    let merged = merge::merge_for_sim(vec![alpha, bravo, test], None).expect("merge");
    let program = harc::ir::lower::lower_program(&merged).expect("lower");
    harc::ir::verify::verify_program(&program).expect("verify");
    assert_eq!(program.constraint_sites.len(), 2);
    assert_eq!(
        program.constraint_sites[0].target.span, program.constraint_sites[1].target.span,
        "fixture must keep the component randomize targets at equal offsets"
    );
    assert_ne!(
        program.constraint_sites[0].source_id,
        program.constraint_sites[1].source_id
    );
    assert!(program
        .constraint_sites
        .iter()
        .all(|site| site.problem_id.is_none()));

    let opts = cpp_tb::EmitOpts::default();
    let v1 = cpp_tb::emit(&merged).expect("v1 emits distinct component sites");
    let v1_tags = component_solver_site_tags(&v1);
    assert_eq!(v1_tags.len(), 2, "{v1}");
    let tbir = harc::codegen::tbir::emit(&program, &merged, &opts)
        .expect("self-contained TB-IR emits distinct component sites");
    let tbir_tags = component_solver_site_tags(&tbir);
    assert_eq!(tbir_tags.len(), 2, "{tbir}");
    assert_eq!(v1_tags, tbir_tags);
}

#[test]
fn component_randomize_site_tags_ignore_source_id_allocation_order() {
    const ALPHA: &str = r#"agent Alpha
    hookable draw()
        let ctx : TxA
        randomize(ctx)
    end draw
end agent Alpha

transaction TxA
    value : uint<8> with [unique within test]
end transaction TxA
"#;
    const BRAVO: &str = r#"agent Bravo
    hookable draw()
        let ctx : TxB
        randomize(ctx)
    end draw
end agent Bravo

transaction TxB
    value : uint<130> with [unique within test]
end transaction TxB
"#;
    const TEST: &str = r#"test EqualOffsetComponents
    let dut : Top
    let alpha : Alpha
    let bravo : Bravo
    run
        alpha.draw()
        bravo.draw()
    end run
end test EqualOffsetComponents
"#;

    let render = |reverse_parse_order: bool| {
        let (alpha, bravo) = if reverse_parse_order {
            let bravo = harc::parser::parse_source_named("bravo.harc", BRAVO).expect("bravo");
            let alpha = harc::parser::parse_source_named("alpha.harc", ALPHA).expect("alpha");
            (alpha, bravo)
        } else {
            let alpha = harc::parser::parse_source_named("alpha.harc", ALPHA).expect("alpha");
            let bravo = harc::parser::parse_source_named("bravo.harc", BRAVO).expect("bravo");
            (alpha, bravo)
        };
        let test = harc::parser::parse_source_named("test.harc", TEST).expect("test");
        let merged = merge::merge_for_sim(vec![alpha, bravo, test], None).expect("merge");
        cpp_tb::emit(&merged).expect("emit")
    };

    let forward = render(false);
    let reverse = render(true);
    assert_eq!(component_solver_site_tags(&forward).len(), 2, "{forward}");
    assert_eq!(
        component_solver_site_tags(&forward),
        component_solver_site_tags(&reverse)
    );
    assert_eq!(forward, reverse);
}

#[test]
fn component_randomize_site_tags_disambiguate_duplicate_source_names() {
    const ALPHA: &str = r#"agent Alpha
    hookable draw()
        let ctx : TxA
        randomize(ctx)
    end draw
end agent Alpha

transaction TxA
    value : uint<8> with [unique within test]
end transaction TxA
"#;
    const BRAVO: &str = r#"agent Bravo
    hookable draw()
        let ctx : TxB
        randomize(ctx)
    end draw
end agent Bravo

transaction TxB
    value : uint<130> with [unique within test]
end transaction TxB
"#;
    const TEST: &str = r#"test EqualOffsetComponents
    let dut : Top
    let alpha : Alpha
    let bravo : Bravo
    run
        alpha.draw()
        bravo.draw()
    end run
end test EqualOffsetComponents
"#;

    let render = |reverse_parse_order: bool| {
        let (alpha, bravo) = if reverse_parse_order {
            let bravo = parse_source(BRAVO).expect("bravo");
            let alpha = parse_source(ALPHA).expect("alpha");
            (alpha, bravo)
        } else {
            let alpha = parse_source(ALPHA).expect("alpha");
            let bravo = parse_source(BRAVO).expect("bravo");
            (alpha, bravo)
        };
        assert_eq!(alpha.sources[0].name.as_ref(), "<input>");
        assert_eq!(bravo.sources[0].name.as_ref(), "<input>");
        let test = parse_source(TEST).expect("test");
        let merged = merge::merge_for_sim(vec![alpha, bravo, test], None).expect("merge");
        cpp_tb::emit(&merged).expect("emit")
    };

    let forward = render(false);
    let reverse = render(true);
    let forward_tags = component_solver_site_tags(&forward);
    assert_eq!(forward_tags.len(), 2, "{forward}");
    assert_eq!(forward_tags, component_solver_site_tags(&reverse));
    assert_eq!(forward, reverse);
}

/// An enum is a value type, not a DUT module. `local_value_c_type` used
/// to short-circuit only on records, so `let c : Color = BLUE` fell
/// through to the "Named type means Verilator handle" rule and declared
/// `VColor* c` — a handle type for a module that does not exist, so the
/// generated C++ failed to compile (`unknown type name 'VColor'`).
///
/// The declaration alone is not the whole fix: the initializer is a bare
/// variant name, which `emit_expr` writes through verbatim, so the
/// variant needs a file-scope definition too. Both halves are asserted
/// here because either one alone still yields C++ that does not build.
#[test]
fn enum_typed_local_lowers_to_an_integer_with_a_defined_variant() {
    let cpp = v1_cpp(
        r#"enum Color { RED, GREEN, BLUE }
test T
    let dut : Top
    run
        let c : Color = BLUE
        log(info, "${c}")
    end run
end test T"#,
    );
    assert!(
        !cpp.contains("VColor"),
        "an enum-typed local must not become a Verilator DUT handle; got:\n{cpp}"
    );
    let red = v1_enum_symbol(&cpp, "RED");
    let green = v1_enum_symbol(&cpp, "GREEN");
    let blue = v1_enum_symbol(&cpp, "BLUE");
    assert!(
        cpp.contains(&format!("int64_t c = {blue};")),
        "an enum-typed local takes the same integer type an enum-typed \
         record field does; got:\n{cpp}"
    );
    assert!(
        cpp.contains(&format!("static constexpr int64_t {red} = 0;"))
            && cpp.contains(&format!("static constexpr int64_t {green} = 1;"))
            && cpp.contains(&format!("static constexpr int64_t {blue} = 2;")),
        "enum variants need file-scope definitions to be usable outside a \
         `keep`; got:\n{cpp}"
    );
}

/// A `const` of the same name outranks a variant. That resolution is
/// v1's own — the `Ident` arm consults `self.consts` before
/// `self.enum_variants` — and it is also what TB-IR does, folding both
/// into one table with the `const` winning. Emitting the variant anyway
/// would give one C++ name two definitions.
#[test]
fn a_const_outranks_a_same_named_enum_variant() {
    let cpp = v1_cpp(
        r#"const GREEN : uint<8> = 42
enum Color { RED, GREEN, BLUE }
test T
    let dut : Top
    run
        log(info, "${GREEN} ${RED}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("static constexpr uint64_t harc_user_const_GREEN_")
            && cpp.contains(" = 42;")
            && !cpp.contains("static constexpr int64_t harc_user_enum_GREEN_"),
        "a `const` outranks a same-named variant, and only one definition \
         may be emitted; got:\n{cpp}"
    );
    let red = v1_enum_symbol(&cpp, "RED");
    assert!(
        cpp.contains(&format!("static constexpr int64_t {red} = 0;")),
        "the other variants of that enum are unaffected; got:\n{cpp}"
    );
}

/// A variant name that another top-level type already owns cannot become
/// a file-scope `constexpr`: a variable declaration hides a class name in
/// the same C++ scope, so defining `Pkt` would turn every later `Pkt p;`
/// into "must use 'struct' tag to refer to type 'Pkt'" — breaking a
/// program that compiles today and never mentions the variant.
///
/// So the definition is suppressed. DECLARING the clash stays legal,
/// because constraint position resolves variants through `enum_variants`
/// and is unaffected; only USING the name as a value is diagnosed.
#[test]
fn a_variant_colliding_with_a_type_name_is_suppressed_and_diagnosed_at_use() {
    let declared_only = r#"transaction Pkt
    addr : uint<8>
end transaction Pkt
enum Kind { Pkt, OTHER }
test T
    let dut : Top
    run
        let p : Pkt
        p.addr = 3
        log(info, "${p.addr}")
    end run
end test T"#;
    let cpp = v1_cpp(declared_only);
    assert!(
        !cpp.contains("constexpr int64_t Pkt ="),
        "a variant colliding with a record name must not be defined; got:\n{cpp}"
    );
    let other = v1_enum_symbol(&cpp, "OTHER");
    assert!(
        cpp.contains(&format!("static constexpr int64_t {other} = 1;")),
        "the enum's other variants are still defined; got:\n{cpp}"
    );

    let used_as_value = r#"transaction Pkt
    addr : uint<8>
end transaction Pkt
enum Kind { Pkt, OTHER }
test T
    let dut : Top
    run
        let k : Kind = Pkt
        log(info, "${k}")
    end run
end test T"#;
    let err = v1_cpp_err(used_as_value);
    assert!(
        err.contains("`Kind.Pkt`") && err.contains("a type of the same name"),
        "using the suppressed variant as a value must be diagnosed; got:\n{err}"
    );
}

/// One variant name declared by two enums has no correct index, so it
/// gets no definition either. `enum_variants` resolves this first-wins
/// because v0 assumes variant names are globally unique — tolerable for a
/// solver token, but as a value it would silently compile
/// `let w : WrResp = OKAY` to 0 when `WrResp.OKAY` is 1. A diagnostic
/// beats a wrong number.
#[test]
fn a_variant_declared_by_two_enums_is_suppressed_and_diagnosed_at_use() {
    let src = r#"enum RdResp { OKAY, SLVERR }
enum WrResp { SLVERR, OKAY }
test T
    let dut : Top
    run
        let w : WrResp = OKAY
        log(info, "${w}")
    end run
end test T"#;
    let err = v1_cpp_err(src);
    assert!(
        err.contains("declared by 2 different enums"),
        "an ambiguous variant used as a value must be diagnosed; got:\n{err}"
    );
    // Not "qualify the use" — HARC has no `Enum.VARIANT` form, and
    // suggesting one sends the reader at a parse error.
    assert!(
        !err.contains("qualify"),
        "the message must not suggest syntax the language lacks; got:\n{err}"
    );
}

/// A `const` initialised from a variant does NOT compile under v1, and
/// that is deliberate. spec.md's `const` paragraph admits references to
/// *earlier* const/enum-variant names only ("forward and cyclic
/// references are compile errors") and names "enum-variant references in
/// initializers" among the corners TB-IR alone defines — TB-IR rejects a
/// forward reference by declaration order.
///
/// Emitting the variant block ABOVE the consts would make v1 accept both
/// orders, so v1 would take a program the spec calls an error while the
/// default backend refuses it. The variant block therefore stays BELOW,
/// and this test pins the emission order that keeps the two agreeing.
#[test]
fn enum_variants_emit_after_consts_so_v1_does_not_outrun_the_const_subset() {
    let cpp = v1_cpp(
        r#"enum Color { RED, GREEN, BLUE }
const LIMIT : uint<8> = BLUE
test T
    let dut : Top
    run
        log(info, "${LIMIT}")
    end run
end test T"#,
    );
    // The const names the variant, so it is the case the rule is about:
    // the initializer emits above `BLUE` and v1 fails to compile, exactly
    // as the spec says a forward reference must.
    let konst = cpp
        .find("static constexpr uint64_t harc_user_const_LIMIT_")
        .unwrap_or_else(|| panic!("no LIMIT definition; got:\n{cpp}"));
    let variant = cpp
        .find(&format!(
            "static constexpr int64_t {} = 2;",
            v1_enum_symbol(&cpp, "BLUE")
        ))
        .unwrap_or_else(|| panic!("no BLUE definition; got:\n{cpp}"));
    assert!(
        konst < variant,
        "consts must be emitted before enum variants, so a `const` cannot \
         name a variant; got:\n{cpp}"
    );
}

/// One enum repeating a name is a malformed enum, not an ambiguity
/// between two of them, so `collect_unsafe_enum_variants` counts it once
/// and does NOT suppress it. That makes the emission loop the only thing
/// standing between `enum E { A, B, A }` and two `constexpr A` definitions
/// — a C++ redefinition error for a program that merely DECLARES the enum
/// and never uses it.
#[test]
fn a_name_repeated_inside_one_enum_is_defined_once() {
    let cpp = v1_cpp(
        r#"enum E { A, B, A }
test T
    let dut : Top
    run
        log(info, "hi")
    end run
end test T"#,
    );
    let a = v1_enum_symbol(&cpp, "A");
    assert_eq!(
        cpp.matches(&format!("static constexpr int64_t {a} = "))
            .count(),
        1,
        "a name repeated inside one enum must be defined once; got:\n{cpp}"
    );
    assert!(
        cpp.contains(&format!("static constexpr int64_t {a} = 0;")),
        "the first occurrence fixes the index, matching `enum_variants`; got:\n{cpp}"
    );
}

/// Names the backend writes into every generated file are not declared by
/// anything in the user's program, so they cannot be found by inspecting
/// its items. `main` is the one an ordinary program hits — `enum Phase {
/// setup, main, teardown }` is unremarkable naming, and a file-scope
/// `constexpr main` is rejected outright by C++.
#[test]
fn a_variant_named_like_a_symbol_the_backend_always_emits_is_suppressed() {
    let cpp = v1_cpp(
        r#"enum Kind { main, OTHER }
test T
    let dut : Top
    run
        log(info, "hi")
    end run
end test T"#,
    );
    assert!(
        !cpp.contains("constexpr int64_t main"),
        "a variant named `main` must not be defined at file scope; got:\n{cpp}"
    );
    let other = v1_enum_symbol(&cpp, "OTHER");
    assert!(
        cpp.contains(&format!("static constexpr int64_t {other} = 1;")),
        "the enum's other variants are unaffected; got:\n{cpp}"
    );
}

/// A local or a parameter named like a suppressed variant is a perfectly
/// good program — C++ scoping hides the file-scope name, which is the same
/// shadowing the whole `constexpr` approach relies on — and it compiled
/// before this feature existed. Diagnosing it would reject working code,
/// so `declared_value_names` gates the diagnostic.
#[test]
fn a_local_or_param_named_like_a_suppressed_variant_is_not_diagnosed() {
    let local = r#"enum RdResp { OKAY, SLVERR }
enum WrResp { SLVERR, OKAY }
test T
    let dut : Top
    run
        let OKAY : uint<8> = 7
        log(info, "${OKAY}")
    end run
end test T"#;
    let cpp = v1_cpp(local);
    assert!(
        cpp.contains("uint64_t OKAY = 7;"),
        "the local must still be emitted; got:\n{cpp}"
    );

    let param = r#"enum RdResp { OKAY, SLVERR }
enum WrResp { SLVERR, OKAY }
function twice(OKAY: uint<8>) -> uint<8>
    return OKAY + OKAY
end function twice
test T
    let dut : Top
    run
        log(info, "${twice(3)}")
    end run
end test T"#;
    let cpp = v1_cpp(param);
    assert!(
        cpp.contains("OKAY + OKAY"),
        "the param must still be referenced by name in the body; got:\n{cpp}"
    );
}

/// The binder forms that build their C++ name inline rather than through
/// `emit_let` or `cpp_param_names`: a `for` variable and an `on`-handler
/// argument (both the test-scope and component-body spellings). Each hides
/// a file-scope name exactly as a `let` does, so none may draw the
/// suppressed-variant diagnostic. All three compiled on the backend before
/// this feature existed, and all three run green on the default backend.
#[test]
fn for_and_on_handler_binders_named_like_a_suppressed_variant_are_not_diagnosed() {
    let dup = "enum RdResp { OKAY, SLVERR }\nenum WrResp { SLVERR, OKAY }\n";

    let for_var = format!(
        r#"{dup}test T
    let dut : Top
    run
        for OKAY in 0 .. 3
            log(info, "i=${{OKAY}}")
        end for
    end run
end test T"#
    );
    let cpp = v1_cpp(&for_var);
    assert!(
        cpp.contains("for (int64_t OKAY = "),
        "the loop variable must still be emitted by name; got:\n{cpp}"
    );

    let on_test_scope = format!(
        r#"{dup}test T
    let dut : Top
    run
        let ev : event<uint<8>>
        on ev(OKAY)
            log(info, "got ${{OKAY}}")
        end on
        emit ev(5)
        wait 1 cycle
    end run
end test T"#
    );
    let cpp = v1_cpp(&on_test_scope);
    assert!(
        cpp.contains("ev.push_back([&](uint64_t OKAY) {"),
        "the handler argument must still be emitted by name; got:\n{cpp}"
    );

    let on_component = format!(
        r#"{dup}agent Tagger
    in_ev : event<uint<8>>
    last  : uint<8> default 0
    on in_ev(OKAY)
        last = OKAY
    end on
end agent Tagger
test T
    let dut : Top
    run
        let tg : Tagger
        log(info, "ok")
    end run
end test T"#
    );
    let cpp = v1_cpp(&on_component);
    assert!(
        cpp.contains("uint64_t OKAY) {"),
        "the component handler argument must still be emitted by name; got:\n{cpp}"
    );
}

/// The argument for diagnosing at the use site rather than the enum
/// declaration: constraint position resolves variants through
/// `enum_variants`, not through the emitted `constexpr`, so a program that
/// only ever uses an ambiguous name inside a `keep` keeps working under
/// the documented first-wins rule. Erroring at the declaration would have
/// rejected it.
#[test]
fn a_duplicate_variant_used_only_in_a_keep_is_still_accepted() {
    let cpp = v1_cpp(
        r#"enum RdResp { OKAY, SLVERR }
enum WrResp { SLVERR, OKAY }
transaction Req
    r : RdResp
    keep r == OKAY
end transaction Req
test T
    let dut : Top
    run
        let t : Req
        randomize(t)
        log(info, "${t.r}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("_z_r == _ctx.bv_val((uint64_t)0, 64)"),
        "constraint position must still resolve the variant index; got:\n{cpp}"
    );
}

/// A sized literal masks at its **declared** width, the same rule a
/// `const` follows, so the two agree about a token spellable either way.
/// The digits are not consulted, and `8'h0F` is where that is
/// observable: the value needs 4 bits, the token declares 8, and the
/// mask must follow the 8.
///
/// This used to be written with `4'hFF` — declared 4, digits needing 8 —
/// to pin which of the two the mask read. That token is now a parse
/// error (harc#565), which is the better answer: the mask no longer has
/// to choose between two readings of the same literal, because a literal
/// can no longer have two.
#[test]
fn a_sized_literal_masks_at_its_declared_width() {
    let src = r#"transaction Txn
    len : uint<4> default 0
    keep len +% 8'h0F == 15
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#;
    let cpp = v1_cpp(src);
    assert!(
        cpp.contains(
            "((_z_len + _ctx.bv_val((uint64_t)0x0F, 64)) \
             & harc_z3_bv_value(_ctx, (uint64_t)0x00000000000000ffULL, 64)) =="
        ),
        "8'h0F must mask at its declared 8 bits, not the 4 its value needs; got:\n{cpp}"
    );
    let tbir = tbir_constraint_snippets(src).expect("tbir lowers");
    assert!(
        tbir.contains("harc_z3_bv_value(_ctx, (uint64_t)0x00000000000000ffULL, 64)"),
        "the default backend must mask identically; got:\n{tbir}"
    );
}

/// A sized literal whose value overflows `u64` still has a perfectly
/// good declared width, and at a solver width that wide it needs no mask
/// at all. Parsing the digits to size it made this a build error.
#[test]
fn a_sized_literal_wider_than_u64_still_resolves() {
    let cpp = v1_cpp(
        r#"transaction Txn
    m : bits<128> default 0
    keep m +% 128'hFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF == 1
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.m}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("_s.add(_z_m + harc_z3_bv_value"),
        "a 128-bit wrap at a 128-bit solver width needs no mask and must not be \
         rejected; got:\n{cpp}"
    );
}

/// Under `blocking randomize` the emitter inlines an arbitrary in-scope
/// C++ expression, and the width oracle cannot resolve those on the
/// default backend: `emit_randomize_snippets` builds its emitter per-site
/// with no statement state, so `let_types`/`let_widths`/`probe_widths`
/// are empty by construction. Resolving them on v1 only would make the
/// backends disagree about the same source — the exact divergence #552's
/// fix promised not to introduce — so both reject. Tracked in harc#566.
#[test]
fn blocking_non_target_wrap_operands_are_rejected_by_both_backends() {
    let src = r#"struct Cfg
    max : uint<8>
end struct Cfg
transaction Txn
    len : uint<16> default 0
end transaction Txn
test T
    let dut : Top
    run
        let c : Cfg
        let t : Txn
        blocking randomize(t) with
            t.len +% c.max == 5
        end randomize
        log(info, "${t.len}")
    end run
end test T"#;
    let v1 = v1_emit_err(src);
    assert!(
        v1.contains("statically known bit-width"),
        "v1 must reject a blocking non-target wrap operand; got: {v1}"
    );
    let tbir = tbir_constraint_snippets(src).expect_err("tbir must reject too");
    assert!(
        tbir.contains("statically known bit-width"),
        "the default backend must reject it the same way, not diverge; got: {tbir}"
    );

    // A DUT probe is the shape the statement path's `wrap_operand_width`
    // *can* resolve, so it is the one that would silently come back as a
    // v1-only acceptance if the fallback were reinstated.
    let probe_src = r#"transaction Txn
    len : uint<16> default 0
end transaction Txn
test T
    let dut : Top
        probe cnt : uint<8> at count
    end let dut
    run
        let t : Txn
        blocking randomize(t) with
            t.len +% dut.cnt == 5
        end randomize
        log(info, "${t.len}")
    end run
end test T"#;
    let v1_probe = v1_emit_err(probe_src);
    assert!(
        v1_probe.contains("statically known bit-width"),
        "v1 must reject a blocking probe operand; got: {v1_probe}"
    );
    let tbir_probe = tbir_constraint_snippets(probe_src).expect_err("tbir must reject too");
    assert!(
        tbir_probe.contains("statically known bit-width"),
        "the default backend must reject the probe the same way; got: {tbir_probe}"
    );
}

/// `cast_relabel_width` returns `None` both for "not a width type" and
/// for "out of range", so a caller that falls back on `None` cannot tell
/// them apart — a `uint<200>` const would silently take its initializer's
/// 4-bit value width. It must be loud instead.
#[test]
fn an_out_of_range_const_width_is_rejected_not_silently_value_sized() {
    let err = v1_emit_err(
        r#"const K : uint<200> = 10
transaction Txn
    len : uint<8> default 0
    keep len +% K == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
    end run
end test T"#,
    );
    assert!(
        err.contains("200-bit result"),
        "a uint<200> const must report its declared width, not fall back to the \
         initializer's value width; got: {err}"
    );
    // On the TB-IR path too. `const_widths` is built in both emitter
    // constructors, and the cross-backend assertion elsewhere uses a
    // `uint<16>` const, which `cast_relabel_width` and
    // `declared_type_bit_width` size identically — so only an
    // out-of-range width can see this change on the default backend.
    let tbir = tbir_constraint_snippets(
        r#"const K : uint<200> = 10
transaction Txn
    len : uint<8> default 0
    keep len +% K == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
    end run
end test T"#,
    )
    .expect_err("tbir must reject it too");
    assert!(
        tbir.contains("200-bit result"),
        "the default backend must reject it identically, not diverge; got: {tbir}"
    );
}

/// `declared_type_bit_width`'s non-parameterised arms. A `bool` const is
/// one bit and an `int` const is 32, and each must agree with the value
/// the emitter materialises for it.
#[test]
fn const_widths_follow_the_declared_type_for_unparameterised_builtins() {
    let cpp = v1_cpp(
        r#"const B : bool = 1
const I : int = 5
transaction Txn
    len : uint<1> default 0
    wide : uint<40> default 0
    keep len +% B == 0
    keep wide +% I == 7
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    // `bool` is 1 bit, and the field is 1 bit, so the wrap masks to 1.
    assert!(
        cpp.contains("& harc_z3_bv_value(_ctx, (uint64_t)0x0000000000000001ULL, 64)) =="),
        "a `bool` const must contribute 1 bit; got:\n{cpp}"
    );
    // `int` is 32 bits and the field is 40, so 40 wins — but only if the
    // const resolved at all.
    assert!(
        cpp.contains("& harc_z3_bv_value(_ctx, (uint64_t)0x000000ffffffffffULL, 64)) =="),
        "an `int` const must resolve so the 40-bit field's mask applies; got:\n{cpp}"
    );

    // A width-less `uint` is 64 bits, which at a 64-bit solver width is
    // exactly the bitvector — so the correct emission has NO mask at all.
    // Sizing it any narrower would emit one.
    let bare = v1_cpp(
        r#"const U : uint = 10
transaction Txn
    len : uint<8> default 0
    keep len +% U == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    assert!(
        bare.contains("_s.add(_z_len + _ctx.bv_val((uint64_t)10, 64) =="),
        "a width-less `uint` const is 64 bits, so the wrap needs no mask; got:\n{bare}"
    );
}

/// `0'h0` used to lex and resolve, sized as one bit by a `.max(1)` clamp
/// in `literal_operand_bit_width` — without which it would have reached
/// `solver_unsigned_mask_expr(0)` and produced a bogus 32-bits-of-ones
/// mask. harc#565 rejects the token at parse time instead, so both the
/// clamp and the shape it protected are gone; the rejection is pinned by
/// `a_zero_width_sized_literal_is_rejected`.
///
/// What remains worth checking is the coupling that clamp hid: no width
/// reaching the mask may be zero.
#[test]
fn a_wrap_operand_width_is_never_zero() {
    let cpp = v1_cpp(
        r#"transaction Txn
    len : uint<8> default 0
    keep len +% 1'b0 == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("& harc_z3_bv_value(_ctx, (uint64_t)0x00000000000000ffULL, 64)) =="),
        "the field's 8 bits must win over the literal's 1; got:\n{cpp}"
    );

    // Both operands one bit wide: the narrowest mask the emitter can be
    // asked for, and the shape that would expose a zero.
    let both = v1_cpp(
        r#"transaction Txn
    len : uint<8> default 0
    keep len == 1'b1 +% 1'b1
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    assert!(
        both.contains("& harc_z3_bv_value(_ctx, (uint64_t)0x0000000000000001ULL, 64))"),
        "a wrap of two one-bit literals must mask at one bit; got:\n{both}"
    );
}

/// A `let` under `blocking randomize` is the third shape the emitter
/// inlines from surrounding scope, alongside a record field and a DUT
/// probe. It has to be rejected by BOTH backends for the same reason
/// (harc#566): the width is only reachable from per-test emitter state
/// that the TB-IR randomize emitter never populates, so resolving it on
/// v1 alone would make the backends disagree.
#[test]
fn a_blocking_let_wrap_operand_is_rejected_by_both_backends() {
    let src = r#"transaction Txn
    len : uint<8> default 0
end transaction Txn
test T
    let dut : Top
    run
        let n : uint<8> = 10
        let t : Txn
        blocking randomize(t) with
            t.len +% n == 5
        end randomize
        log(info, "${t.len}")
    end run
end test T"#;
    let v1 = v1_emit_err(src);
    assert!(
        v1.contains("statically known bit-width"),
        "v1 must reject a blocking `let` wrap operand; got: {v1}"
    );
    let tbir = tbir_constraint_snippets(src).expect_err("tbir must reject too");
    assert!(
        tbir.contains("statically known bit-width"),
        "the default backend must reject it the same way, not diverge; got: {tbir}"
    );
}

/// A sized literal can declare more bits than the solver bitvector has,
/// which is the second way (besides an unbounded bit-slice) to reach an
/// unrepresentable residue.
#[test]
fn a_sized_literal_wider_than_the_solver_bitvector_is_rejected() {
    let err = v1_emit_err(
        r#"transaction Txn
    len : uint<8> default 0
    keep len +% 128'h1 == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
    end run
end test T"#,
    );
    assert!(
        err.contains("wider than the 64-bit solver bitvector"),
        "expected the unrepresentable-residue diagnostic; got: {err}"
    );
}

/// Parse a statement body carrying `lit` and return the diagnostic,
/// asserting the parse failed. These rejections land at parse level, so
/// `v1_emit_err` — which `expect("parses")` — cannot express them.
fn parse_err_with_literal(lit: &str) -> String {
    match parse_source(&literal_src(lit)) {
        Ok(_) => panic!("expected `{lit}` to be rejected, but it parsed"),
        Err(e) => e.to_string(),
    }
}

fn literal_parses(lit: &str) -> bool {
    parse_source(&literal_src(lit)).is_ok()
}

fn literal_src(lit: &str) -> String {
    format!(
        r#"test T
    let dut : Top
    run
        let a : uint<8> = {lit}
        log(info, "a=${{a}}")
    end run
end test T"#
    )
}

/// harc#565. `4'hFF` used to lower to `0xFF` — 255 out of a token that
/// declares four bits. The declared width is what the §2.4 wrap mask
/// reads, so a literal whose digits outrun it made that mask a guess
/// between two readings, each wrong for a different program.
#[test]
fn a_sized_literal_wider_than_its_declared_width_is_rejected() {
    let err = parse_err_with_literal("4'hFF");
    assert!(
        err.contains("needs 8 bits") && err.contains("declares 4"),
        "the diagnostic must name both widths so the fix is obvious; got: {err}"
    );
}

/// The same defect in decimal, the spelling easiest to write by accident.
#[test]
fn an_overwide_sized_decimal_literal_is_rejected() {
    let err = parse_err_with_literal("8'd300");
    assert!(
        err.contains("needs 9 bits") && err.contains("declares 8"),
        "got: {err}"
    );
}

/// The boundary, in both directions. Off by one either way would still
/// pass a test that only checked a value far from the edge.
#[test]
fn the_declared_width_boundary_is_exact() {
    assert!(
        literal_parses("16'd65535"),
        "65535 fits in 16 bits and must be accepted"
    );
    assert!(
        literal_parses("8'hFF") && literal_parses("1'b1") && literal_parses("4'hF"),
        "a value that exactly fills its declared width must be accepted"
    );
    let err = parse_err_with_literal("16'd65536");
    assert!(
        err.contains("needs 17 bits"),
        "65536 needs 17 bits; got: {err}"
    );

    // A hex literal is sized as 4 bits per digit EXCEPT the leading one,
    // which contributes only its significant bits. `1F` is 5, not 8 —
    // rounding the first digit up to 4 would reject this correct program
    // and every test above would still pass, since their leading digit
    // is `F` or the value is decimal.
    assert!(literal_parses("5'h1F"), "0x1F is 5 bits, not 8");
    let err = parse_err_with_literal("4'h1F");
    assert!(err.contains("needs 5 bits"), "got: {err}");
}

/// Leading zeros are padding, not significant bits — rejecting `8'h0F`
/// would break correct programs, which is the failure mode that makes a
/// new static error worse than the bug it replaces.
#[test]
fn leading_zeros_do_not_count_toward_the_width() {
    assert!(literal_parses("8'h0F"), "`8'h0F` is 15 in 8 bits");
    assert!(literal_parses("8'h00FF"), "`8'h00FF` is 255 in 8 bits");
    assert!(literal_parses("4'b0000_0001"), "`4'b0000_0001` is 1");
    assert!(literal_parses("8'd000255"), "`8'd000255` is 255");
}

/// A value too large for `u128` takes a different path — repeated
/// halving — than the common one, and that path has to be EXACT. The
/// obvious shortcut is `digits * log2(10)`, which over-counts whenever
/// the leading digit is small, and over-counting rejects a correct
/// program.
///
/// 10^39 is the discriminating case: 40 digits, exactly 130 bits, but
/// `40 * 3.3219 = 132`. 2^128 alone does NOT discriminate — it is 39
/// digits and the estimate lands on 129, the right answer — so a test
/// using only that value passes with the shortcut in place.
#[test]
fn a_decimal_literal_too_large_for_u128_is_still_sized_exactly() {
    // 2^128, one past `u128::MAX`: the boundary where the fast path ends.
    assert!(
        literal_parses("129'd340282366920938463463374607431768211456"),
        "2^128 fits in 129 bits"
    );
    let err = parse_err_with_literal("128'd340282366920938463463374607431768211456");
    assert!(
        err.contains("needs 129 bits") && err.contains("declares 128"),
        "got: {err}"
    );

    // 10^39: exactly 130 bits, which an estimate puts at 132.
    let e39 = "1000000000000000000000000000000000000000";
    assert!(
        literal_parses(&format!("130'd{e39}")),
        "10^39 is 130 bits and must be accepted; an over-counting estimate rejects it"
    );
    let err = parse_err_with_literal(&format!("129'd{e39}"));
    assert!(
        err.contains("needs 130 bits") && err.contains("declares 129"),
        "the >u128 path must be exact, not an estimate; got: {err}"
    );
}

/// `8'dFF` and `4'b1012` lexed, then emitted `uint64_t a = FF;` and
/// `0b1012` — neither is valid C++. The mismatch surfaced as a C++
/// compiler error on generated code instead of a HARC diagnostic.
#[test]
fn digits_outside_the_declared_radix_are_rejected() {
    let err = parse_err_with_literal("8'dFF");
    assert!(
        err.contains("digit `F`") && err.contains("not decimal"),
        "got: {err}"
    );
    let err = parse_err_with_literal("4'b1012");
    assert!(
        err.contains("digit `2`") && err.contains("not binary"),
        "got: {err}"
    );
}

/// `0'h0` declared zero bits and lowered as an ordinary value. Removing
/// it is what lets `literal_operand_bit_width` drop its `.max(1)` clamp.
#[test]
fn a_zero_width_sized_literal_is_rejected() {
    let err = parse_err_with_literal("0'h0");
    assert!(err.contains("width of 0"), "got: {err}");
}

/// `8'h_` matches the lexer's `[0-9a-fA-F_]+` tail but has no digits
/// once underscores are stripped.
#[test]
fn a_sized_literal_with_no_digits_is_rejected() {
    let err = parse_err_with_literal("8'h_");
    assert!(err.contains("no digits"), "got: {err}");
}

/// A `${...}` capture is not in the token stream — the whole string is
/// one `StringLit`, re-parsed later and separately by each backend — so a
/// sized literal written there bypassed the check completely. It bypassed
/// it into the two worst available outcomes: v1 discarded the re-parse
/// error and wrote the raw HARC text into the C++ (`harc_printf_ll(4'hFF)`,
/// which does not compile), and TB-IR reported it as an unimplemented
/// subset gap pointing at `--codegen v1`, i.e. at the backend that emits
/// that.
#[test]
fn a_sized_literal_inside_a_string_interpolation_is_checked() {
    let bad = |body: &str| {
        let src =
            format!("test T\n    let dut : Top\n    run\n        {body}\n    end run\nend test T");
        match parse_source(&src) {
            Ok(_) => panic!("expected rejection of: {body}"),
            Err(e) => e.to_string(),
        }
    };
    assert!(
        bad(r#"log(info, "b=${4'hFF}")"#).contains("needs 8 bits"),
        "an overwide literal in an interpolation must be rejected"
    );
    // With a format spec, so the capture/spec split matches
    // `process_interp`'s (expression is everything before the LAST `:`).
    assert!(
        bad(r#"log(info, "b=${4'hFF:04x}")"#).contains("needs 8 bits"),
        "the format-spec form must be checked too"
    );
    // Second capture bad: the scan must not stop at the first one.
    assert!(
        bad(r#"log(info, "${8'hFF} ${0'h0}")"#).contains("width of 0"),
        "every capture must be checked, not just the first"
    );
}

/// The other half of that check, and the one that would make it worse
/// than the bug: text OUTSIDE `${...}` is prose, not code. Rejecting a
/// log message that happens to mention `4'hFF` would break working
/// programs to fix a bug none of them had.
#[test]
fn a_sized_literal_in_plain_string_text_is_left_alone() {
    let ok = |body: &str| {
        let src =
            format!("test T\n    let dut : Top\n    run\n        {body}\n    end run\nend test T");
        parse_source(&src).is_ok()
    };
    assert!(
        ok(r#"log(info, "wrote 4'hFF to the register")"#),
        "prose mentioning an overwide literal must still parse"
    );
    assert!(
        ok(r#"log(info, "wrote 4'hFF, read ${8'hFF}")"#),
        "prose alongside a legal capture must still parse"
    );
    assert!(
        ok(r#"log(info, "unmatched ${4'hFF")"#),
        "an unmatched brace is not a capture — `process_interp` bails too"
    );
}

/// harc#593. A `${...}` capture was never validated, and `parse_expr_fragment`
/// does not require the fragment to be consumed — so a PARTIAL parse was
/// silently truncated by both backends. `${1 2}` printed `1`. Because the two
/// emitters agreed, no parity check could see it, and because the output was a
/// plausible number rather than a failure, nothing else would either.
#[test]
fn a_partially_parsing_interpolation_capture_is_rejected() {
    let bad = |body: &str| {
        let src =
            format!("test T\n    let dut : Top\n    run\n        {body}\n    end run\nend test T");
        match parse_source(&src) {
            Ok(_) => panic!("expected rejection of: {body}"),
            Err(e) => e.to_string(),
        }
    };
    // Truncation: each of these used to print the leading fragment.
    for c in ["1 2", "x + 1 total_garbage", "x 999"] {
        let err = bad(&format!(r#"log(info, "v=${{{c}}}")"#));
        assert!(
            err.contains("is not an expression"),
            "`${{{c}}}` must be rejected, not truncated; got: {err}"
        );
    }
    // Outright unparseable: these used to be pasted into the C++ verbatim
    // by v1, and reported as an unimplemented subset gap by TB-IR.
    for c in ["1 +", "dut.", "@@@"] {
        let err = bad(&format!(r#"log(info, "v=${{{c}}}")"#));
        assert!(
            err.contains("is not an expression"),
            "`${{{c}}}` must be rejected; got: {err}"
        );
    }
}

/// The ternary is the one shape whose rejection the author cannot explain
/// from the text: a capture is split on its LAST `:`, so `${a ? b : c}`
/// hands `a ? b` to the parser. It emitted uncompilable C++ before, so this
/// is not a regression — but the diagnostic has to say why, and name the
/// spelling that works.
#[test]
fn the_ternary_interpolation_collision_is_explained() {
    let src = |body: &str| {
        format!("test T\n    let dut : Top\n    run\n        {body}\n    end run\nend test T")
    };
    let err = parse_source(&src(r#"log(info, "v=${x > 3 ? 1 : 0}")"#))
        .expect_err("a bare ternary capture cannot work with the last-`:` split")
        .to_string();
    // The explanation is in the MESSAGE, not the `help`: an integration test
    // sees only `Display`, and a claim a test cannot read is a claim that
    // rots. The suggested spelling stays in `help`.
    assert!(
        err.contains("split on its LAST `:`") && err.contains("ternary"),
        "the diagnostic must explain the collision; got: {err}"
    );
    // With an explicit format spec the split point moves and it parses.
    parse_source(&src(r#"log(info, "v=${x > 3 ? 1 : 0:d}")"#))
        .expect("an explicit format spec moves the split and makes a ternary work");
}

/// An EMPTY capture is not an expression either, and `parse_expr_fragment("")`
/// fails — so exempting it from the check let `harc check` accept a program
/// both backends then rejected. That is the check-versus-backend split
/// harc#593 is about, reproduced by the first version of the fix for it.
///
/// The point of validating in the parser is that one place answers for
/// `harc check` and both backends, so this asserts the three agree.
#[test]
fn an_empty_interpolation_capture_is_rejected_by_the_parser() {
    for c in ["", ":04x", ":"] {
        let src = format!(
            "test T\n    let dut : Top\n    run\n        log(info, \"v=${{{c}}}\")\n    end run\nend test T"
        );
        let err = parse_source(&src)
            .expect_err(&format!("`${{{c}}}` must be rejected at parse time"))
            .to_string();
        assert!(err.contains("is not an expression"), "got: {err}");
    }
}

/// A bit-slice carries the ternary's exact root cause — its own `:` collides
/// with the last-`:` format-spec split — and has the exact same fix, so it
/// must get the same explanation rather than "you did not write an
/// expression", which is false: the author wrote one.
#[test]
fn the_bit_slice_interpolation_collision_is_explained_too() {
    let src = |body: &str| {
        format!("test T\n    let dut : Top\n    run\n        {body}\n    end run\nend test T")
    };
    let err = parse_source(&src(r#"log(info, "v=${a[1:0]}")"#))
        .expect_err("a bare bit-slice capture collides with the spec split")
        .to_string();
    assert!(
        err.contains("split on its LAST `:`") && err.contains("bit-slice"),
        "the diagnostic must name the bit-slice collision; got: {err}"
    );
    parse_source(&src(r#"log(info, "v=${a[1:0]:02x}")"#))
        .expect("an explicit format spec moves the split and makes a bit-slice work");
}

/// Captures that ARE expressions must still parse, including the format-spec
/// form — the whole risk of validating here is rejecting working programs.
#[test]
fn legal_interpolation_captures_still_parse() {
    let ok = |body: &str| {
        let src =
            format!("test T\n    let dut : Top\n    run\n        {body}\n    end run\nend test T");
        parse_source(&src).is_ok()
    };
    for c in [
        "x",
        "x + 1",
        "x:04x",
        "x + 1:08x",
        "dut.count_out",
        "cycle_count",
        "t.vals[0]",
        "(x + 1) * 2",
    ] {
        assert!(
            ok(&format!(r#"log(info, "v=${{{c}}}")"#)),
            "`${{{c}}}` must still parse"
        );
    }
    // Text outside `${...}` is prose and is not touched.
    assert!(
        ok(r#"log(info, "1 + and dut. and @@@ are fine in prose")"#),
        "prose must not be parsed as an expression"
    );
}

/// Sizing a decimal literal above `u128` is quadratic in the digit count,
/// and it runs in the parser on whatever the source says: 100 000 digits
/// took ~10s, and `MAX_SOURCE_LEN` is `u32::MAX`. Capped, with hex and
/// binary left uncapped because they are sized structurally.
#[test]
fn an_absurdly_long_sized_decimal_literal_is_capped() {
    let at_cap = format!("40000'd{}", "9".repeat(4096));
    assert!(
        literal_parses(&at_cap),
        "4096 digits is at the cap and must be accepted"
    );
    let over = format!("40000'd{}", "9".repeat(4097));
    let err = parse_err_with_literal(&over);
    assert!(
        err.contains("4097 digits") && err.contains("maximum is 4096"),
        "got: {err}"
    );
    // Hex is structural, so length costs nothing and is not capped.
    let long_hex = format!("400000'h{}", "F".repeat(50000));
    assert!(
        literal_parses(&long_hex),
        "a 50 000-digit hex literal is sized structurally and must parse"
    );
}

#[test]
fn an_absurdly_long_unsized_decimal_literal_is_capped() {
    let at_cap = "9".repeat(4096);
    assert!(
        literal_parses(&at_cap),
        "4096 unsized decimal digits is at the cap and must be accepted"
    );
    let over = "9".repeat(4097);
    let err = parse_err_with_literal(&over);
    assert!(
        err.contains("4097 digits") && err.contains("maximum is 4096"),
        "got: {err}"
    );
    let long_hex = format!("0x{}", "F".repeat(50000));
    assert!(
        literal_parses(&long_hex),
        "a 50 000-digit hexadecimal value is structural and remains uncapped"
    );
}

/// The point of rejecting rather than choosing. harc#565's table had two
/// programs that the same mask rule could not both satisfy while `4'hFF`
/// was legal: masking at the declared 4 made `keep (len +% 4'hFF) ==
/// 4'hFF` unsatisfiable, masking at the value's 8 did the same to `keep
/// len +% 4'hFF == 15`. Write the literal so its digits fit — `8'hFF` —
/// and the two spellings now agree, because one width describes both.
#[test]
fn a_sized_literal_masks_the_same_inside_and_outside_the_wrap() {
    let mask8 = "& harc_z3_bv_value(_ctx, (uint64_t)0x00000000000000ffULL, 64))";
    let outside = v1_cpp(
        r#"transaction Txn
    len : uint<4> default 0
    keep len +% 8'hFF == 15
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    assert!(
        outside.contains(mask8),
        "`len +% 8'hFF == 15` must mask at 8; got:\n{outside}"
    );

    let inside = v1_cpp(
        r#"transaction Txn
    len : uint<4> default 0
    keep (len +% 8'hFF) == 8'hFF
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.len}")
    end run
end test T"#,
    );
    assert!(
        inside.contains(mask8),
        "the same token inside and outside the wrap must mask identically; got:\n{inside}"
    );
}

/// Build a transaction with `keep <expr>` and a `randomize(t)`.
fn keep_src(constraint: &str) -> String {
    format!(
        r#"transaction Txn
    len : uint<8> default 0
    flag : uint<1> default 0
    keep {constraint}
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${{t.len}}")
    end run
end test T"#
    )
}

/// `keep_src` plus a signed one-bit field and an enum field — the two
/// shapes that are one bit wide, or scalar, yet still not flags.
fn keep_src_enum(constraint: &str) -> String {
    format!(
        r#"enum Mode {{ A, B }}
transaction Txn
    len : uint<8> default 0
    flag : uint<1> default 0
    sflag : sint<1> default 0
    mode : Mode
    keep {constraint}
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${{t.len}}")
    end run
end test T"#
    )
}

/// harc#560. A constraint has to BE a proposition. The top-level `_s.add`
/// site emitted through the VALUE emitter, so an arithmetic, bitwise,
/// shift or wrapping operator produced a bitvector, which
/// `z3::solver::add` — which takes a Bool — accepted at build time and
/// rejected at runtime as a sort error.
///
/// Both backends share this emitter, so both must reject: a one-sided
/// rejection here would be the exact divergence harc#551 is about.
/// EVERY value operator, not a sample. An earlier version of this test
/// listed nine of them, and deleting `- * / % -% *%` and unary `-` from
/// the rule left all 263 tests passing — a compiler that emitted
/// `_s.add(_z_len - 1)` with no complaint. The spelling assertion is part
/// of it: `-%` and `*%` are the two ops whose HARC spelling differs from
/// the C++ one, so they are where a spelling helper regresses silently.
#[test]
fn a_value_operator_cannot_be_a_whole_constraint() {
    let binaries = [
        ("len + 1", "+"),
        ("len - 1", "-"),
        ("len * 2", "*"),
        ("len / 2", "/"),
        ("len % 2", "%"),
        ("len +% 1", "+%"),
        ("len -% 1", "-%"),
        ("len *% 2", "*%"),
        ("len & 3", "&"),
        ("len | 3", "|"),
        ("len ^ 3", "^"),
        ("len << 2", "<<"),
        ("len >> 2", ">>"),
        ("(len +% 1)", "+%"),
    ];
    for (c, spelling) in binaries {
        let src = keep_src(c);
        let v1 = v1_emit_err(&src);
        assert!(
            v1.contains(&format!("`{spelling}` produces a value, not a condition")),
            "v1 must reject `keep {c}` naming `{spelling}`; got: {v1}"
        );
        let tbir = tbir_constraint_snippets(&src)
            .expect_err(&format!("the default backend must reject `keep {c}` too"));
        assert!(
            tbir.contains(&format!("`{spelling}` produces a value, not a condition")),
            "the default backend must reject `keep {c}` the same way, not diverge; got: {tbir}"
        );
    }
    for (c, spelling) in [("~len", "~"), ("-len", "-")] {
        let v1 = v1_emit_err(&keep_src(c));
        assert!(
            v1.contains(&format!("`{spelling}` produces a value, not a condition")),
            "v1 must reject `keep {c}` naming `{spelling}`; got: {v1}"
        );
    }
}

/// The rule has to reach bare references too, and that is where the first
/// version of this fix stopped short: only a one-bit UNSIGNED field was
/// coerced to `!= 0`, and every other reference fell through to the value
/// emitter with no diagnostic — so `keep len` on a `uint<8>` still handed
/// `z3::solver::add` a bitvector and still died at runtime, which is the
/// whole bug.
#[test]
fn a_bare_reference_that_is_not_a_flag_cannot_be_a_constraint() {
    for (c, needle) in [
        ("len", "`len` is a value, not a condition"),
        ("sflag", "`sflag` is a value, not a condition"),
        ("mode", "`mode` is a value, not a condition"),
        ("len[0]", "bit-select or list element is a value"),
        ("len[3:0]", "bit-slice is a value"),
    ] {
        let src = keep_src_enum(c);
        let v1 = v1_emit_err(&src);
        assert!(v1.contains(needle), "v1 must reject `keep {c}`; got: {v1}");
        let tbir = tbir_constraint_snippets(&src)
            .expect_err(&format!("the default backend must reject `keep {c}` too"));
        assert!(
            tbir.contains(needle),
            "the default backend must reject `keep {c}` identically; got: {tbir}"
        );
    }
}

/// `keep len & 3 == 0` is the case from the issue, and the one worth a
/// hint rather than a bare rejection. HARC binds `&` looser than `==`,
/// exactly as C++ does, so it parses as `len & (3 == 0)` — the top
/// operator is `&`, which is why the one rule above catches it.
///
/// The fix must NOT be to parenthesise it in codegen: the emitter would
/// then contradict the parser and silently change what the program means.
/// So the diagnostic states the grouping and lets the author choose.
#[test]
fn the_bitwise_precedence_surprise_is_explained_not_guessed_at() {
    let err = v1_emit_err(&keep_src("len & 3 == 0"));
    assert!(
        err.contains("binds LOOSER than the comparison operators"),
        "the diagnostic must name the precedence, not just reject; got: {err}"
    );
    // Says "the comparison operators" rather than `==`, because `a & b != c`
    // and `a & b < c` reach the same message and quoting `==` at an author
    // who wrote `!=` describes someone else's program.
    let ne = v1_emit_err(&keep_src("len & 3 != 0"));
    assert!(
        ne.contains("binds LOOSER than the comparison operators"),
        "the `!=` form gets the same explanation; got: {ne}"
    );
    assert!(
        err.contains("`a & (b == c)`") && err.contains("`(a & b) == c`"),
        "it must show both groupings so the author picks; got: {err}"
    );
    // The shift and arithmetic forms bind TIGHTER than `==`, so the
    // precedence note would be false for them and must not appear.
    let shift = v1_emit_err(&keep_src("len << 2"));
    assert!(
        !shift.contains("binds LOOSER"),
        "the precedence note is only true of the bitwise operators; got: {shift}"
    );
}

/// A bare integer is a value too. It used to reach `add()` as a bitvector.
#[test]
fn a_bare_literal_cannot_be_a_constraint() {
    let err = v1_emit_err(&keep_src("1"));
    assert!(
        err.contains("is a value, not a condition") && err.contains("`x == 1`"),
        "got: {err}"
    );
}

/// Routing the top-level assertion through the Bool emitter also fixes a
/// bare one-bit field, which used to be emitted as a raw bitvector at the
/// top level while the SAME field under `&&` already got the `!= 0`
/// coercion. This is the only emitted-text change in harc#560's fix.
#[test]
fn a_bare_one_bit_field_constraint_coerces_to_a_comparison() {
    let cpp = v1_cpp(&keep_src("flag"));
    assert!(
        cpp.contains("_s.add(_z_flag != _ctx.bv_val((uint64_t)0, 64))"),
        "a bare one-bit field must be compared, not asserted as a bitvector; got:\n{cpp}"
    );
}

/// The other half: everything that IS a proposition still emits, and the
/// parenthesised form of the rejected shape is the one the diagnostic
/// recommends — so it had better work.
#[test]
fn real_conditions_still_emit_after_the_bool_position_check() {
    // Each case asserts the DISTINCTIVE text it lowers to, not merely that
    // some `_s.add(` exists. A transaction emits four of those before any
    // `keep` is considered — two range assumptions and two randomize
    // preferences — so `contains("_s.add(")` passed even against an
    // implementation that dropped every constraint on the floor.
    let cases = [
        (
            "(len & 3) == 0",
            "(_z_len & _ctx.bv_val((uint64_t)3, 64)) == _ctx.bv_val((uint64_t)0, 64)",
        ),
        (
            "(len << 2) == 0",
            "(_z_len << _ctx.bv_val((uint64_t)2, 64)) == _ctx.bv_val((uint64_t)0, 64)",
        ),
        ("len == 3", "_s.add(_z_len == _ctx.bv_val((uint64_t)3, 64))"),
        ("len != 3", "_s.add(_z_len != _ctx.bv_val((uint64_t)3, 64))"),
        (
            "len +% 1 == 5",
            "((_z_len + _ctx.bv_val((uint64_t)1, 64)) &",
        ),
        (
            "len in [1..16]",
            "z3::uge(_z_len, _ctx.bv_val((uint64_t)1, 64))",
        ),
        // The `!= 0` coercion, and the same coercion under a `Paren`.
        ("flag", "_s.add(_z_flag != _ctx.bv_val((uint64_t)0, 64))"),
        (
            "(flag)",
            "_s.add((_z_flag != _ctx.bv_val((uint64_t)0, 64)))",
        ),
        (
            "not flag",
            "_s.add(!(_z_flag != _ctx.bv_val((uint64_t)0, 64)))",
        ),
        (
            "len == 1 && flag",
            "&& _z_flag != _ctx.bv_val((uint64_t)0, 64)",
        ),
        ("len == 1 || len == 2", "||"),
        // Bool literals. Routing the top level through the Bool emitter
        // changed these from `bv_val(1, 64)` / `bv_val(0, 64)` — bitvectors
        // handed to `add()` — to real Bools. Cheapest possible canary for
        // the routing, and unpinned until the review pointed it out.
        ("true", "_s.add(_ctx.bool_val(true))"),
        ("false", "_s.add(_ctx.bool_val(false))"),
    ];
    for (c, needle) in cases {
        let src = keep_src(c);
        let cpp = v1_cpp(&src);
        assert!(
            cpp.contains(needle),
            "`keep {c}` must emit `{needle}`; got:\n{cpp}"
        );
        tbir_constraint_snippets(&src)
            .unwrap_or_else(|e| panic!("`keep {c}` must still lower under tbir: {e}"));
    }
}

/// `a in b` on a scalar lowers to `a == b`, and that `==` is NOT the
/// operator the author wrote: HARC puts `in` at relational precedence,
/// tighter than `==`, while C++'s `==` is looser than a relational
/// operator. So `keep sf in 3 < 5`, grouped by HARC as `(sf in 3) < 5`,
/// re-parsed in C++ as `sf == (3 < 5)`. Its sibling range and set arms
/// already parenthesised; this one did not.
///
/// This is a genuine codegen grouping bug, and the distinction from
/// harc#560's reported one matters: here the emitter substitutes an
/// operator, so it owns the grouping. There, it reproduced the operator
/// the author wrote, so the parser owned it.
#[test]
fn a_scalar_membership_lowering_parenthesises_its_substituted_operator() {
    let cpp = v1_cpp(&keep_src("len in 3"));
    assert!(
        cpp.contains("_s.add((_z_len == _ctx.bv_val((uint64_t)3, 64)))"),
        "the substituted `==` must be parenthesised; got:\n{cpp}"
    );
}

/// harc#563, part 1. A signed `let` inlined into a `blocking randomize`
/// constraint compared UNSIGNED, because the signedness oracle resolved
/// only `field_info`. `(uint64_t)(int8_t)-1` is `0xFFFF...FF`, so
/// `z3::ult(_z_x, that)` is true for every `x` — the solver returned a
/// value the source `x < -1` forbids outright.
///
/// The declared signedness rides on the existing `let_widths` entry rather
/// than a table of its own; harc#550 removed a flat `let_signed_widths`
/// because a signed local in one function poisoned the name for all others.
#[test]
fn a_signed_local_compares_signed_in_a_blocking_constraint() {
    let src = |ty: &str, init: &str| {
        format!(
            r#"transaction Txn
    x : uint<8> default 0
end transaction Txn
test T
    let dut : Top
    run
        let s : {ty} = {init}
        let t : Txn
        blocking randomize(t) with
            t.x < s
        end randomize
        log(info, "${{t.x}}")
    end run
end test T"#
        )
    };
    let signed = v1_cpp(&src("sint<8>", "0 - 1"));
    assert!(
        signed.contains("_s.add(_z_x < _ctx.bv_val((uint64_t)(s), 64))"),
        "a signed local must use the signed predicate; got:\n{signed}"
    );
    assert!(
        !signed.contains("z3::ult(_z_x, _ctx.bv_val((uint64_t)(s)"),
        "and must not use `ult`; got:\n{signed}"
    );
    // The other direction matters as much: an unsigned local must NOT be
    // dragged onto the signed predicate by the same lookup.
    let unsigned = v1_cpp(&src("uint<8>", "7"));
    assert!(
        unsigned.contains("z3::ult(_z_x, _ctx.bv_val((uint64_t)(s), 64))"),
        "an unsigned local must stay unsigned; got:\n{unsigned}"
    );
}

/// `let_widths` is keyed by bare name with NO scoping, so a shadowed name's
/// entry belongs to whichever `let` was seen last rather than the one in
/// scope. Signedness cannot ride that out the way a width check can:
/// reading the wrong entry flips `udiv` to `/`, changing the solved value.
///
/// The first version of the harc#563 fix ignored the `shadowed_lets` guard
/// the width oracle already consults, so an inner-block `let m : sint<64>`
/// made an OUTER `uint<64>` `m` divide signed — a regression in a
/// constraint that mentions neither block. A shadowed name now answers
/// unsigned: under-reporting, i.e. the pre-harc#563 answer, rather than a
/// new wrong predicate.
#[test]
fn a_shadowed_local_does_not_flip_the_predicate() {
    let shadowed = v1_cpp(
        r#"transaction Txn
    u : uint<64> default 0
end transaction Txn
test T
    let dut : Top
    run
        let m : uint<64> = 10000000000000000000
        if dut.en == 1
            let m : sint<64> = 0 - 1
            log(info, "inner=${m}")
        end if
        let t : Txn
        blocking randomize(t) with
            t.u == m / 2
        end randomize
        log(info, "${t.u}")
    end run
end test T"#,
    );
    assert!(
        shadowed.contains("z3::udiv(_ctx.bv_val((uint64_t)(m), 64)"),
        "the outer `m` is unsigned; an inner shadow must not make it divide \
         signed; got:\n{shadowed}"
    );
    // And the guard must not swallow the fix for an UNSHADOWED local.
    let plain = v1_cpp(
        r#"transaction Txn
    x : uint<8> default 0
end transaction Txn
test T
    let dut : Top
    run
        let s : sint<8> = 0 - 1
        let t : Txn
        blocking randomize(t) with
            t.x < s
        end randomize
        log(info, "${t.x}")
    end run
end test T"#,
    );
    assert!(
        plain.contains("_s.add(_z_x < _ctx.bv_val((uint64_t)(s), 64))"),
        "an unshadowed signed local must still compare signed; got:\n{plain}"
    );
}

/// harc#563, part 2. The oracle had no `Index` arm, so an element of a
/// signed list compared unsigned: `v < 0` over a `list<sint<8>>` emitted
/// `z3::ult(_z_vals_0, 0)`, which no value satisfies, where the source is
/// satisfied by any negative element.
///
/// The element's signedness lives on `ListFieldInfo::elem_signed`, not on
/// the field's own `signed` — which is false for `list<sint<8>>`, and
/// reading it kept the bug alive through the first attempt at this fix.
#[test]
fn a_signed_list_element_compares_signed() {
    let src = |elem: &str| {
        format!(
            r#"transaction Txn
    vals : list<{elem}>
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t) with
            t.vals.len() == 2
            for v in t.vals
                v < 0
            end for
        end randomize
        log(info, "${{t.vals[0]}}")
    end run
end test T"#
        )
    };
    let signed = v1_cpp(&src("sint<8>"));
    assert!(
        signed.contains("|| (_z_vals_0 < _ctx.bv_val((uint64_t)0, 64))"),
        "a signed list element must use the signed predicate; got:\n{signed}"
    );
    assert!(
        !signed.contains("z3::ult(_z_vals_0"),
        "and must not use `ult`; got:\n{signed}"
    );
    let unsigned = v1_cpp(&src("uint<8>"));
    assert!(
        unsigned.contains("|| (z3::ult(_z_vals_0, _ctx.bv_val((uint64_t)0, 64)))"),
        "an unsigned list element must stay unsigned; got:\n{unsigned}"
    );
}

/// harc#598. `sum(...)` was the OTHER way a signed list reaches a
/// constraint, and fixing only the `Index` arm left it comparing unsigned:
/// `z3::ult(<sum>, 0)` is false for every value, where the source is
/// satisfied by any negative element.
/// BOTH argument shapes, because the emitter accepts both and the oracle
/// has to agree with it about each. Covering only the sliced form left
/// `sum(vals)` still emitting `z3::ult` over a signed list — harc#598
/// unfixed in its sibling syntax, and a brute force over a two-element list
/// put 65664 solutions in the emitted constraint that the source forbids.
#[test]
fn a_sum_over_a_signed_list_compares_signed() {
    let src = |elem: &str, arg: &str| {
        format!(
            r#"transaction Txn
    vals : list<{elem}>
    keep vals.len() <= 2
    keep sum({arg}) < 0
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${{t.vals[0]}}")
    end run
end test T"#
        )
    };
    for arg in ["vals[0..vals.len()]", "vals"] {
        let signed = v1_cpp(&src("sint<8>", arg));
        assert!(
            signed.contains(") < _ctx.bv_val((uint64_t)0, 64))")
                && !signed.contains("z3::ult((z3::ite"),
            "`sum({arg})` over a signed list must use the signed predicate; got:\n{signed}"
        );
        let unsigned = v1_cpp(&src("uint<8>", arg));
        assert!(
            unsigned.contains("z3::ult((z3::ite"),
            "`sum({arg})` over an unsigned list must stay unsigned; got:\n{unsigned}"
        );
        // A `.len()` call is unsigned and must not be dragged along by the
        // new `Call` arm — it shares the arm and differs only by callee.
        assert!(
            signed.contains("z3::ule(_z_vals_len"),
            "`.len()` must stay unsigned; got:\n{signed}"
        );
    }
}

/// The membership call site passes `target_root` too — a nested signed
/// field in `x in [lo..hi]` must compare signed.
#[test]
fn membership_range_signedness_resolves_the_dotted_path() {
    let cpp = v1_cpp(
        r#"struct Hdr
    s : sint<64>
end struct Hdr
transaction Txn
    hdr : Hdr
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t) with
            t.hdr.s in [0..100]
        end randomize
        log(info, "x")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("_z_hdr_s >= ") && !cpp.contains("z3::uge(_z_hdr_s"),
        "a signed nested field in a range must use the signed comparison; got:\n{cpp}"
    );
}

/// `randomize(t) with t +% 10 == 5` on a transaction whose field is named
/// after the target variable. Routing a bare ident through
/// `expr_field_path` strips the target root off the single-element path
/// and resolves nothing, so this has to resolve the way the emitter's own
/// `Ident` arm does — by name.
#[test]
fn bare_ident_named_after_the_randomize_target_still_resolves() {
    let cpp = v1_cpp(
        r#"transaction Txn
    t : uint<8> default 0
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t) with
            t +% 10 == 5
        end randomize
        log(info, "${t.t}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains(
            "((_z_t + _ctx.bv_val((uint64_t)10, 64)) \
             & harc_z3_bv_value(_ctx, (uint64_t)0x00000000000000ffULL, 64)) =="
        ),
        "a field named after the randomize target must still carry its width; got:\n{cpp}"
    );
}

/// A bit-slice operand's width is the slice, not the field it came from.
#[test]
fn bit_slice_wrap_operands_mask_to_the_slice_width() {
    let cpp = v1_cpp(
        r#"transaction Txn
    a : uint<8> default 0
    keep a[3:0] +% 8 == 0
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.a}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains(
            "+ _ctx.bv_val((uint64_t)8, 64)) \
                      & harc_z3_bv_value(_ctx, (uint64_t)0x000000000000000fULL, 64)) =="
        ),
        "a [3:0] slice wraps at 4 bits, not the field's 8; got:\n{cpp}"
    );
}

/// `emit_constraint_bit_slice_expr` does not bound `hi` by the field's
/// width (unlike the bit-select path), so a slice can claim a width above
/// the solver bitvector. That residue is not representable, so there is no
/// mask to apply and it must be rejected rather than emitted unmasked.
#[test]
fn a_wrap_wider_than_the_solver_bitvector_is_rejected() {
    let err = v1_emit_err(
        r#"transaction Txn
    a : uint<8> default 0
    keep a[70:0] +% 1 == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
    end run
end test T"#,
    );
    assert!(
        err.contains("wider than the 64-bit solver bitvector"),
        "expected the unrepresentable-residue diagnostic; got: {err}"
    );
}

/// `<` vs `z3::ult` (and `/` vs `udiv`, `%` vs `urem`) are different Z3
/// predicates over the same variable, so resolving a nested field by its
/// leaf name is a wrong solved value, not a cosmetic difference. A field
/// as wide as `solver_width` carries no range assumption at all, so under
/// `bvslt` the solver may return a value the source constraint forbids.
#[test]
fn nested_field_signedness_is_not_taken_from_a_same_named_top_level_field() {
    let cpp = v1_cpp(
        r#"struct Hdr
    v : uint<64>
end struct Hdr
transaction Txn
    v : sint<64> default 0
    hdr : Hdr
    keep hdr.v < 100
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
        log(info, "${t.v}")
    end run
end test T"#,
    );
    assert!(
        cpp.contains("z3::ult(_z_hdr_v, _ctx.bv_val((uint64_t)100, 64))"),
        "`hdr.v` is unsigned and must compare with ult, not inherit the top-level \
         `v : sint<64>`'s signedness; got:\n{cpp}"
    );
}

/// A wrap operand with no statically known width has no defined mask, and
/// is rejected rather than silently solved unmasked — matching what both
/// emitters already do for a wrap in statement position. The input has to
/// be one that produces *only* this diagnostic: an undefined name errors
/// on its own, so it would pass with the whole check deleted.
#[test]
fn unknown_width_wrap_operand_in_a_constraint_is_rejected() {
    let err = v1_emit_err(
        r#"transaction Txn
    len : uint<8> default 0
    keep (len + 1) +% 10 == 5
end transaction Txn
test T
    let dut : Top
    run
        let t : Txn
        randomize(t)
    end run
end test T"#,
    );
    assert!(
        err.contains("statically known bit-width"),
        "expected the wrap-width diagnostic; got: {err}"
    );
}

/// A `let` that names a transactor and carries an initializer is not one
/// of the instantiation forms (`active` / `passive`, plus `= bind <bus>`
/// for a `bound to` transactor). It used to reach the generic
/// initialized-local path, where `c_type_for`'s `TypeExpr::Named` arm
/// means "Verilator DUT handle" and produced `VDrv* drv = drv();` — a
/// handle type for something that is not a DUT. The failure surfaced as
/// clang's `unknown type name 'VDrv'` against a generated line, with
/// nothing tying it back to the `let`.
///
/// `drv()` is deliberately not a declared function: the point is that
/// the initializer was never validated as a callable either, so this is
/// the exact source that shipped the broken C++.
#[test]
fn transactor_let_with_an_initializer_is_rejected_with_the_supported_forms() {
    let err = v1_emit_err(
        r#"transaction RegOp
    addr : uint<8>
end transaction RegOp

bus BusLite
    handshake_channel w: send kind: valid_ready
        addr : uint<8>
    end handshake_channel w
end bus BusLite

transactor Drv bound to BusLite
    when active
        req : in event<RegOp>
        on req(t)
            bus.w.send(t.addr)
        end on
    end when
end transactor Drv

test BadXactorInstantiationTest
    let dut : DummyDut
    let axil : BusLite = bind dut
    let drv : Drv = drv()
    run
        wait 1 cycle
    end run
end test BadXactorInstantiationTest"#,
    );
    assert!(
        err.contains("transactor `Drv` has no value form"),
        "expected the instantiation-form diagnostic to name the transactor; got: {err}"
    );
    assert!(
        err.contains("`let <name> : Drv active = bind <bus-binding>`")
            && err.contains("`let <name> : Drv passive = bind <bus-binding>`"),
        "the diagnostic must name the supported forms for a `bound to` transactor; got: {err}"
    );
}

/// The same guard for an unbound transactor names the two mode forms and
/// does NOT suggest `= bind`, which has no bus to bind to.
#[test]
fn unbound_transactor_let_with_an_initializer_names_only_the_mode_forms() {
    let err = v1_emit_err(
        r#"transactor Helper
    n : uint<8>
end transactor Helper

test BadUnboundXactorTest
    let dut : DummyDut
    let h : Helper = mk()
    run
        wait 1 cycle
    end run
end test BadUnboundXactorTest"#,
    );
    assert!(
        err.contains("transactor `Helper` has no value form")
            && err.contains("`let <name> : Helper active` or `let <name> : Helper passive`"),
        "expected the unbound-transactor mode forms; got: {err}"
    );
    assert!(
        !err.contains("bind <bus-binding>"),
        "an unbound transactor has no bus to bind — the hint must not suggest it; got: {err}"
    );
}

/// Same root cause, same guard: a composite component instance with an
/// initializer also has no value form. Covered here because the fix is
/// one rule over the instantiation-only families, not a transactor
/// special case — a regression that re-narrowed it to transactors would
/// otherwise go unnoticed.
#[test]
fn component_let_with_an_initializer_is_rejected() {
    let err = v1_emit_err(
        r#"agent Ag
    n : uint<8>
end agent Ag

test BadComponentInstantiationTest
    let dut : DummyDut
    let a : Ag = mk()
    run
        wait 1 cycle
    end run
end test BadComponentInstantiationTest"#,
    );
    assert!(
        err.contains("agent `Ag` has no value form") && err.contains("`let <name> : Ag`"),
        "expected the component instantiation diagnostic; got: {err}"
    );
}

/// The guard must not swallow the legal forms. A record-typed copy is a
/// value, not an instantiation, and the DUT handle type keeps its
/// `V<Name>*` mapping — the two things `instantiation_only_forms`
/// returns `None` for.
#[test]
fn record_copies_and_transactor_mode_forms_still_emit() {
    let parsed = parse_source(
        r#"transaction Txn
    v : uint<8>
end transaction Txn

transactor Helper
    n : uint<8>
end transactor Helper

test GoodInstantiationTest
    let dut : DummyDut
    let h : Helper passive
    run
        let t1 : Txn
        let t2 : Txn = t1
        log(info, "${t2.v}")
    end run
end test GoodInstantiationTest"#,
    )
    .unwrap();
    let cpp = cpp_tb::emit(&parsed).expect("emit");
    assert!(
        cpp.contains("Txn t2 = t1;"),
        "a record-typed copy must keep its by-value C++ type; got:\n{cpp}"
    );
    assert!(
        cpp.contains("Helper h;"),
        "a mode-annotated transactor let must still default-construct; got:\n{cpp}"
    );
    assert!(
        !cpp.contains("VTxn*") && !cpp.contains("VHelper*"),
        "no HARC-declared type may lower to a Verilator handle; got:\n{cpp}"
    );
}

fn independent_fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[test]
fn v1_common_split_artifact_bytes_are_stable() {
    let parsed = parse_source(
        r#"function add_one(x: uint<8>) -> uint<8>
    return x + 1
end function add_one

test StableA
    let dut : StableTop
    run
        dut.a = add_one(2)
        wait 1 cycle
        assert dut.sum == 3
    end run
end test StableA

test StableB
    let dut : StableTop
    run
        dut.a = add_one(8)
        wait 1 cycle
        assert dut.sum == 9
    end run
end test StableB"#,
    )
    .expect("parse stable v1 common fixture");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge stable fixture");
    let output = cpp_tb::emit_common_split(
        &merged,
        cpp_tb::EmitOpts::default(),
        "stable__",
        &[
            "top=StableTop".to_string(),
            "mt=false".to_string(),
            "coverage=false".to_string(),
            "waves=".to_string(),
        ],
    )
    .expect("emit stable v1 common fixture");

    let actual: Vec<(String, String)> = output
        .files
        .iter()
        .map(|file| {
            (
                file.filename.clone(),
                independent_fnv1a_hex(file.contents.as_bytes()),
            )
        })
        .collect();
    let expected = vec![
        (
            "stable__suite_api.hpp".to_string(),
            "5be73ffad52a5128".to_string(),
        ),
        (
            "stable__runtime.cpp".to_string(),
            "9637a52f54635dc0".to_string(),
        ),
        (
            "stable__test_StableA.cpp".to_string(),
            "81f4ffa8a5677485".to_string(),
        ),
        (
            "stable__test_StableB.cpp".to_string(),
            "dd5f025a3216c233".to_string(),
        ),
        (
            "stable__registry.cpp".to_string(),
            "ae9475597705faa6".to_string(),
        ),
    ];
    assert_eq!(actual, expected);
    assert_eq!(output.interface_abi, "64187f3f57f0a4c7");
    assert_eq!(output.build_profile, "f11cada50936fa42");
    assert_eq!(
        output
            .artifact_plan
            .render_manifest(&output.interface_abi, &output.build_profile)
            .expect("render stable manifest"),
        "{\"schema_version\":1,\"interface_abi\":\"64187f3f57f0a4c7\",\"build_profile\":\"f11cada50936fa42\",\"tests\":[\"StableA\",\"StableB\"],\"artifacts\":[\"stable__suite_api.hpp\",\"stable__runtime.cpp\",\"stable__test_StableA.cpp\",\"stable__test_StableB.cpp\",\"stable__registry.cpp\",\"harc_thread_rt.h\",\"harc_random_rt.h\",\"harc_queue_rt.h\",\"harc_trace_rt.h\",\"harc_log_rt.h\",\"harc_z3_rt.h\"]}\n"
    );
}

#[test]
fn v1_common_test_constraint_edit_preserves_interface_and_unrelated_capsule() {
    let render = |alpha_constraint: u8| {
        let parsed = parse_source(&format!(
            r#"transaction Req
    addr : uint<8>
end transaction Req

test Alpha
    let dut : StableTop
    run
        let req : Req
        randomize(req) with
            req.addr != {alpha_constraint}
        end randomize
    end run
end test Alpha

test Bravo
    let dut : StableTop
    run
        let req : Req
        randomize(req) with
            req.addr != 99
        end randomize
    end run
end test Bravo"#
        ))
        .expect("parse v1-common constraint incrementality fixture");
        let merged = merge::merge_for_sim(vec![parsed], None)
            .expect("merge v1-common constraint incrementality fixture");
        cpp_tb::emit_common_split(
            &merged,
            cpp_tb::EmitOpts::default(),
            "constraint_incr__",
            &[],
        )
        .expect("emit v1-common constraint incrementality fixture")
    };

    let before = render(7);
    let after = render(8);
    fn contents<'a>(output: &'a cpp_tb::CommonSplitOutput, name: &str) -> &'a str {
        output
            .files
            .iter()
            .find(|file| file.filename == name)
            .unwrap_or_else(|| panic!("missing generated artifact {name}"))
            .contents
            .as_str()
    }

    assert_eq!(before.interface_abi, after.interface_abi);
    assert_eq!(
        contents(&before, "constraint_incr__suite_api.hpp"),
        contents(&after, "constraint_incr__suite_api.hpp")
    );
    assert_eq!(
        contents(&before, "constraint_incr__test_Bravo.cpp"),
        contents(&after, "constraint_incr__test_Bravo.cpp")
    );
    assert_eq!(
        contents(&before, "constraint_incr__registry.cpp"),
        contents(&after, "constraint_incr__registry.cpp")
    );
    assert_ne!(
        contents(&before, "constraint_incr__test_Alpha.cpp"),
        contents(&after, "constraint_incr__test_Alpha.cpp")
    );
    assert_ne!(
        contents(&before, "constraint_incr__runtime.cpp"),
        contents(&after, "constraint_incr__runtime.cpp")
    );
    let interface = contents(&before, "constraint_incr__suite_api.hpp");
    assert!(!interface.contains("_harc_runtime_random_problem_table"));
    assert!(!interface.contains("_solver_site_"));
    assert!(contents(&before, "constraint_incr__test_Alpha.cpp").contains("_harc_randomize_state"));
}

#[test]
fn v1_common_context_alias_is_capsule_local() {
    let render_bravo = |alpha_local: &str| {
        let parsed = parse_source(&format!(
            r#"test Alpha
    let dut : StableTop
    run
{alpha_local}        wait 1 cycle
    end run
end test Alpha

test Bravo
    let dut : StableTop
    run
        wait 1 cycle
    end run
end test Bravo"#
        ))
        .expect("parse context-alias fixture");
        let merged = merge::merge_for_sim(vec![parsed], None).expect("merge context-alias fixture");
        let output =
            cpp_tb::emit_common_split(&merged, cpp_tb::EmitOpts::default(), "stable_ctx__", &[])
                .expect("emit context-alias fixture");
        output
            .files
            .iter()
            .find(|file| file.filename == "stable_ctx__test_Bravo.cpp")
            .expect("Bravo capsule")
            .contents
            .clone()
    };

    let before = render_bravo("");
    let after = render_bravo(
        "        let ctx : uint<8> = 1\n        let _harc_randomize_context : uint<8> = 2\n",
    );
    assert_eq!(before, after);
}

#[test]
fn v1_common_split_accepts_the_legacy_empty_prefix() {
    let parsed = parse_source(
        r#"test EmptyPrefix
    let dut : EmptyPrefixTop
    run
        wait 1 cycle
    end run
end test EmptyPrefix"#,
    )
    .expect("parse empty-prefix fixture");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("merge empty-prefix fixture");
    let output = cpp_tb::emit_common_split(&merged, cpp_tb::EmitOpts::default(), "", &[])
        .expect("the public v1 common API accepts an empty prefix");
    assert_eq!(
        output
            .files
            .iter()
            .map(|file| file.filename.as_str())
            .collect::<Vec<_>>(),
        vec![
            "suite_api.hpp",
            "runtime.cpp",
            "test_EmptyPrefix.cpp",
            "registry.cpp",
        ]
    );
    assert_eq!(output.artifact_plan.manifest_filename(), "artifacts.json");
}
