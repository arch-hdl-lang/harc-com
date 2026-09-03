use harc::codegen::{cpp_tb, merge, tbir};
use harc::ir::{self, lower, verify};
use harc::parser::parse_source;
use std::path::PathBuf;
use std::process::Command;

const COPY_SOURCE: &str = r#"
domain SysDomain
  freq_mhz: 100
end domain SysDomain

agent CopyCell
    value : uint<8> default 7

    hookable set(v: uint<8>)
        value = v
    end set
end agent CopyCell

agent OtherCell
    value : uint<8> default 7
end agent OtherCell

agent CopyOwner
    primary : CopyCell
    snapshot : CopyCell
    other : OtherCell

    hookable load(input_state: CopyCell)
        snapshot = input_state
    end load

    hookable load_via_local(input_state: CopyCell)
        let local_state : CopyCell = input_state
        snapshot = local_state
        local_state.set(88)
    end load_via_local

    hookable clone()
        snapshot = primary
    end clone

    hookable shadow(primary: CopyCell, input_state: CopyCell)
        primary = input_state
    end shadow

    hookable capture_primary()
        load(primary)
    end capture_primary

    hookable capture_primary_via_local()
        load_via_local(primary)
    end capture_primary_via_local

    hookable exercise_shadow()
        shadow(primary, snapshot)
    end exercise_shadow

    hookable copy_into(dst: CopyOwner, input_state: CopyCell)
        dst.snapshot = input_state
    end copy_into
end agent CopyOwner

test ComponentSubcopyValueTest
    let dut : Top
    let source : CopyCell
    let owner : CopyOwner
    clock clk = SysDomain

    run
        source.set(41)
        owner.primary = source
        owner.capture_primary_via_local()
        assert owner.snapshot.value == 41
            else fail("parameter-to-local copy aliased source")
        assert owner.primary.value == 41
            else fail("typed component local aliased parameter")
        assert source.value == 41
            else fail("component parameter mutation escaped value semantics")
        let local_source : CopyCell = source
        owner.snapshot = local_source
        local_source.set(73)
        assert owner.snapshot.value == 41
            else fail("local-to-self copy aliased local")
        assert source.value == 41
            else fail("typed component local aliased initializer")
        owner.capture_primary()
        owner.clone()
        owner.exercise_shadow()
        owner.primary.set(99)
        source = owner.primary
        owner.primary.set(123)
        assert source.value == 99
            else fail("direct testbench component copy aliased nested source")
        log(info, "ALL TESTS PASSED - typed component locals copy by value")
        wait 1 cycle
    end run
end test ComponentSubcopyValueTest
"#;

const MODE_COPY_SOURCE: &str = r#"
agent CopyCell
    value : uint<8> default 7

    hookable set(v: uint<8>)
        value = v
    end set
end agent CopyCell

transactor GatedOwner
    when active
        slot : CopyCell

        hookable touch()
            slot = slot
        end touch
    end when
end transactor GatedOwner

agent Holder
    slot : CopyCell
end agent Holder

env ModeRoot
    active_owner : GatedOwner active
    passive_owner : GatedOwner passive
    source : CopyCell
    holder : Holder
end env ModeRoot

test ModeCopyTest
    let dut : Top
    let root : ModeRoot
    let shadow : CopyCell

    run
        root.active_owner.slot = root.source
        root.holder.slot = root.active_owner.slot
        let local_slot : CopyCell = root.active_owner.slot
        root.holder.slot = local_slot
        shadow = root.active_owner.slot
    end run
end test ModeCopyTest
"#;

const QUALIFIED_COPY_SOURCE: &str = r#"
agent CopyCell
    value : uint<8> default 7
end agent CopyCell

agent Holder
    slot : CopyCell
end agent Holder

agent OtherCell
    value : uint<8> default 9
end agent OtherCell

agent AxiMaster
    csr : CopyCell
end agent AxiMaster

agent Checkers
    axi_master : AxiMaster
end agent Checkers

testbench CopyTb
    dut : Top
    holder : Holder
    source : CopyCell
    spare_holder : Holder
    spare_source : CopyCell
    csr_shadow : CopyCell
    checkers : Checkers
    spare_shadow : CopyCell
    spare_checkers : Checkers
end testbench CopyTb

impl QualifiedCopyTest for CopyTb
    run
        let holder : Holder = _tb.spare_holder
        let source : CopyCell = _tb.spare_source
        let csr_shadow : CopyCell = _tb.spare_shadow
        let checkers : Checkers = _tb.spare_checkers
        _tb.holder.slot = _tb.source
        _tb.csr_shadow = _tb.checkers.axi_master.csr
    end run
end impl QualifiedCopyTest
"#;

fn lower_source(src: &str) -> Result<ir::TbProgram, lower::LowerError> {
    let parsed = parse_source(src).expect("source parses");
    let merged = merge::merge_for_sim(vec![parsed], None).expect("source merges");
    lower::lower_program(&merged)
}

fn merged_source(src: &str) -> harc::ast::SourceFile {
    let parsed = parse_source(src).expect("source parses");
    merge::merge_for_sim(vec![parsed], None).expect("source merges")
}

fn verilator_present() -> bool {
    let present = Command::new("verilator")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    assert!(
        present || std::env::var_os("HARC_REQUIRE_VERILATOR").is_none(),
        "HARC_REQUIRE_VERILATOR is set but `verilator` is not on PATH"
    );
    present
}

fn runtime_outdir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "harc_tbir_component_local_copy_{}",
        std::process::id()
    ))
}

fn method<'a>(prog: &'a ir::TbProgram, component: &str, method: &str) -> &'a ir::TbFunction {
    let component = prog
        .components
        .iter()
        .find(|candidate| candidate.name == component)
        .expect("component exists");
    let function = component.method(method).expect("method exists").function;
    prog.function(function)
}

fn statements(function: &ir::TbFunction) -> impl Iterator<Item = &ir::Stmt> {
    function.blocks.iter().flat_map(|block| &block.stmts)
}

#[test]
fn lowers_self_parameter_self_path_test_scope_and_local_destination_copies() {
    let prog = lower_source(COPY_SOURCE).expect("component copies lower");
    verify::verify_program(&prog).expect("component copies verify");

    let load = method(&prog, "CopyOwner", "load");
    let input_state = load
        .locals
        .iter()
        .position(|local| local.name == "input_state")
        .map(|index| ir::LocalId(index as u32))
        .expect("input_state parameter");
    assert!(statements(load).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentSubAssign {
            dst: ir::ComponentBase::SelfField,
            field,
            src: ir::ComponentBase::Local(source),
        } if field == "snapshot" && *source == input_state
    )));

    let clone = method(&prog, "CopyOwner", "clone");
    assert!(statements(clone).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentSubAssign {
            dst: ir::ComponentBase::SelfField,
            field,
            src: ir::ComponentBase::Path(path),
        } if field == "snapshot" && path == &["self", "primary"]
    )));

    let copy_into = method(&prog, "CopyOwner", "copy_into");
    let dst = copy_into
        .locals
        .iter()
        .position(|local| local.name == "dst")
        .map(|index| ir::LocalId(index as u32))
        .expect("dst parameter");
    let input_state = copy_into
        .locals
        .iter()
        .position(|local| local.name == "input_state")
        .map(|index| ir::LocalId(index as u32))
        .expect("input_state parameter");
    assert!(statements(copy_into).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentSubAssign {
            dst: ir::ComponentBase::Local(target),
            field,
            src: ir::ComponentBase::Local(source),
        } if *target == dst && field == "snapshot" && *source == input_state
    )));

    let run = prog.function(prog.tests[0].run);
    assert!(statements(run).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentSubAssign {
            dst: ir::ComponentBase::Path(dst),
            field,
            src: ir::ComponentBase::Path(src),
        } if dst == &["owner"] && field == "primary" && src == &["source"]
    )));
    assert!(statements(run).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentAssign {
            dst: ir::ComponentBase::Path(dst),
            src: ir::ComponentBase::Path(src),
        } if dst == &["source"] && src == &["owner", "primary"]
    )));

    let shadow = method(&prog, "CopyOwner", "shadow");
    let primary = shadow
        .locals
        .iter()
        .position(|local| local.name == "primary")
        .map(|index| ir::LocalId(index as u32))
        .expect("shadowing primary parameter");
    let input_state = shadow
        .locals
        .iter()
        .position(|local| local.name == "input_state")
        .map(|index| ir::LocalId(index as u32))
        .expect("shadow input_state parameter");
    assert!(statements(shadow).any(|stmt| matches!(
        stmt,
        ir::Stmt::Assign(target, ir::Expr::Local(source))
            if *target == primary && *source == input_state
    )));
    assert!(!statements(shadow).any(|stmt| matches!(stmt, ir::Stmt::ComponentSubAssign { .. })));

    let cpp = tbir::emit(
        &prog,
        &merged_source(COPY_SOURCE),
        &cpp_tb::EmitOpts::default(),
    )
    .expect("component copies emit");
    for expected in [
        "self.snapshot._harc_copy_user_state_from(input_state);",
        "self.snapshot._harc_copy_user_state_from(self.primary);",
        "dst.snapshot._harc_copy_user_state_from(input_state);",
        "owner.primary._harc_copy_user_state_from(source);",
        "source._harc_copy_user_state_from(owner.primary);",
        "primary = input_state;",
    ] {
        assert!(cpp.contains(expected), "missing `{expected}` in:\n{cpp}");
    }
    assert!(
        !cpp.contains("BUG:component-local"),
        "component copies must use the context-aware base renderer:\n{cpp}"
    );
}

#[test]
fn lowers_typed_component_locals_as_value_copies() {
    let prog = lower_source(COPY_SOURCE).expect("typed component locals lower");
    verify::verify_program(&prog).expect("typed component locals verify");

    let copy_cell = prog
        .components
        .iter()
        .position(|component| component.name == "CopyCell")
        .map(|index| ir::ComponentId(index as u32))
        .expect("CopyCell component");
    let load = method(&prog, "CopyOwner", "load_via_local");
    let input_state = load
        .locals
        .iter()
        .position(|local| local.name == "input_state")
        .map(|index| ir::LocalId(index as u32))
        .expect("input_state parameter");
    let local_state = load
        .locals
        .iter()
        .position(|local| local.name == "local_state")
        .map(|index| ir::LocalId(index as u32))
        .expect("component local");
    assert_eq!(load.local(local_state).ty, ir::IrType::Component(copy_cell));
    assert!(statements(load).any(|stmt| matches!(
        stmt,
        ir::Stmt::Assign(target, ir::Expr::ComponentValue {
            base: ir::ComponentBase::Local(source),
        }) if *target == local_state && *source == input_state
    )));
    assert!(statements(load).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentSubAssign {
            dst: ir::ComponentBase::SelfField,
            field,
            src: ir::ComponentBase::Local(source),
        } if field == "snapshot" && *source == local_state
    )));

    let run = prog.function(prog.tests[0].run);
    let local_source = run
        .locals
        .iter()
        .position(|local| local.name == "local_source")
        .map(|index| ir::LocalId(index as u32))
        .expect("test-body component local");
    assert_eq!(run.local(local_source).ty, ir::IrType::Component(copy_cell));
    assert!(statements(run).any(|stmt| matches!(
        stmt,
        ir::Stmt::Assign(target, ir::Expr::ComponentValue {
            base: ir::ComponentBase::Path(path),
        }) if *target == local_source && path == &["source"]
    )));

    let cpp = tbir::emit(
        &prog,
        &merged_source(COPY_SOURCE),
        &cpp_tb::EmitOpts::default(),
    )
    .expect("component locals emit");
    for expected in [
        "CopyCell local_state{};",
        "local_state = input_state;",
        "self.snapshot._harc_copy_user_state_from(local_state);",
        "CopyCell_set(local_state, 88);",
        "CopyCell local_source{};",
        "local_source = source;",
        "CopyCell_set(local_source, 73);",
    ] {
        assert!(cpp.contains(expected), "missing `{expected}` in:\n{cpp}");
    }
}

#[test]
fn tbir_component_locals_copy_values_at_runtime() {
    if !verilator_present() {
        eprintln!("skipping component-local runtime test: verilator not found on PATH");
        return;
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let outdir = runtime_outdir();
    let _ = std::fs::remove_dir_all(&outdir);
    std::fs::create_dir_all(&outdir).expect("create component-copy runtime directory");
    let source = outdir.join("component_local_copy.harc");
    std::fs::write(&source, COPY_SOURCE).expect("write component-copy runtime source");

    let output = Command::new(env!("CARGO_BIN_EXE_harc"))
        .arg("sim")
        .arg("--codegen")
        .arg("tbir")
        .arg("--sv")
        .arg(root.join("tests/dut/top_counter.sv"))
        .arg(&source)
        .arg("--top")
        .arg("Top")
        .arg("--outdir")
        .arg(&outdir)
        .output()
        .expect("spawn TB-IR component-copy simulation");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "TB-IR component-copy simulation failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("ALL TESTS PASSED - typed component locals copy by value")
            || stderr.contains("ALL TESTS PASSED - typed component locals copy by value"),
        "component-copy simulation did not reach its success marker:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    std::fs::remove_dir_all(&outdir).expect("remove component-copy runtime directory");
}

#[test]
fn component_copy_paths_obey_active_passive_access() {
    let prog = lower_source(MODE_COPY_SOURCE).expect("active copy paths lower");
    verify::verify_program(&prog).expect("active copy paths verify");
    let run = prog.function(prog.tests[0].run);
    assert!(statements(run).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentAssign {
            dst: ir::ComponentBase::Path(dst),
            src: ir::ComponentBase::Path(src),
        } if dst == &["shadow"] && src == &["root", "active_owner", "slot"]
    )));

    let passive_destination = MODE_COPY_SOURCE.replace(
        "root.active_owner.slot = root.source",
        "root.passive_owner.slot = root.source",
    );
    let err = lower_source(&passive_destination)
        .expect_err("an active-only destination cannot be reached through passive ownership");
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(
        err.to_string().contains("active-only") && err.to_string().contains("root.passive_owner"),
        "{err}"
    );

    let passive_source = MODE_COPY_SOURCE.replace(
        "root.holder.slot = root.active_owner.slot",
        "root.holder.slot = root.passive_owner.slot",
    );
    let err = lower_source(&passive_source)
        .expect_err("an active-only source cannot be reached through passive ownership");
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(
        err.to_string().contains("active-only") && err.to_string().contains("root.passive_owner"),
        "{err}"
    );

    let passive_direct_source = MODE_COPY_SOURCE.replace(
        "shadow = root.active_owner.slot",
        "shadow = root.passive_owner.slot",
    );
    let err = lower_source(&passive_direct_source)
        .expect_err("a direct testbench destination cannot copy an inactive nested source");
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(
        err.to_string().contains("active-only") && err.to_string().contains("root.passive_owner"),
        "{err}"
    );
}

#[test]
fn explicit_tb_component_paths_are_not_hijacked_by_shadowing_locals() {
    let prog = lower_source(QUALIFIED_COPY_SOURCE).expect("qualified component paths lower");
    verify::verify_program(&prog).expect("qualified component paths verify");
    let run = prog.function(prog.tests[0].run);
    assert!(statements(run).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentSubAssign {
            dst: ir::ComponentBase::Path(dst),
            field,
            src: ir::ComponentBase::Path(src),
        } if dst == &["holder"] && field == "slot" && src == &["source"]
    )));
    assert!(statements(run).any(|stmt| matches!(
        stmt,
        ir::Stmt::ComponentAssign {
            dst: ir::ComponentBase::Path(dst),
            src: ir::ComponentBase::Path(src),
        } if dst == &["csr_shadow"] && src == &["checkers", "axi_master", "csr"]
    )));
    let cpp = tbir::emit(
        &prog,
        &merged_source(QUALIFIED_COPY_SOURCE),
        &cpp_tb::EmitOpts::default(),
    )
    .expect("qualified component paths emit");
    assert!(cpp.contains("Holder holder_2{};"), "{cpp}");
    assert!(cpp.contains("CopyCell source_2{};"), "{cpp}");
    assert!(cpp.contains("CopyCell csr_shadow_2{};"), "{cpp}");
    assert!(cpp.contains("Checkers checkers_2{};"), "{cpp}");
    assert!(
        cpp.contains("holder.slot._harc_copy_user_state_from(source);"),
        "{cpp}"
    );
    assert!(
        cpp.contains("csr_shadow._harc_copy_user_state_from(checkers.axi_master.csr);"),
        "{cpp}"
    );
    assert!(!cpp.contains("holder_2.slot = source_2;"), "{cpp}");
    assert!(
        !cpp.contains("csr_shadow_2 = checkers_2.axi_master.csr;"),
        "{cpp}"
    );
}

#[test]
fn rejects_mismatched_direct_testbench_component_copy() {
    let mismatch = QUALIFIED_COPY_SOURCE.replace("csr_shadow : CopyCell", "csr_shadow : OtherCell");
    let err =
        lower_source(&mismatch).expect_err("direct component copy schemas must match exactly");
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    let message = err.to_string();
    assert!(message.contains("CopyCell"), "{message}");
    assert!(message.contains("OtherCell"), "{message}");
}

#[test]
fn rejects_mismatched_and_unknown_component_copy_operands() {
    let mismatch = COPY_SOURCE.replacen("snapshot = input_state", "other = input_state", 1);
    let err = lower_source(&mismatch).expect_err("different component schemas must not copy");
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    let message = err.to_string();
    assert!(message.contains("CopyCell"), "{message}");
    assert!(message.contains("OtherCell"), "{message}");

    let unknown_source =
        COPY_SOURCE.replacen("snapshot = input_state", "snapshot = missing_state", 1);
    let err = lower_source(&unknown_source).expect_err("unknown source must not copy");
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(err.to_string().contains("component value"), "{err}");

    let unknown_destination =
        COPY_SOURCE.replacen("snapshot = input_state", "missing_state = input_state", 1);
    let err = lower_source(&unknown_destination).expect_err("unknown destination must fail");
    assert!(matches!(err, lower::LowerError::Invalid(_)), "{err:?}");
    assert!(
        err.to_string().contains("has no field `missing_state`"),
        "{err}"
    );

    let unsupported_ownership =
        COPY_SOURCE.replacen("snapshot = input_state", "snapshot = input_state.child", 1);
    let err = lower_source(&unsupported_ownership)
        .expect_err("nested paths rooted at a component parameter need an owned path base");
    assert!(
        matches!(err, lower::LowerError::Unsupported { .. }),
        "{err:?}"
    );
    assert!(
        err.to_string()
            .contains("nested path rooted at component local"),
        "{err}"
    );

    let mismatched_local = COPY_SOURCE.replacen(
        "let local_state : CopyCell = input_state",
        "let local_state : OtherCell = input_state",
        1,
    );
    let err = lower_source(&mismatched_local)
        .expect_err("a typed component local requires the exact initializer schema");
    let message = err.to_string();
    assert!(message.contains("CopyCell"), "{message}");
    assert!(message.contains("OtherCell"), "{message}");

    let scalar_local = COPY_SOURCE.replacen(
        "let local_state : CopyCell = input_state",
        "let local_state : CopyCell = 7",
        1,
    );
    let err = lower_source(&scalar_local)
        .expect_err("a typed component local requires a component-valued initializer");
    assert!(
        err.to_string().contains("requires a component initializer"),
        "{err}"
    );
}

#[test]
fn verifier_rejects_corrupt_component_copy_bases_fields_and_types() {
    let prog = lower_source(COPY_SOURCE).expect("component copies lower");
    verify::verify_program(&prog).expect("control verifies");

    let corrupt =
        |mut candidate: ir::TbProgram,
         mutate: &dyn Fn(&mut ir::ComponentBase, &mut String, &mut ir::ComponentBase)| {
            let load_id = candidate
                .components
                .iter()
                .find(|component| component.name == "CopyOwner")
                .and_then(|component| component.method("load"))
                .map(|method| method.function)
                .expect("load method");
            let stmt = candidate.functions[load_id.index()]
                .blocks
                .iter_mut()
                .flat_map(|block| &mut block.stmts)
                .find_map(|stmt| match stmt {
                    ir::Stmt::ComponentSubAssign { dst, field, src } => Some((dst, field, src)),
                    _ => None,
                })
                .expect("component copy statement");
            mutate(stmt.0, stmt.1, stmt.2);
            verify::verify_program(&candidate).expect_err("corrupt component copy must not verify")
        };

    let errors = corrupt(prog.clone(), &|_, field, _| *field = "missing".to_string());
    assert!(format!("{errors:?}").contains("ComponentSubAssign"));

    let errors = corrupt(prog.clone(), &|dst, _, _| {
        *dst = ir::ComponentBase::Path(vec!["missing".to_string()])
    });
    assert!(format!("{errors:?}").contains("ComponentSubAssign"));

    let other = prog
        .components
        .iter()
        .position(|component| component.name == "OtherCell")
        .map(|index| ir::ComponentId(index as u32))
        .expect("OtherCell component");
    let mut wrong_type = prog.clone();
    let load_id = wrong_type
        .components
        .iter()
        .find(|component| component.name == "CopyOwner")
        .and_then(|component| component.method("load"))
        .map(|method| method.function)
        .expect("load method");
    let source = wrong_type.functions[load_id.index()]
        .locals
        .iter()
        .position(|local| local.name == "input_state")
        .expect("input_state parameter");
    wrong_type.functions[load_id.index()].locals[source].ty = ir::IrType::Component(other);
    let errors = verify::verify_program(&wrong_type).expect_err("schema mismatch must not verify");
    assert!(format!("{errors:?}").contains("ComponentSubAssign"));

    let mut wrong_local_initializer = prog.clone();
    let run_id = wrong_local_initializer.tests[0].run;
    let local_source = wrong_local_initializer.functions[run_id.index()]
        .locals
        .iter()
        .position(|local| local.name == "local_source")
        .map(|index| ir::LocalId(index as u32))
        .expect("component local");
    let initializer = wrong_local_initializer.functions[run_id.index()]
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::Assign(target, ir::Expr::ComponentValue { base })
                if *target == local_source =>
            {
                Some(base)
            }
            _ => None,
        })
        .expect("component-local initializer");
    *initializer = ir::ComponentBase::Path(vec!["owner".to_string(), "other".to_string()]);
    let errors = verify::verify_program(&wrong_local_initializer)
        .expect_err("component-local schema mismatch must not verify");
    assert!(format!("{errors:?}").contains("TypeMismatch"));

    let corrupt_direct =
        |mut candidate: ir::TbProgram,
         mutate: &dyn Fn(&mut ir::ComponentBase, &mut ir::ComponentBase)| {
            let run_id = candidate.tests[0].run;
            let copy = candidate.functions[run_id.index()]
                .blocks
                .iter_mut()
                .flat_map(|block| &mut block.stmts)
                .find_map(|stmt| match stmt {
                    ir::Stmt::ComponentAssign { dst, src } => Some((dst, src)),
                    _ => None,
                })
                .expect("direct component copy statement");
            mutate(copy.0, copy.1);
            verify::verify_program(&candidate)
                .expect_err("corrupt direct component copy must not verify")
        };

    let errors = corrupt_direct(prog.clone(), &|dst, _| {
        *dst = ir::ComponentBase::Path(vec!["missing".to_string()])
    });
    assert!(format!("{errors:?}").contains("ComponentAssign"));

    let errors = corrupt_direct(prog.clone(), &|dst, _| {
        *dst = ir::ComponentBase::Path(vec!["owner".to_string(), "primary".to_string()])
    });
    let message = format!("{errors:?}");
    assert!(message.contains("ComponentAssign"), "{message}");
    assert!(
        message.contains("direct testbench component field"),
        "{message}"
    );

    let errors = corrupt_direct(prog.clone(), &|dst, _| *dst = ir::ComponentBase::SelfField);
    let message = format!("{errors:?}");
    assert!(message.contains("ComponentAssign"), "{message}");
    assert!(
        message.contains("direct testbench component field"),
        "{message}"
    );

    let errors = corrupt_direct(prog, &|_, src| {
        *src = ir::ComponentBase::Path(vec!["owner".to_string(), "other".to_string()])
    });
    assert!(format!("{errors:?}").contains("ComponentAssign"));
}

#[test]
fn verifier_rejects_passive_component_copy_paths() {
    let prog = lower_source(MODE_COPY_SOURCE).expect("active copy paths lower");
    verify::verify_program(&prog).expect("control verifies");

    let mutate_path = |mut candidate: ir::TbProgram, destination: bool| {
        let run_id = candidate.tests[0].run;
        let copy = candidate.functions[run_id.index()]
            .blocks
            .iter_mut()
            .flat_map(|block| &mut block.stmts)
            .filter_map(|stmt| match stmt {
                ir::Stmt::ComponentSubAssign { dst, src, .. } => Some((dst, src)),
                _ => None,
            })
            .nth(if destination { 0 } else { 1 })
            .expect("component copy statement");
        let base = if destination { copy.0 } else { copy.1 };
        let ir::ComponentBase::Path(path) = base else {
            panic!("mode-sensitive copy base must be a path")
        };
        path[1] = "passive_owner".to_string();
        verify::verify_program(&candidate).expect_err("passive copy path must not verify")
    };

    let errors = mutate_path(prog.clone(), true);
    let message = format!("{errors:?}");
    assert!(message.contains("ComponentSubAssign"), "{message}");
    assert!(
        message.contains("disabled by its instance mode"),
        "{message}"
    );

    let errors = mutate_path(prog, false);
    let message = format!("{errors:?}");
    assert!(message.contains("ComponentSubAssign"), "{message}");
    assert!(
        message.contains("disabled by its instance mode"),
        "{message}"
    );

    let mut passive_direct_source = lower_source(MODE_COPY_SOURCE).expect("active paths lower");
    let run_id = passive_direct_source.tests[0].run;
    let source = passive_direct_source.functions[run_id.index()]
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::ComponentAssign {
                src: ir::ComponentBase::Path(path),
                ..
            } => Some(path),
            _ => None,
        })
        .expect("direct component copy source");
    source[1] = "passive_owner".to_string();
    let errors = verify::verify_program(&passive_direct_source)
        .expect_err("a passive direct-copy source must not verify");
    let message = format!("{errors:?}");
    assert!(message.contains("ComponentAssign"), "{message}");
    assert!(
        message.contains("disabled by its instance mode"),
        "{message}"
    );

    let mut inactive_self_body = lower_source(MODE_COPY_SOURCE).expect("active copy paths lower");
    let touch = inactive_self_body
        .components
        .iter_mut()
        .find(|component| component.name == "GatedOwner")
        .and_then(|component| {
            component
                .methods
                .iter_mut()
                .find(|method| method.name == "touch")
        })
        .expect("active touch method");
    touch.activation = ir::Activation::Always;
    let errors = verify::verify_program(&inactive_self_body)
        .expect_err("an always-on body cannot copy an active-only self field");
    let message = format!("{errors:?}");
    assert!(message.contains("ComponentSubAssign"), "{message}");
    assert!(message.contains("active-only"), "{message}");

    let mut passive_local_initializer = lower_source(MODE_COPY_SOURCE).expect("active paths lower");
    let run_id = passive_local_initializer.tests[0].run;
    let local_slot = passive_local_initializer.functions[run_id.index()]
        .locals
        .iter()
        .position(|local| local.name == "local_slot")
        .map(|index| ir::LocalId(index as u32))
        .expect("typed component local");
    let base = passive_local_initializer.functions[run_id.index()]
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.stmts)
        .find_map(|stmt| match stmt {
            ir::Stmt::Assign(target, ir::Expr::ComponentValue { base })
                if *target == local_slot =>
            {
                Some(base)
            }
            _ => None,
        })
        .expect("component-local initializer");
    let ir::ComponentBase::Path(path) = base else {
        panic!("mode-sensitive component initializer must use a path")
    };
    path[1] = "passive_owner".to_string();
    let errors = verify::verify_program(&passive_local_initializer)
        .expect_err("a passive component-local initializer must not verify");
    let message = format!("{errors:?}");
    assert!(message.contains("component value base"), "{message}");
    assert!(
        message.contains("disabled by its instance mode"),
        "{message}"
    );
}
