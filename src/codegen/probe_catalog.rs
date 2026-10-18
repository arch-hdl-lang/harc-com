//! Backend-neutral DUT probe catalog discovery.

use crate::ast::{ComponentItem, ComponentKind, Item, LetStmt, Probe, SourceFile, TestItem};
use std::borrow::Cow;

/// Return the validated union of all DUT probes in the suite. Identical
/// declarations shared by multiple tests are emitted once; conflicts are
/// rejected so lowering, C++ accessors, and the SV bind stub cannot disagree.
pub fn dut_probes(
    file: &SourceFile,
) -> Result<Option<(String, Vec<Probe>)>, crate::codegen::sv_stub::DutProbeCatalogError> {
    let lowered = source_with_bound_testbench_dut_probes(file);
    let catalog = crate::codegen::sv_stub::collect_suite_probes(&lowered)?;
    if catalog.probes.is_empty() {
        return Ok(None);
    }
    Ok(Some((
        catalog
            .dut_type
            .expect("a non-empty probe catalog always has a DUT type"),
        catalog.probes,
    )))
}

/// Materialize only the testbench-owned DUT declaration needed by probe
/// catalog discovery. Full lifecycle/helper desugaring belongs to lowering;
/// the probe catalog needs neither and must not depend on the native emitter.
fn source_with_bound_testbench_dut_probes(file: &SourceFile) -> Cow<'_, SourceFile> {
    let testbenches = file
        .items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let Item::Env(component) = item else {
                return None;
            };
            if component.kind != ComponentKind::Testbench {
                return None;
            }
            let dut = component.items.iter().find_map(|item| {
                let ComponentItem::Field(field) = item else {
                    return None;
                };
                (field.name.name == "dut" && !field.probes.is_empty()).then_some(field)
            })?;
            Some((
                component.name.name.clone(),
                (dut.clone(), file.item_source(index)),
            ))
        })
        .collect::<std::collections::HashMap<_, _>>();
    if testbenches.is_empty() {
        return Cow::Borrowed(file);
    }

    let mut materialized = None;
    for (item_index, item) in file.items.iter().enumerate() {
        let Item::Test(test) = item else {
            continue;
        };
        let Some(testbench) = test.for_testbench.as_ref() else {
            continue;
        };
        let Some((field, source_id)) = testbenches.get(&testbench.name) else {
            continue;
        };
        let output = materialized.get_or_insert_with(|| file.clone());
        let Item::Test(output_test) = &mut output.items[item_index] else {
            unreachable!("the cloned source preserves item variants");
        };
        if output_test.items.iter().any(|item| {
            matches!(item, TestItem::Let(decl) if decl.name.name == "dut" && !decl.probes.is_empty())
        }) {
            continue;
        }
        output_test.items.insert(
            0,
            TestItem::Let(LetStmt {
                name: field.name.clone(),
                ty: Some(field.ty.clone()),
                value: None,
                bind: false,
                probes: field.probes.clone(),
                bind_remap: field.bind_remap.clone(),
                span: field.span,
            }),
        );
        output_test.item_sources.insert(0, *source_id);
    }
    materialized.map(Cow::Owned).unwrap_or(Cow::Borrowed(file))
}
