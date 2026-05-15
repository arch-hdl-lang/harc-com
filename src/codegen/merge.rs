//! Cross-file extend merging.
//!
//! Given a set of parsed files where one declares `test Smoke { ... }` and
//! others declare `extend Smoke { scope sim ... }`, produce a single
//! synthetic `SourceFile` whose `Item::Test` for `Smoke` has all the
//! extension items appended. Other items (transactions, packages, etc.)
//! pass through unchanged.
//!
//! This is the depth-1 aspect rule from spec §3.6: extends always target a
//! base declaration, never another extend. Multiple extends compose by
//! appending in file/declaration order.

use crate::ast::*;

pub fn merge_for_sim(files: &[SourceFile], pick: Option<&str>) -> Result<SourceFile, String> {
    // Index test bases by name and collect extends targeting tests.
    let mut tests: std::collections::HashMap<String, TestDecl> =
        std::collections::HashMap::new();
    let mut test_extends: Vec<(String, Vec<TestItem>)> = Vec::new();
    let mut other_items: Vec<Item> = Vec::new();

    for file in files {
        for it in &file.items {
            match it {
                Item::Test(t) => {
                    if tests.contains_key(&t.name.name) {
                        return Err(format!(
                            "duplicate test base `{}` across input files", t.name.name
                        ));
                    }
                    tests.insert(t.name.name.clone(), t.clone());
                }
                Item::Extend(e) => {
                    let target = e.target.segments.last()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    match &e.body {
                        ExtendBody::Test(items) => {
                            test_extends.push((target, items.clone()));
                        }
                        _ => {
                            // Non-test extends pass through — they apply at
                            // type/component resolution, not test merge time.
                            other_items.push(it.clone());
                        }
                    }
                }
                _ => other_items.push(it.clone()),
            }
        }
    }

    // Apply test extends.
    for (target, items) in test_extends {
        let test = tests.get_mut(&target).ok_or_else(|| {
            format!("`extend {target}` has no matching base `test {target}` in input files")
        })?;
        test.items.extend(items);
    }

    // Validate `--test <name>` if given — surface a clear error when
    // the requested test doesn't exist. Otherwise pass ALL tests
    // through to codegen, which emits one `run_<TestName>` per test
    // and a dispatcher `main()` that picks one at runtime via
    // `--test <name>` / `HARC_TEST` (Phase 1b of
    // docs/separate-compilation-plan.md). The `pick` here used to
    // filter at merge time; now it just validates.
    if let Some(name) = pick {
        if !tests.contains_key(name) {
            return Err(format!("no test named `{name}` in input files"));
        }
    }
    if tests.is_empty() {
        return Err("no `test` declaration found in input files".into());
    }

    // Synthetic file: ALL tests plus all other items, in stable order
    // (alphabetical-by-name for tests to keep emitted output
    // deterministic across runs).
    let mut items = other_items;
    let mut test_names: Vec<String> = tests.keys().cloned().collect();
    test_names.sort();
    for name in test_names {
        items.push(Item::Test(tests.remove(&name).unwrap()));
    }
    Ok(SourceFile { items, inner_doc: None, frontmatter: None })
}
