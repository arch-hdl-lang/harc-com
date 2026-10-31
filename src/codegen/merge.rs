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

    {
        let refs: Vec<&SourceFile> = files.iter().collect();
        if let Err(e) = validate_extend_targets(&refs) {
            let all: Vec<SourceInfo> = files.iter().flat_map(|f| f.sources.clone()).collect();
            return Err(e.render_with(&all));
        }
    }

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
                    // Key by the full dotted path: a qualified target was
                    // validated above to name a declaration inside a
                    // package, so it must never fold into a same-named
                    // top-level test (only top-level tests are indexed).
                    let target = e
                        .target
                        .segments
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
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
            if target.contains('.') {
                format!(
                    "`extend {target}`: test extensions merge only into top-level tests, \
                     and a test declared inside a package is not one; extend a top-level \
                     `test` by its bare name"
                )
            } else {
                format!("`extend {target}` has no matching base `test {target}` in input files")
            }
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

/// A rejected `extend` target: the source the offending item came from plus
/// a spanned diagnostic, so callers can render it against that file's text.
#[derive(Debug)]
pub struct ExtendTargetError {
    pub source: SourceId,
    pub error: crate::diagnostics::CompileError,
}

impl ExtendTargetError {
    /// One-line `file:line:col: message` form for string-error callers.
    pub fn render_with(&self, sources: &[SourceInfo]) -> String {
        let message = self.error.to_string();
        let offset = match &self.error {
            crate::diagnostics::CompileError::General { span, .. } => span.offset(),
            _ => return message,
        };
        match sources.iter().find(|s| s.id == self.source) {
            Some(src) => {
                let before = &src.text[..offset.min(src.text.len())];
                let line = before.matches('\n').count() + 1;
                let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
                format!("{}:{line}:{col}: {message}", src.name)
            }
            None => message,
        }
    }
}

/// Validate every qualified `extend` target against the spec §3.6 depth-1
/// rule.
///
/// An unqualified `extend T` keeps its by-name resolution. A qualified
/// `extend P.T` (or `P.Q.T`) resolves its qualifier as a package path
/// (nested packages chain by dot) first relative to the package enclosing
/// the `extend`, then absolute from the root. Every declaration of that
/// package path counts (a package may be reopened, also across files), and
/// one of them must declare `T` as a *base* declaration. A qualifier that
/// names no package is an error, and so is one whose package only holds
/// `extend T`: extending an extension is the depth-2 chain the spec
/// forbids. Packages are collected from the given files only — a `use`
/// import carries buses, never packages.
pub fn validate_extend_targets(files: &[&SourceFile]) -> Result<(), ExtendTargetError> {
    use crate::diagnostics::CompileError;
    use std::collections::HashMap;

    type Packages<'a> = HashMap<String, Vec<&'a PackageDecl>>;

    fn index_packages<'a>(prefix: &str, items: &'a [Item], out: &mut Packages<'a>) {
        for it in items {
            if let Item::Package(p) = it {
                let path = if prefix.is_empty() {
                    p.name.name.clone()
                } else {
                    format!("{prefix}.{}", p.name.name)
                };
                index_packages(&path, &p.items, out);
                out.entry(path).or_default().push(p);
            }
        }
    }

    /// `use` lines keyed by the name they could be mistaken to import (the
    /// last segment and the full path), mapped to the path as written.
    fn collect_uses(items: &[Item], out: &mut HashMap<String, String>) {
        for it in items {
            match it {
                Item::Use(u) => {
                    let written = u
                        .path
                        .segments
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    if let Some(last) = u.path.segments.last() {
                        out.entry(last.name.clone()).or_insert_with(|| written.clone());
                    }
                    out.entry(written.clone()).or_insert(written);
                }
                Item::Package(p) => collect_uses(&p.items, out),
                _ => {}
            }
        }
    }

    fn check_items(
        enclosing: &str,
        items: &[Item],
        packages: &Packages<'_>,
        uses: &HashMap<String, String>,
    ) -> Result<(), CompileError> {
        for it in items {
            match it {
                Item::Package(p) => {
                    let path = if enclosing.is_empty() {
                        p.name.name.clone()
                    } else {
                        format!("{enclosing}.{}", p.name.name)
                    };
                    check_items(&path, &p.items, packages, uses)?;
                }
                Item::Extend(e) if e.target.segments.len() > 1 => {
                    let segs = &e.target.segments;
                    let (last, qual) = segs.split_last().expect("len > 1");
                    let qual_name = qual
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    let full = format!("{qual_name}.{}", last.name);
                    let qual_span = qual[0].span.merge(qual[qual.len() - 1].span);
                    let relative = (!enclosing.is_empty()).then(|| format!("{enclosing}.{qual_name}"));
                    let resolved = relative
                        .as_ref()
                        .and_then(|r| packages.get(r).map(|d| (r.clone(), d)))
                        .or_else(|| packages.get(&qual_name).map(|d| (qual_name.clone(), d)));
                    let Some((pkg_path, decls)) = resolved else {
                        let searched = match &relative {
                            Some(r) => format!("neither `{r}` nor `{qual_name}` is a declared package"),
                            None => format!("`{qual_name}` is not a declared package"),
                        };
                        let root = qual[0].name.as_str();
                        let use_note = match uses.get(root).or_else(|| uses.get(&qual_name)) {
                            Some(written) => format!(
                                "; `use {written}` does not make package `{root}` visible — `use` \
                                 imports only carry buses, so pass the file declaring package \
                                 `{root}` as an input"
                            ),
                            None => String::new(),
                        };
                        return Err(CompileError::general(
                            &format!(
                                "`extend {full}`: qualifier `{qual_name}` does not name a declared \
                                 package ({searched} in the input files){use_note}"
                            ),
                            qual_span,
                        ));
                    };
                    let items = || decls.iter().flat_map(|d| d.items.iter());
                    let has_base = items().any(|i| {
                        !matches!(
                            i,
                            Item::Extend(_) | Item::Use(_) | Item::Apply(_) | Item::Package(_)
                        ) && i.as_construct().name().name == last.name
                    });
                    if has_base {
                        continue;
                    }
                    let extends_it = items().any(|i| {
                        matches!(i, Item::Extend(x) if x.target.segments.last()
                            .is_some_and(|s| s.name == last.name))
                    });
                    let message = if extends_it {
                        format!(
                            "`extend {full}` extends an extension: package `{pkg_path}` only \
                             extends `{0}`, it does not declare it. An `extend` must target a base \
                             declaration (spec §3.6 depth-1 rule); extend `{0}` directly and \
                             compose with `apply`",
                            last.name
                        )
                    } else {
                        format!("`extend {full}`: package `{pkg_path}` declares no `{}`", last.name)
                    };
                    return Err(CompileError::general(&message, e.target.span));
                }
                _ => {}
            }
        }
        Ok(())
    }

    let mut packages = Packages::new();
    let mut uses = HashMap::new();
    for f in files {
        index_packages("", &f.items, &mut packages);
        collect_uses(&f.items, &mut uses);
    }
    for f in files {
        for (index, it) in f.items.iter().enumerate() {
            check_items("", std::slice::from_ref(it), &packages, &uses).map_err(|error| {
                ExtendTargetError {
                    source: f.item_sources.get(index).copied().unwrap_or_default(),
                    error,
                }
            })?;
        }
    }
    Ok(())
}
