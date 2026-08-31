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

/// Takes the parsed files **by value** and moves their items into the
/// merged file. It used to borrow and deep-clone every item, which on a
/// large suite means copying every statement of every test and then
/// dropping the originals — 5.2s of a 46s frontend on the 352-test
/// benchmark, plus the peak memory of holding both copies. Callers that
/// still need their parsed files afterwards should clone at the call site,
/// where the cost is visible.
pub fn merge_for_sim(files: Vec<SourceFile>, pick: Option<&str>) -> Result<SourceFile, String> {
    // Index test bases by name and collect extends targeting tests.
    let mut tests: std::collections::HashMap<String, (TestDecl, SourceId)> =
        std::collections::HashMap::new();
    let mut test_extends: Vec<(String, Vec<TestItem>, SourceId)> = Vec::new();
    let mut other_items: Vec<(Item, SourceId)> = Vec::new();
    let mut sources = Vec::new();

    for file in files {
        let SourceFile {
            items: file_items,
            item_sources: file_item_sources,
            sources: file_sources,
            ..
        } = file;
        sources.extend(file_sources);
        assert_eq!(file_items.len(), file_item_sources.len());
        for (it, source) in file_items.into_iter().zip(file_item_sources) {
            match it {
                Item::Test(t) => {
                    if tests.contains_key(&t.name.name) {
                        return Err(format!(
                            "duplicate test base `{}` across input files",
                            t.name.name
                        ));
                    }
                    tests.insert(t.name.name.clone(), (t, source));
                }
                Item::Extend(e) => {
                    let target = e
                        .target
                        .segments
                        .last()
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    if let ExtendBody::Test(items) = e.body {
                        test_extends.push((target, items, source));
                    } else {
                        // Non-test extends pass through — they apply at
                        // type/component resolution, not test merge time.
                        other_items.push((Item::Extend(e), source));
                    }
                }
                other => other_items.push((other, source)),
            }
        }
    }

    // Apply test extends.
    for (target, items, source) in test_extends {
        let (test, _) = tests.get_mut(&target).ok_or_else(|| {
            format!("`extend {target}` has no matching base `test {target}` in input files")
        })?;
        test.item_sources
            .extend(std::iter::repeat_n(source, items.len()));
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
    let (mut items, mut item_sources): (Vec<_>, Vec<_>) = other_items.into_iter().unzip();
    let mut test_names: Vec<String> = tests.keys().cloned().collect();
    test_names.sort();
    for name in test_names {
        let (test, source) = tests.remove(&name).unwrap();
        items.push(Item::Test(test));
        item_sources.push(source);
    }
    Ok(SourceFile {
        items,
        item_sources,
        sources,
        inner_doc: None,
        frontmatter: None,
    })
}

/// Return a synthetic source file containing all non-test items and only the
/// selected test. `merge_for_sim` intentionally keeps every test for the
/// default build-once-run-many path; focused codegen uses this helper after
/// that validation/merge step when the user explicitly asks for a test-only
/// compile.
pub fn filter_tests_for_codegen(file: &SourceFile, pick: &str) -> Result<SourceFile, String> {
    let mut found = false;
    let mut items = Vec::with_capacity(file.items.len());
    let mut item_sources = Vec::with_capacity(file.items.len());
    for (index, item) in file.items.iter().enumerate() {
        match item {
            Item::Test(t) if t.name.name == pick => {
                found = true;
                items.push(item.clone());
                item_sources.push(file.item_source(index));
            }
            Item::Test(_) => {}
            _ => {
                items.push(item.clone());
                item_sources.push(file.item_source(index));
            }
        }
    }
    if !found {
        return Err(format!("no test named `{pick}` in input files"));
    }
    Ok(SourceFile {
        items,
        item_sources,
        sources: file.sources.clone(),
        inner_doc: file.inner_doc.clone(),
        frontmatter: file.frontmatter.clone(),
    })
}
