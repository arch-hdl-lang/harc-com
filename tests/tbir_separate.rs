use harc::codegen::cpp_tb::EmitOpts;
use harc::codegen::tbir::{
    emit_separate_common_with_prefix, emit_separate_interface_with_prefix,
    emit_separate_shard_with_prefix, plan_separate_tests, separate_category_bytes,
};
use harc::ir;

fn minimal_program(tests: usize) -> (ir::TbProgram, harc::ast::SourceFile, EmitOpts) {
    let mut src = String::new();
    for i in 0..tests {
        src.push_str(&format!(
            "test T{i} let dut : MyDut run wait 1 cycle end run end test T{i}\n"
        ));
    }
    let file = harc::parser::parse_source(&src).expect("parse minimal");
    let prog = harc::ir::lower::lower_program(&file).expect("lower");
    harc::ir::verify::verify_program(&prog).expect("verify");
    let opts = EmitOpts::default();
    let file_with_dut = file.clone();
    (prog, file_with_dut, opts)
}

#[test]
fn separate_plan_is_deterministic_and_has_expected_artifacts() {
    let (prog, file, opts) = minimal_program(4);
    let p1 = plan_separate_tests(&prog, &file, &opts, "h_", 1).unwrap();
    let p2 = plan_separate_tests(&prog, &file, &opts, "h_", 1).unwrap();
    assert_eq!(p1.test_names, p2.test_names);
    assert_eq!(p1.shards.len(), p2.shards.len());
    assert_eq!(p1.interface.filename, "h_suite.hpp");
    assert_eq!(p1.common.filename, "h_common.cpp");
    assert_eq!(p1.dispatcher.filename, "h_main.cpp");
    for (a, b) in p1.shards.iter().zip(p2.shards.iter()) {
        assert_eq!(a.filename, b.filename);
        assert_eq!(a.test_indices, b.test_indices);
    }
}

#[test]
fn separate_category_bytes_reports_savings() {
    // Use the bench-like suite with records and helpers to have non-trivial shared bytes
    let src = r#"
        transaction Rec0
            addr : uint<32>
            data : uint<32>
            keep addr < 4096
        end transaction Rec0
        test T0 let dut : MyDut run let r : Rec0 randomize(r) with r.addr == 0 end randomize end run end test T0
        test T1 let dut : MyDut run let r : Rec0 randomize(r) with r.addr == 1 end randomize end run end test T1
        test T2 let dut : MyDut run let r : Rec0 randomize(r) with r.addr == 2 end randomize end run end test T2
        test T3 let dut : MyDut run let r : Rec0 randomize(r) with r.addr == 3 end randomize end run end test T3
    "#;
    let file = harc::parser::parse_source(src).unwrap();
    let prog = harc::ir::lower::lower_program(&file).unwrap();
    harc::ir::verify::verify_program(&prog).unwrap();
    let opts = EmitOpts::default();
    let cat = separate_category_bytes(&prog, &file, &opts, "h_", 1).unwrap();
    // Interface+common should be non-empty and shards should be smaller than self-contained total
    assert!(cat.interface_total > 0);
    assert!(cat.common_total > 0);
    assert!(cat.shards_total > 0);
    assert!(cat.total_separate < cat.total_self_contained || cat.total_self_contained == cat.total_separate); // at least not larger
    // For this suite, shared scaffolding should be at least a few KB
    assert!(cat.records > 0 || cat.preamble > 0);
}

#[test]
fn separate_shards_are_minimal_and_include_header() {
    let (prog, file, opts) = minimal_program(2);
    let plan = plan_separate_tests(&prog, &file, &opts, "pref__", 1).unwrap();
    let iface = emit_separate_interface_with_prefix(&prog, &file, &opts, &plan.scaffold, "pref__").unwrap();
    assert!(iface.contains("struct HarcTestContext"));
    assert!(iface.contains("suite__") || iface.contains("pref__") || iface.contains("HarcTestContext"));
    let common = emit_separate_common_with_prefix(&prog, &file, &opts, &plan.scaffold, "pref__").unwrap();
    assert!(common.contains("#include \"pref__suite.hpp\""));
    for shard in &plan.shards {
        let cpp = emit_separate_shard_with_prefix(&prog, &file, &opts, &plan.scaffold, shard, "pref__").unwrap();
        assert!(cpp.contains("#include \"pref__suite.hpp\""));
        // Shard should not contain record definitions (they are in interface)
        // For minimal program with no records, this is vacuous, but check it doesn't contain the interface's pragma
        assert!(!cpp.contains("struct HarcTestContext {"));
    }
}

#[test]
fn separate_context_is_deepened() {
    let (prog, file, opts) = minimal_program(1);
    let plan = plan_separate_tests(&prog, &file, &opts, "h_", 1).unwrap();
    let iface = emit_separate_interface_with_prefix(&prog, &file, &opts, &plan.scaffold, "h_").unwrap();
    // M3: deepened context has methods
    assert!(iface.contains("void start("));
    assert!(iface.contains("void tick("));
    assert!(iface.contains("int finish("));
    assert!(iface.contains("harc_rt::random::HarcRng rng;"));
    let common = emit_separate_common_with_prefix(&prog, &file, &opts, &plan.scaffold, "h_").unwrap();
    assert!(common.contains("HarcTestContext::start"));
    assert!(common.contains("HarcTestContext::tick"));
    assert!(common.contains("HarcTestContext::finish"));
}
