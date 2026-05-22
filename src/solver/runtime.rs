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
}
