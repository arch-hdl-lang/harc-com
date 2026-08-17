//! THROWAWAY probe test 3 — delete before finishing.

use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{lower, verify};
use harc::parser::parse_source;

fn merged_src(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("fixture parses");
    merge::merge_for_sim(vec![parsed], None).expect("merge")
}

fn report(label: &str, src: &str) {
    match parse_source(src) {
        Err(e) => {
            println!("{label}: PARSE ERROR {e:?}");
            return;
        }
        Ok(_) => {}
    }
    let merged = merged_src(src);
    let v1 = match cpp_tb::emit(&merged) {
        Ok(_) => "v1=OK".to_string(),
        Err(e) => format!("v1=ERR({})", format!("{e}").replace('\n', " | ")),
    };
    let tb = match lower::lower_program(&merged) {
        Err(e) => format!("tbir_lower=ERR({})", format!("{e}").replace('\n', " | ")),
        Ok(prog) => match verify::verify_program(&prog) {
            Err(e) => format!("tbir_verify=ERR({e:?})"),
            Ok(()) => match tbir::emit(&prog, &merged, &cpp_tb::EmitOpts::default()) {
                Ok(_) => "tbir=EMITS".to_string(),
                Err(e) => format!("tbir_emit=ERR({})", format!("{e}").replace('\n', " | ")),
            },
        },
    };
    println!("{label}\n    {v1}\n    {tb}");
}

/// Transaction-level `keep` (not `randomize ... with`).
#[test]
fn probe_txn_keep() {
    let src = |keep: &str, fields: &str| {
        format!(
            "domain D\n  freq_mhz: 100\nend domain D\n\n\
             transaction P\n{fields}    keep {keep}\nend transaction P\n\n\
             test T\n    let dut : Top\n    let p : P\n    clock clk = D\n    run\n\
             \x20       randomize(p)\n    end run\nend test T\n"
        )
    };
    for keep in ["sum(n) == 1", "n == 1", "nosuchfn(n) == 1"] {
        report(
            &format!("[keep, scalar txn] {keep}"),
            &src(keep, "    n : uint<8>\n"),
        );
    }
}

/// Fixed `Vec<T, N>` fields: TB-IR lowers them, v1 does NOT treat them
/// as lists.
#[test]
fn probe_vec_field() {
    let src = |clause: &str| {
        format!(
            "domain D\n  freq_mhz: 100\nend domain D\n\n\
             transaction P\n    n   : uint<8>\n    arr : Vec<uint<8>, 4>\n\
             end transaction P\n\n\
             test T\n    let dut : Top\n    let p : P\n    clock clk = D\n    run\n\
             \x20       randomize(p) with\n            {clause}\n        end randomize\n\
             \x20   end run\nend test T\n"
        )
    };
    for clause in [
        "sum(arr[0 .. 4]) == 100",
        "sum(arr) == 100",
        "arr[0] == 1",
        "sum(p.n) == 1",
    ] {
        report(&format!("[Vec txn] {clause}"), &src(clause));
    }
}

/// Is the list-field gate reachable-around? A list transaction that is
/// declared but never instantiated / never randomized.
#[test]
fn probe_unused_list_txn() {
    let src = format!(
        "domain D\n  freq_mhz: 100\nend domain D\n\n\
         transaction Q\n    items : list<uint<8>>\nend transaction Q\n\n\
         transaction P\n    n : uint<8>\nend transaction P\n\n\
         test T\n    let dut : Top\n    let p : P\n    clock clk = D\n    run\n\
         \x20       randomize(p) with\n            sum(p.n) == 1\n        end randomize\n\
         \x20   end run\nend test T\n"
    );
    report("[unused list txn Q, randomize P with sum(p.n)]", &src);
}

/// `sum` reached through a RELATION body (the third call site).
#[test]
fn probe_via_relation() {
    let src = |clause: &str| {
        format!(
            "domain D\n  freq_mhz: 100\nend domain D\n\n\
             transaction P\n    n : uint<8>\n    m : uint<8>\nend transaction P\n\n\
             relation R(a)\n    {clause}\nend relation R\n\n\
             test T\n    let dut : Top\n    let p : P\n    clock clk = D\n    run\n\
             \x20       randomize(p) with\n            R(p.n)\n        end randomize\n\
             \x20   end run\nend test T\n"
        )
    };
    for clause in ["sum(a) == 1", "a == 1", "nosuchfn(a) == 1"] {
        report(&format!("[relation body] {clause}"), &src(clause));
    }
}
