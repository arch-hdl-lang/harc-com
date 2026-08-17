//! THROWAWAY probe test — delete before finishing.

use harc::codegen::{cpp_tb, merge};
use harc::ir::lower;
use harc::parser::parse_source;

fn merged_src(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("fixture parses");
    merge::merge_for_sim(vec![parsed], None).expect("merge")
}

fn lower_src(src: &str) -> Result<harc::ir::TbProgram, lower::LowerError> {
    lower::lower_program(&merged_src(src))
}

fn src_scalar(clause: &str) -> String {
    format!(
        "domain D\n  freq_mhz: 100\nend domain D\n\n\
         transaction P\n    n     : uint<8>\n    m     : uint<8>\n\
         end transaction P\n\n\
         test T\n    let dut : Top\n    let p : P\n    clock clk = D\n    run\n\
         \x20       randomize(p) with\n            {clause}\n        end randomize\n\
         \x20   end run\nend test T\n"
    )
}

fn src_list(clause: &str) -> String {
    format!(
        "domain D\n  freq_mhz: 100\nend domain D\n\n\
         transaction P\n    n     : uint<8>\n    items : list<uint<8>>\n\
         end transaction P\n\n\
         test T\n    let dut : Top\n    let p : P\n    clock clk = D\n    run\n\
         \x20       randomize(p) with\n            items.len() <= 4\n            {clause}\n        end randomize\n\
         \x20   end run\nend test T\n"
    )
}

fn report(label: &str, src: &str) {
    let parsed = parse_source(src);
    if parsed.is_err() {
        println!("{label}: PARSE ERROR {:?}", parsed.err());
        return;
    }
    let v1 = cpp_tb::emit(&merged_src(src));
    let v1s = match &v1 {
        Ok(_) => "v1=OK".to_string(),
        Err(e) => format!("v1=ERR({})", format!("{e}").replace('\n', " | ")),
    };
    let tb = lower_src(src);
    let tbs = match &tb {
        Ok(_) => "tbir=LOWERS".to_string(),
        Err(e) => format!("tbir=ERR({})", format!("{e}").replace('\n', " | ")),
    };
    println!("{label}\n    {v1s}\n    {tbs}");
}

#[test]
fn probe_b_and_d_scalar_only() {
    for clause in [
        "sum(p.n) == 1",
        "sum(p.n)",
        "sum(n) == 1",
        "p.n == 1",
        "nosuchfn(p.n) == 1",
        "sum(p.n, p.m) == 1",
    ] {
        report(&format!("[scalar txn] {clause}"), &src_scalar(clause));
    }
}

#[test]
fn probe_b_list_arg_shapes() {
    for clause in [
        "sum(items[0 .. items.len()]) == 100",
        "sum(items[0]) == 100",
        "sum(items) == 100",
        "sum(p.items) == 100",
        "sum(items[0 .. 2]) == 100",
        "sum(p.n) == 100",
    ] {
        report(&format!("[list txn] {clause}"), &src_list(clause));
    }
}

#[test]
fn probe_a_other_builtin_names() {
    for name in [
        "sum", "len", "size", "count", "min", "max", "abs", "unique", "product", "sums",
    ] {
        for clause in [
            format!("{name}(items[0 .. items.len()]) == 1"),
            format!("{name}(items) == 1"),
            format!("{name}(p.n) == 1"),
        ] {
            report(&format!("[list txn] {clause}"), &src_list(&clause));
        }
    }
}
