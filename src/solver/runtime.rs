//! Runtime-facing typed problem descriptors.
//!
//! Generated C++ uses these immutable descriptors together with explicit
//! per-run state in `runtime/harc_random_rt.*`.

use crate::constraints::typed::{CType, CTypedProblem, FieldPath, Sign};
use crate::solver::problem_table::{TypedSolverProblemBuild, TypedSolverProblemTable};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProblemTable {
    pub problems: Vec<RuntimeProblemDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProblemDescriptor {
    pub id: u64,
    pub origin: String,
    pub fields: Vec<RuntimeFieldDescriptor>,
    pub constraints: Vec<RuntimeConstraintDescriptor>,
    pub soft_constraints: Vec<RuntimeSoftConstraintDescriptor>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSoftConstraintDescriptor {
    pub assertion_name: String,
    pub origin: String,
    pub expr: String,
    pub weight: u32,
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

    /// Render the immutable descriptor half of the runtime problem table.
    /// Mutable call-site iterations belong to a concrete simulation run and
    /// are deliberately omitted here.
    pub fn render_cpp_inline_descriptors(&self, symbol: &str) -> String {
        self.render_cpp_descriptors(symbol, "inline constexpr")
    }

    pub fn render_cpp_private_descriptors(&self, symbol: &str) -> String {
        self.render_cpp_descriptors(symbol, "static constexpr")
    }

    fn render_cpp_descriptors(&self, symbol: &str, storage: &str) -> String {
        let mut out = String::new();
        out.push_str("// HARC typed runtime randomization problem descriptors.\n");
        out.push_str(storage);
        out.push_str(" harc_rt::random::HarcRuntimeProblemDescriptor ");
        out.push_str(symbol);
        out.push_str("_entries[] = {\n");
        for problem in &self.problems {
            out.push_str("    {");
            out.push_str(&problem.id.to_string());
            out.push_str("ULL, \"");
            out.push_str(&escape_cpp_string(&problem.origin));
            out.push_str("\", \"");
            out.push_str(&escape_cpp_string(&problem.manifest()));
            out.push_str("\"},\n");
        }
        out.push_str("};\n");
        out.push_str(storage);
        out.push_str(" harc_rt::random::HarcRuntimeProblemTable ");
        out.push_str(symbol);
        out.push_str(" = {");
        out.push_str(symbol);
        out.push_str("_entries, ");
        out.push_str(&self.problems.len().to_string());
        out.push_str("};\n\n");
        out
    }

    pub fn problem_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.problems.iter().map(|problem| problem.id)
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

        let soft_constraints = problem
            .soft_constraints
            .iter()
            .map(|clause| RuntimeSoftConstraintDescriptor {
                assertion_name: clause.assertion_name.clone(),
                origin: format!("{:?}", clause.origin),
                expr: clause.expr.to_string(),
                weight: clause.weight,
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
            soft_constraints,
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
        for constraint in &self.soft_constraints {
            out.push_str(&format!(
                "  soft_constraint {} weight={} {} {}\n",
                constraint.assertion_name, constraint.weight, constraint.origin, constraint.expr
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
        let site_id = runtime_table.problems[1].id;
        assert_ne!(site_id, 1);

        let manifest = runtime_table.manifest();
        assert!(manifest.contains("problem 1 randomize(Packet)"));
        assert!(manifest.contains(&format!("problem {site_id} randomize(Packet) with")));
        assert!(manifest.contains("field addr u8 random=true default=false attrs=[range]"));
        assert!(manifest.contains("field op enum Op {READ,WRITE}"));
        assert!(manifest.contains("field tag u4 random=false"));
    }

    #[test]
    fn renders_immutable_cpp_problem_table_metadata() {
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
        let site_id = runtime_table.problems[1].id;
        let cpp = runtime_table.render_cpp_inline_descriptors("_harc_runtime_problem_table");

        assert!(cpp.contains("HarcRuntimeProblemDescriptor _harc_runtime_problem_table_entries[]"));
        assert!(cpp.contains("HarcRuntimeProblemTable _harc_runtime_problem_table"));
        assert!(cpp.contains("{1ULL, \"randomize(Packet)\""));
        assert!(cpp.contains(&format!("{{{site_id}ULL, \"randomize(Packet) with\"")));
        assert!(!cpp.contains("HarcRuntimeCallSite"));
        assert!(cpp.contains("problem 1 randomize(Packet)\\n"));

        let cxx = std::env::var("HARC_CXX").unwrap_or_else(|_| "c++".to_string());
        let tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("tmp");
        std::fs::create_dir_all(&tmp).expect("create target/tmp");
        let source = tmp.join("harc_random_rt_descriptor_test.cpp");
        let object = tmp.join("harc_random_rt_descriptor_test.o");
        std::fs::write(
            &source,
            format!("#include \"harc_random_rt.h\"\n{cpp}\nint main() {{ return 0; }}\n"),
        )
        .expect("write descriptor compile test");
        let status = match Command::new(&cxx)
            .args([
                "-std=c++20",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-Iruntime",
                "-c",
            ])
            .arg(&source)
            .arg("-o")
            .arg(&object)
            .status()
        {
            Ok(status) => status,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping C++ descriptor compile check: `{cxx}` not found");
                return;
            }
            Err(err) => panic!("failed to launch `{cxx}`: {err}"),
        };
        assert!(
            status.success(),
            "`{cxx}` rejected generated descriptor literals"
        );
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
                "-Wall",
                "-Wextra",
                "-Werror",
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
    uint64_t direct_rng_state = 123;
    uint64_t direct_rng_draw = harc_rng_next_state(direct_rng_state);
    HarcRandomizeCall call = harc_prepare_randomize_call(table, sites, 2, 4, 11, 99);
    uint64_t pref_u = harc_prefer_uint(a, 0, 6);
    _harc_u128 pref_u128 = harc_prefer_u128(a, 3, 96);
    harc_rt::HarcWide<5> pref_wide = harc_prefer_wide<5>(a, 4, 132);
    int64_t pref_s = harc_prefer_sint(a, 1, 4);
    int64_t pref_d = harc_prefer_dist(a, 2, {{1, 2, 1}, {7, 9, 3}});
    uint64_t rng_state = 123;
    auto next_draw = [&]() { return harc_splitmix64(rng_state++); };
    int64_t rng_range = harc_rng_range(next_draw, 3, 7);
    uint64_t rng_uint = harc_rng_uint(next_draw, 5);
    _harc_u128 rng_u128 = harc_rng_u128(next_draw, 96);
    harc_rt::HarcWide<5> rng_wide = harc_rng_wide<5>(next_draw, 132);
    int64_t rng_dist = harc_rng_dist(next_draw, {{1, 2, 1}, {7, 9, 3}});
    HarcUniqueHistory<int> unique;
    harc_unique_remember(unique, 5);
    bool unique_has_value = false;
    for (int value : harc_unique_values(unique)) unique_has_value = unique_has_value || value == 5;
    harc_unique_clear(unique);
    int cov_values[2] = {3, 9};
    int cross_a_values[2] = {1, 2};
    int cross_b_values[2] = {7, 8};
    bool report_registered = false;
    std::vector<std::function<void()>> reports;
    harc_auto_cov_register_report(report_registered, reports, []() {});
    bool hit = false;
    bool blocked = false;
    harc_auto_cov_mark_blocked(blocked);
    harc_auto_cov_mark_hit(hit, blocked);
    const char* point_labels[] = {"Packet.kind=Read", "Packet.kind=Write"};
    const char* cross_labels[] = {
        "Packet.kind=Read x Packet.len=1",
        "Packet.kind=Read x Packet.len=4",
        "Packet.kind=Write x Packet.len=1",
        "Packet.kind=Write x Packet.len=4",
    };
    HarcAutoCovPointMeta point_meta[] = {{point_labels, 2}};
    HarcAutoCovCrossMeta cross_meta[] = {{cross_labels, 2, 2}};
    HarcAutoCovPlan cov_plan{"Packet", 12, point_meta, 1, cross_meta, 1};
    HarcAutoCovState cov_state_table;
    HarcAutoCovSelection state_selection;
    int state_point_pref = 0;
    int state_cross_a_pref = 0;
    int state_cross_b_pref = 0;
    bool applied_state_point = harc_auto_cov_apply_point_preference(cov_plan, cov_state_table, state_selection, 0, cov_values, state_point_pref);
    harc_auto_cov_mark_selected_point_blocked(cov_plan, cov_state_table, state_selection, 0);
    HarcAutoCovSelection state_cross_selection;
    bool applied_state_cross = harc_auto_cov_apply_cross_preference(cov_plan, cov_state_table, state_cross_selection, 0, cross_a_values, cross_b_values, state_cross_a_pref, state_cross_b_pref);
    harc_auto_cov_mark_selected_cross_blocked(cov_plan, cov_state_table, state_cross_selection, 0);
    harc_auto_cov_mark_value_hit(9, 9, cov_plan, cov_state_table, 0, 1);
    harc_auto_cov_mark_cross_hit(1, 1, 8, 8, cov_plan, cov_state_table, 0, 0, 1);
    const char* cov_state = harc_auto_cov_state(cov_state_table.point_hit[1], cov_state_table.point_blocked[1]);
    harc_auto_cov_report(cov_plan, cov_state_table);
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
    return (status.ok && handled_ok && !handled_unsat && constrained.ok && !unsat.ok && unsat.problem_id == 4 && unsat.seed == b && call.problem_id == 4 && call.problem && call.seed != 99 && direct_rng_state != 123 && direct_rng_draw != 0 && pref_u < 64 && pref_u128 != 0 && (pref_u128 >> 96) == 0 && pref_wide[4] < 16 && pref_s >= -8 && pref_s <= 7 && pref_d >= 1 && pref_d <= 9 && rng_range >= 3 && rng_range <= 7 && rng_uint < 32 && rng_u128 != 0 && (rng_u128 >> 96) == 0 && rng_wide[4] < 16 && rng_dist >= 1 && rng_dist <= 9 && unique_has_value && unique.empty() && report_registered && reports.size() == 1 && hit && !blocked && applied_state_point && state_point_pref == 3 && applied_state_cross && state_cross_a_pref == 1 && state_cross_b_pref == 7 && cov_state[0] == 'h' && cov_state_table.point_blocked[0] && cov_state_table.point_hit[1] && !cov_state_table.point_blocked[1] && cov_state_table.cross_blocked[0] && cov_state_table.cross_hit[1] && !cov_state_table.cross_blocked[1] && retry_pref && retry_unique && retry.retried_without_preferences && retry.retried_without_unique_history && packet.value == 7 && found && found->site_id == 8 && site.iteration == 2 && a != b && site.problem_id == 2) ? 0 : 1;
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

    #[test]
    fn random_callsite_iterations_cover_zero_one_and_wrap() {
        let cxx = std::env::var("HARC_CXX").unwrap_or_else(|_| "c++".to_string());
        let tmp = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("tmp");
        std::fs::create_dir_all(&tmp).expect("create target/tmp");
        let src = tmp.join("harc_random_rt_iteration_test.cpp");
        let out = tmp.join("harc_random_rt_iteration_test");
        std::fs::write(
            &src,
            r#"#include "harc_random_rt.h"

using namespace harc_rt::random;

int main() {
    HarcRuntimeCallSite site{9, 1, 0};
    harc_seed zero = harc_call_site_next_seed(site, 23);
    harc_seed one = harc_call_site_next_seed(site, 23);
    site.iteration = ~uint64_t{0};
    harc_seed maximum = harc_call_site_next_seed(site, 23);
    harc_seed wrapped = harc_call_site_next_seed(site, 23);
    return zero == harc_seed_from(23, 9, 0) &&
            one == harc_seed_from(23, 9, 1) &&
            maximum == harc_seed_from(23, 9, ~uint64_t{0}) &&
            wrapped == zero && site.iteration == 1
        ? 0
        : 1;
}
"#,
        )
        .expect("write iteration test");

        let status = match Command::new(&cxx)
            .args(["-std=c++20", "-Iruntime"])
            .arg(&src)
            .arg("-o")
            .arg(&out)
            .status()
        {
            Ok(status) => status,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping C++ runtime iteration check: `{cxx}` not found");
                return;
            }
            Err(err) => panic!("failed to launch `{cxx}`: {err}"),
        };
        assert!(status.success(), "`{cxx}` failed to compile iteration test");
        let status = Command::new(&out)
            .status()
            .expect("run runtime iteration test");
        assert!(status.success(), "runtime iteration test failed");
    }
}
