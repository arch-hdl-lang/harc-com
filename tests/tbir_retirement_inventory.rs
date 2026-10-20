use proc_macro2::{Span, TokenStream, TokenTree};
use std::fs;
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{ExprPath, ExprStruct, ItemUse, Macro, Pat, UseTree};

#[derive(Default)]
struct RetirementUseVisitor {
    path: PathBuf,
    violations: Vec<String>,
}

impl RetirementUseVisitor {
    fn record(&mut self, span: Span, what: &str) {
        let start = span.start();
        self.violations.push(format!(
            "{}:{}:{} constructs {what}",
            self.path.display(),
            start.line,
            start.column + 1
        ));
    }
}

fn forbidden_variant(name: &str) -> bool {
    matches!(name, "Unsupported" | "WorksButRetired" | "Unreliable")
}

fn terminal(path: &syn::Path) -> Option<String> {
    path.segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn forbidden_macro_token(tokens: TokenStream) -> Option<Span> {
    for token in tokens {
        match token {
            TokenTree::Ident(ident) if forbidden_variant(&ident.to_string()) => {
                return Some(ident.span());
            }
            TokenTree::Group(group) => {
                if let Some(span) = forbidden_macro_token(group.stream()) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

fn inspect_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    violations: &mut Vec<(Span, String)>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            inspect_use_tree(&path.tree, prefix, violations);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let ident = name.ident.to_string();
            if forbidden_variant(&ident) {
                violations.push((name.ident.span(), ident));
            }
        }
        UseTree::Rename(rename) => {
            let ident = rename.ident.to_string();
            if forbidden_variant(&ident) {
                violations.push((rename.ident.span(), ident));
            }
        }
        UseTree::Glob(glob) => {
            if prefix.last().is_some_and(|name| name == "V1Status") {
                violations.push((glob.star_token.span(), "V1Status::*".into()));
            }
            if prefix.last().is_some_and(|name| name == "LowerError") {
                violations.push((glob.star_token.span(), "LowerError::*".into()));
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                inspect_use_tree(item, prefix, violations);
            }
        }
    }
}

impl<'ast> Visit<'ast> for RetirementUseVisitor {
    fn visit_pat(&mut self, node: &'ast Pat) {
        // Unit enum variants in match patterns are compatibility/display
        // handling, not production construction of a retired status.
        if matches!(node, Pat::Path(_)) {
            return;
        }
        visit::visit_pat(self, node);
    }

    fn visit_expr_struct(&mut self, node: &'ast ExprStruct) {
        if terminal(&node.path).is_some_and(|name| name == "Unsupported") {
            self.record(node.path.span(), "LowerError::Unsupported");
        }
        visit::visit_expr_struct(self, node);
    }

    fn visit_expr_path(&mut self, node: &'ast ExprPath) {
        for status in ["WorksButRetired", "Unreliable"] {
            if terminal(&node.path).is_some_and(|name| name == status) {
                self.record(node.path.span(), &format!("V1Status::{status}"));
            }
        }
        visit::visit_expr_path(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        let mut violations = Vec::new();
        inspect_use_tree(&node.tree, &mut Vec::new(), &mut violations);
        for (span, name) in violations {
            self.record(span, &format!("import of retired variant {name}"));
        }
    }

    fn visit_macro(&mut self, node: &'ast Macro) {
        if let Some(span) = forbidden_macro_token(node.tokens.clone()) {
            self.record(span, "a retired variant inside a macro token tree");
        }
        visit::visit_macro(self, node);
    }
}

fn rust_sources(root: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).unwrap_or_else(|error| {
        panic!("failed to read {}: {error}", root.display());
    }) {
        let path = entry.expect("lowering directory entry").path();
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn violations_in(source: &str) -> Vec<String> {
    let syntax = syn::parse_file(source).expect("test source parses");
    let mut visitor = RetirementUseVisitor::default();
    visitor.path = PathBuf::from("probe.rs");
    visitor.visit_file(&syntax);
    visitor.violations
}

#[test]
fn inventory_is_source_aware_and_rejects_constructor_expressions() {
    let harmless = r#"
        // V1Status::Unreliable and LowerError::Unsupported { fake: true }
        const NOTE: &str = "V1Status::WorksButRetired";
        fn describe(status: V1Status, error: LowerError) {
            match status {
                V1Status::WorksButRetired | V1Status::Unreliable => {}
                _ => {}
            }
            if let LowerError::Unsupported { .. } = error {}
        }
    "#;
    assert!(violations_in(harmless).is_empty());

    let constructors = r#"
        fn retired_status() -> V1Status { V1Status::Unreliable }
        fn unsupported() -> LowerError {
            LowerError::Unsupported { construct: String::new(), detail: String::new() }
        }
    "#;
    let violations = violations_in(constructors);
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(violations
        .iter()
        .any(|item| item.contains("V1Status::Unreliable")));
    assert!(violations
        .iter()
        .any(|item| item.contains("LowerError::Unsupported")));

    let aliases_and_macros = r#"
        use crate::lower::V1Status::WorksButRetired as Retired;
        use crate::lower::LowerError::Unsupported;
        fn retired_status() -> V1Status { Retired }
        fn unsupported() -> LowerError {
            Unsupported { construct: String::new(), detail: String::new() }
        }
        macro_rules! retired { () => { V1Status::Unreliable } }
    "#;
    let violations = violations_in(aliases_and_macros);
    assert!(violations.len() >= 3, "{violations:?}");
    assert!(violations
        .iter()
        .any(|item| item.contains("import of retired variant WorksButRetired")));
    assert!(violations
        .iter()
        .any(|item| item.contains("import of retired variant Unsupported")));
    assert!(violations
        .iter()
        .any(|item| item.contains("macro token tree")));
}

#[test]
fn lowering_source_does_not_construct_retired_gap_statuses() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ir/lower");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    files.sort();

    let mut visitor = RetirementUseVisitor::default();
    for path in files {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let syntax = syn::parse_file(&source)
            .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
        visitor.path = path;
        visitor.visit_file(&syntax);
    }

    assert!(
        visitor.violations.is_empty(),
        "TB-IR retirement-only constructors returned:\n{}",
        visitor.violations.join("\n")
    );
}
