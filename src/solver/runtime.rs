//! Runtime-facing typed problem descriptors.
//!
//! Phase 5A keeps generated C++ behavior on the existing `cpp_tb.rs` inline
//! Z3 path. This module gives future codegen a stable descriptor shape and
//! deterministic problem IDs to hand to `runtime/harc_random_rt.*`.

use crate::constraints::typed::{CType, CTypedProblem, FieldPath, Sign};
use crate::solver::problem_table::{TypedSolverProblemBuild, TypedSolverProblemTable};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProblemTable {
    pub problems: Vec<RuntimeProblemDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProblemDescriptor {
    pub id: u32,
    pub origin: String,
    pub fields: Vec<RuntimeFieldDescriptor>,
    pub constraints: Vec<RuntimeConstraintDescriptor>,
    pub solve_order: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFieldDescriptor {
    pub path: String,
    pub ty: RuntimeTypeDescriptor,
    pub non_random: bool,
    pub has_default: bool,
    pub attrs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTypeDescriptor {
    BitVector {
        width: u32,
        signed: bool,
    },
    Bool,
    Enum {
        name: String,
        variants: Vec<String>,
    },
    List {
        elem: Box<RuntimeTypeDescriptor>,
        max_len: Option<usize>,
    },
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConstraintDescriptor {
    pub assertion_name: String,
    pub origin: String,
    pub expr: String,
}

impl RuntimeProblemTable {
    pub fn from_typed_solver_table(table: &TypedSolverProblemTable) -> Self {
        let problems = table
            .entries
            .iter()
            .filter_map(|entry| match &entry.build {
                TypedSolverProblemBuild::Z3 { typed, .. } => {
                    Some(RuntimeProblemDescriptor::from_typed_problem(typed))
                }
                TypedSolverProblemBuild::LowerError(_)
                | TypedSolverProblemBuild::BackendError(_) => None,
            })
            .collect();
        Self { problems }
    }

    pub fn manifest(&self) -> String {
        let mut out = String::new();
        for problem in &self.problems {
            out.push_str(&problem.manifest());
        }
        out
    }

    pub fn render_cpp_table(&self, symbol: &str) -> String {
        let mut out = String::new();
        out.push_str("// HARC typed runtime randomization problem table (Phase 5B scaffold).\n");
        out.push_str(
            "// Current randomization behavior still uses cpp_tb.rs inline Z3 emission.\n",
        );
        out.push_str("namespace {\n");
        out.push_str("static constexpr harc_rt::random::HarcRuntimeProblemDescriptor ");
        out.push_str(symbol);
        out.push_str("_entries[] = {\n");
        for problem in &self.problems {
            out.push_str("    {");
            out.push_str(&problem.id.to_string());
            out.push_str(", \"");
            out.push_str(&escape_cpp_string(&problem.origin));
            out.push_str("\", \"");
            out.push_str(&escape_cpp_string(&problem.manifest()));
            out.push_str("\"},\n");
        }
        out.push_str("};\n");
        out.push_str("static constexpr harc_rt::random::HarcRuntimeProblemTable ");
        out.push_str(symbol);
        out.push_str(" = {");
        out.push_str(symbol);
        out.push_str("_entries, ");
        out.push_str(&self.problems.len().to_string());
        out.push_str("};\n");
        out.push_str("static harc_rt::random::HarcRuntimeCallSite ");
        out.push_str(symbol);
        out.push_str("_call_sites[] = {\n");
        for problem in &self.problems {
            out.push_str("    {");
            out.push_str(&problem.id.to_string());
            out.push_str(", ");
            out.push_str(&problem.id.to_string());
            out.push_str(", 0},\n");
        }
        out.push_str("};\n");
        out.push_str("static constexpr uint32_t ");
        out.push_str(symbol);
        out.push_str("_call_site_count = ");
        out.push_str(&self.problems.len().to_string());
        out.push_str(";\n");
        out.push_str("static inline harc_rt::random::HarcRandomizeCall ");
        out.push_str(symbol);
        out.push_str("_prepare_call(\n");
        out.push_str("    harc_rt::random::harc_problem_id problem_id,\n");
        out.push_str("    harc_rt::random::harc_seed global_seed,\n");
        out.push_str("    harc_rt::random::harc_seed fallback_seed) {\n");
        out.push_str("    return harc_rt::random::harc_prepare_randomize_call(\n");
        out.push_str("        ");
        out.push_str(symbol);
        out.push_str(",\n");
        out.push_str("        ");
        out.push_str(symbol);
        out.push_str("_call_sites,\n");
        out.push_str("        ");
        out.push_str(symbol);
        out.push_str("_call_site_count,\n");
        out.push_str("        problem_id,\n");
        out.push_str("        global_seed,\n");
        out.push_str("        fallback_seed);\n");
        out.push_str("}\n");
        out.push_str("} // namespace\n\n");
        out
    }
}

impl RuntimeProblemDescriptor {
    pub fn from_typed_problem(problem: &CTypedProblem) -> Self {
        let mut fields = Vec::new();
        for (path, info) in &problem.env.fields {
            let attrs = info
                .attrs
                .iter()
                .map(|attr| attr.name.clone())
                .collect::<Vec<_>>();
            fields.push(RuntimeFieldDescriptor {
                path: path.dotted(),
                ty: RuntimeTypeDescriptor::from_ctype(&info.ty, problem),
                non_random: info.non_random,
                has_default: info.has_default,
                attrs,
            });
        }

        let constraints = problem
            .constraints
            .iter()
            .map(|clause| RuntimeConstraintDescriptor {
                assertion_name: clause.assertion_name.clone(),
                origin: format!("{:?}", clause.origin),
                expr: clause.expr.to_string(),
            })
            .collect();

        let solve_order = problem
            .solve_order
            .as_ref()
            .map(|order| order.iter().map(FieldPath::dotted).collect())
            .unwrap_or_default();

        Self {
            id: problem.problem_id.0,
            origin: problem.origin.to_string(),
            fields,
            constraints,
            solve_order,
        }
    }

    pub fn manifest(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("problem {} {}\n", self.id, self.origin));
        for field in &self.fields {
            out.push_str(&format!(
                "  field {} {} random={} default={} attrs=[{}]\n",
                field.path,
                field.ty.manifest(),
                !field.non_random,
                field.has_default,
                field.attrs.join(",")
            ));
        }
        for constraint in &self.constraints {
            out.push_str(&format!(
                "  constraint {} {} {}\n",
                constraint.assertion_name, constraint.origin, constraint.expr
            ));
        }
        if !self.solve_order.is_empty() {
            out.push_str(&format!("  solve_order [{}]\n", self.solve_order.join(",")));
        }
        out
    }
}

fn escape_cpp_string(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

impl RuntimeTypeDescriptor {
    fn from_ctype(ty: &CType, problem: &CTypedProblem) -> Self {
        match ty {
            CType::BV { width, sign } => RuntimeTypeDescriptor::BitVector {
                width: *width,
                signed: *sign == Sign::Signed,
            },
            CType::Bool => RuntimeTypeDescriptor::Bool,
            CType::Enum { domain } => match problem.env.enum_by_id(*domain) {
                Some(entry) => RuntimeTypeDescriptor::Enum {
                    name: entry.name.clone(),
                    variants: entry
                        .variants
                        .iter()
                        .map(|variant| variant.variant.clone())
                        .collect(),
                },
                None => RuntimeTypeDescriptor::Unsupported(format!("missing enum#{}", domain.0)),
            },
            CType::List { elem, max_len } => RuntimeTypeDescriptor::List {
                elem: Box::new(RuntimeTypeDescriptor::from_ctype(elem, problem)),
                max_len: *max_len,
            },
            CType::Range { .. } | CType::Set { .. } | CType::Bottom => {
                RuntimeTypeDescriptor::Unsupported(ty.to_string())
            }
        }
    }

    fn manifest(&self) -> String {
        match self {
            RuntimeTypeDescriptor::BitVector { width, signed } => {
                format!("{}{}", if *signed { "s" } else { "u" }, width)
            }
            RuntimeTypeDescriptor::Bool => "bool".to_string(),
            RuntimeTypeDescriptor::Enum { name, variants } => {
                format!("enum {} {{{}}}", name, variants.join(","))
            }
            RuntimeTypeDescriptor::List { elem, max_len } => match max_len {
                Some(max_len) => format!("list<{}, max_len={}>", elem.manifest(), max_len),
                None => format!("list<{}>", elem.manifest()),
            },
            RuntimeTypeDescriptor::Unsupported(detail) => format!("unsupported<{detail}>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_source;
    use crate::solver::problem_table::build_typed_solver_problem_table;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn runtime_descriptor_manifest_is_stable() {
        let src = r#"
enum Op { READ, WRITE }

transaction Packet
    op : Op
    addr : uint<8> with [range(1, 12)]
    !tag : bits<4>
    keep addr != 7
end transaction Packet

test Smoke
    run
        let p : Packet
        randomize(p) with
            p.op == READ
        end randomize
    end run
end test Smoke
"#;
        let parsed = parse_source(src).expect("parse");
        let typed_table = build_typed_solver_problem_table(&parsed);
        let runtime_table = RuntimeProblemTable::from_typed_solver_table(&typed_table);

        assert_eq!(runtime_table.problems.len(), 2);
        assert_eq!(runtime_table.problems[0].id, 1);
        assert_eq!(runtime_table.problems[1].id, 2);

        let manifest = runtime_table.manifest();
        assert!(manifest.contains("problem 1 randomize(Packet)"));
        assert!(manifest.contains("problem 2 randomize(Packet) with"));
        assert!(manifest.contains("field addr u8 random=true default=false attrs=[range]"));
        assert!(manifest.contains("field op enum Op {READ,WRITE}"));
        assert!(manifest.contains("field tag u4 random=false"));
    }

    #[test]
    fn renders_cpp_problem_table_metadata() {
        let src = r#"
transaction Packet
    addr : uint<8>
    keep addr != 7
end transaction Packet

test Smoke
    run
        let p : Packet
        randomize(p) with
            p.addr != 9
        end randomize
    end run
end test Smoke
"#;
        let parsed = parse_source(src).expect("parse");
        let typed_table = build_typed_solver_problem_table(&parsed);
        let runtime_table = RuntimeProblemTable::from_typed_solver_table(&typed_table);
        let cpp = runtime_table.render_cpp_table("_harc_runtime_problem_table");

        assert!(cpp.contains("HarcRuntimeProblemDescriptor _harc_runtime_problem_table_entries[]"));
        assert!(cpp.contains("HarcRuntimeProblemTable _harc_runtime_problem_table"));
        assert!(cpp.contains("HarcRuntimeCallSite _harc_runtime_problem_table_call_sites[]"));
        assert!(cpp.contains("_harc_runtime_problem_table_call_site_count = 2"));
        assert!(cpp.contains("{1, \"randomize(Packet)\""));
        assert!(cpp.contains("{2, \"randomize(Packet) with\""));
        assert!(cpp.contains("{1, 1, 0}"));
        assert!(cpp.contains("{2, 2, 0}"));
        assert!(cpp.contains("problem 1 randomize(Packet)\\n"));
    }

    #[test]
    fn random_runtime_scaffold_compiles_as_cxx20() {
        let cxx = std::env::var("HARC_CXX").unwrap_or_else(|_| "c++".to_string());
        let out = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("tmp")
            .join("harc_random_rt_test.o");
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).expect("create target/tmp");
        }

        let status = match Command::new(&cxx)
            .args([
                "-std=c++20",
                "-Iruntime",
                "-c",
                "runtime/harc_random_rt.cpp",
                "-o",
            ])
            .arg(&out)
            .status()
        {
            Ok(status) => status,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping C++ runtime compile check: `{cxx}` not found");
                return;
            }
            Err(err) => panic!("failed to launch `{cxx}`: {err}"),
        };
        assert!(
            status.success(),
            "`{cxx}` failed to compile harc_random_rt.cpp"
        );
    }

    #[test]
    fn random_runtime_lookup_and_callsite_helpers_compile() {
        let cxx = std::env::var("HARC_CXX").unwrap_or_else(|_| "c++".to_string());
        let tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("tmp");
        std::fs::create_dir_all(&tmp).expect("create target/tmp");
        let src = tmp.join("harc_random_rt_lookup_test.cpp");
        let out = tmp.join("harc_random_rt_lookup_test.o");
        std::fs::write(
            &src,
            r#"#include "harc_random_rt.h"

using namespace harc_rt::random;

static constexpr HarcRuntimeProblemDescriptor problems[] = {
    {1, "randomize(Packet)", "problem 1"},
    {2, "randomize(Packet) with", "problem 2"},
};
static constexpr HarcRuntimeProblemTable table = {problems, 2};
static_assert(harc_find_problem(table, 1)->id == 1);
static_assert(harc_find_problem(table, 3) == nullptr);

int main() {
    struct Packet { int value = 0; };
    auto randomize_packet = [](Packet* packet) { packet->value = 7; };
    Packet packet;
    HarcRuntimeCallSite site{7, 2, 0};
    HarcRuntimeCallSite sites[] = {{7, 2, 0}, {8, 4, 0}};
    HarcRuntimeCallSite* found = harc_find_call_site(sites, 2, 4);
    harc_seed a = harc_call_site_next_seed(site, 11);
    harc_seed b = harc_call_site_next_seed(site, 11);
    HarcRandomizeCall call = harc_prepare_randomize_call(table, sites, 2, 4, 11, 99);
    uint64_t pref_u = harc_prefer_uint(a, 0, 6);
    int64_t pref_s = harc_prefer_sint(a, 1, 4);
    int64_t pref_d = harc_prefer_dist(a, 2, {{1, 2, 1}, {7, 9, 3}});
    HarcUniqueHistory<int> unique;
    harc_unique_remember(unique, 5);
    bool unique_has_value = false;
    for (int value : harc_unique_values(unique)) unique_has_value = unique_has_value || value == 5;
    harc_unique_clear(unique);
    HarcAutoCovSelection auto_cov;
    harc_auto_cov_select_cross(auto_cov, 3, 1, 2);
    bool cov_hit[2] = {true, false};
    bool cov_blocked[2] = {false, false};
    size_t cov_i = 0;
    bool found_cov_point = harc_auto_cov_first_uncovered(cov_hit, cov_blocked, cov_i);
    bool cross_hit[2][2] = {{true, false}, {false, false}};
    bool cross_blocked[2][2] = {{false, false}, {true, false}};
    size_t cross_i = 0;
    size_t cross_j = 0;
    bool found_cov_cross = harc_auto_cov_first_uncovered_cross(cross_hit, cross_blocked, cross_i, cross_j);
    uint64_t cov_hits = harc_auto_cov_count(cov_hit);
    uint64_t cross_blocked_count = harc_auto_cov_count(cross_blocked);
    const char* cov_state = harc_auto_cov_state(cov_hit[1], cov_blocked[1]);
    bool hit = false;
    bool blocked = false;
    harc_auto_cov_mark_blocked(blocked);
    harc_auto_cov_mark_hit(hit, blocked);
    bool selected_blocked = false;
    harc_auto_cov_mark_selected_cross_blocked(auto_cov, 3, selected_blocked);
    bool value_hit = false;
    bool value_blocked = true;
    harc_auto_cov_mark_value_hit(7, 7, value_hit, value_blocked);
    bool cross_hit_match = false;
    bool cross_hit_blocked = true;
    harc_auto_cov_mark_cross_hit(1, 1, 2, 2, cross_hit_match, cross_hit_blocked);
    HarcSolverRetryPolicy retry;
    bool retry_pref = harc_retry_without_preferences(retry, false);
    bool retry_unique = harc_retry_without_unique_history(retry, false);
    HarcSolveStatus status = harc_solve_queued(packet, 4, a, randomize_packet);
    HarcSolveStatus constrained = harc_solve_constrained(
        packet,
        4,
        a,
        HarcSolveMode::Queued,
        []() { return harc_solve_status_ok(); });
    HarcSolveStatus unsat = harc_solve_status_unsat(4, b);
    bool handled_ok = harc_handle_solve_status(constrained);
    bool handled_unsat = harc_handle_solve_status(unsat);
    return (status.ok && handled_ok && !handled_unsat && constrained.ok && !unsat.ok && unsat.problem_id == 4 && unsat.seed == b && call.problem_id == 4 && call.problem && call.seed != 99 && pref_u < 64 && pref_s >= -8 && pref_s <= 7 && pref_d >= 1 && pref_d <= 9 && unique_has_value && unique.empty() && harc_auto_cov_has_preference(auto_cov) && harc_auto_cov_selected_cross(auto_cov, 3) && found_cov_point && cov_i == 1 && found_cov_cross && cross_i == 0 && cross_j == 1 && cov_hits == 1 && cross_blocked_count == 1 && cov_state[0] == '*' && hit && !blocked && selected_blocked && value_hit && !value_blocked && cross_hit_match && !cross_hit_blocked && retry_pref && retry_unique && retry.retried_without_preferences && retry.retried_without_unique_history && packet.value == 7 && found && found->site_id == 8 && site.iteration == 2 && a != b && site.problem_id == 2) ? 0 : 1;
}
"#,
        )
        .expect("write lookup test");

        let status = match Command::new(&cxx)
            .args(["-std=c++20", "-Iruntime", "-c"])
            .arg(&src)
            .arg("-o")
            .arg(&out)
            .status()
        {
            Ok(status) => status,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping C++ runtime lookup compile check: `{cxx}` not found");
                return;
            }
            Err(err) => panic!("failed to launch `{cxx}`: {err}"),
        };
        assert!(
            status.success(),
            "`{cxx}` failed to compile runtime lookup/callsite helper test"
        );
    }
}
