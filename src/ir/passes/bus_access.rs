use crate::ir::{
    BusBindingId, BusBindingSchema, Expr, FunctionId, FunctionKind, IrType, LaneIndex,
    PortDirection, PortOrigin, PortRef, Stmt, TbFunction, TbProgram, TestId, TestbenchId,
    TestbenchTypeId, TransactorMethodTarget,
};

use super::dut_access::{DutInterfaceCatalog, DutInterfacePort};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum BusAccessOperation {
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BusAccessEntry {
    test: TestId,
    test_name: String,
    testbench: TestbenchId,
    function: FunctionId,
    function_name: String,
    binding: BusBindingId,
    field: String,
    bus: String,
    channel: String,
    signal: String,
    physical_port: String,
    operation: BusAccessOperation,
    direction: Option<PortDirection>,
    value_type: IrType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusAccessPlan {
    accesses: Vec<BusAccessEntry>,
}

impl BusAccessPlan {
    pub fn is_empty(&self) -> bool {
        self.accesses.is_empty()
    }

    pub fn abi_lines(&self) -> Vec<String> {
        self.accesses
            .iter()
            .map(|access| {
                format!(
                    "test={}:function={}:field={}:bus={}:logical={}.{}:physical={}:op={:?}:direction={:?}:type={:?}",
                    access.test_name,
                    access.function_name,
                    access.field,
                    access.bus,
                    access.channel,
                    access.signal,
                    access.physical_port,
                    access.operation,
                    access.direction,
                    access.value_type,
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusAccessPlanError(pub String);

impl std::fmt::Display for BusAccessPlanError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BusAccessPlanError {}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BindingSelector {
    Concrete {
        binding: BusBindingId,
        field: String,
    },
    TestbenchField {
        testbench: TestbenchTypeId,
        field: String,
        bus: String,
    },
    Bound {
        field: Option<String>,
    },
}

#[derive(Clone, Debug)]
struct LogicalUse {
    selector: BindingSelector,
    channel: String,
    signal: String,
    operation: BusAccessOperation,
    direction: Option<PortDirection>,
    value_type: IrType,
    lane: BusLaneUse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BusLaneUse {
    None,
    Constant(u64),
    Dynamic,
}

pub fn analyze(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
) -> Result<BusAccessPlan, BusAccessPlanError> {
    let mut accesses = Vec::new();
    for function in &prog.functions {
        let uses = collect_function_uses(prog, function)?;
        for logical in uses {
            for context in binding_contexts(prog, function, &logical.selector)? {
                let physical = physical_name(context.binding, &logical.channel, &logical.signal);
                let port = interface.port_by_physical_name(&physical).ok_or_else(|| {
                    BusAccessPlanError(format!(
                        "bus access in fn{} `{}` for test `{}` maps `{}.{}` to DUT port `{physical}`, which is absent from the interface catalog",
                        function.id.0,
                        function.name,
                        context.test_name,
                        logical.channel,
                        logical.signal,
                    ))
                })?;
                validate_physical_access(prog, function, &logical, port, &physical)?;
                let entry = BusAccessEntry {
                    test: context.test,
                    test_name: context.test_name.to_string(),
                    testbench: context.testbench,
                    function: function.id,
                    function_name: function.name.clone(),
                    binding: context.binding.id,
                    field: context.binding.field.clone(),
                    bus: context.binding.bus.clone(),
                    channel: logical.channel.clone(),
                    signal: logical.signal.clone(),
                    physical_port: physical,
                    operation: logical.operation,
                    direction: logical.direction,
                    value_type: logical.value_type.clone(),
                };
                if !accesses.contains(&entry) {
                    accesses.push(entry);
                }
            }
        }
    }
    collect_target_actor_accesses(prog, interface, &mut accesses)?;
    accesses.sort_by(|lhs, rhs| {
        (
            lhs.test_name.as_str(),
            lhs.function_name.as_str(),
            lhs.field.as_str(),
            lhs.channel.as_str(),
            lhs.signal.as_str(),
            lhs.operation,
        )
            .cmp(&(
                rhs.test_name.as_str(),
                rhs.function_name.as_str(),
                rhs.field.as_str(),
                rhs.channel.as_str(),
                rhs.signal.as_str(),
                rhs.operation,
            ))
    });
    Ok(BusAccessPlan { accesses })
}

fn collect_target_actor_accesses(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
    accesses: &mut Vec<BusAccessEntry>,
) -> Result<(), BusAccessPlanError> {
    for test in &prog.tests {
        let testbench = prog
            .testbenches
            .get(test.testbench.index())
            .ok_or_else(|| {
                BusAccessPlanError(format!(
                    "test `{}` references missing testbench tb{}",
                    test.name, test.testbench.0
                ))
            })?;
        for actor in &testbench.target_tlm_actors {
            let binding = testbench
                .bus_bindings
                .iter()
                .find(|binding| binding.field == actor.bus_field)
                .ok_or_else(|| {
                    BusAccessPlanError(format!(
                        "test `{}` target responder `{}` has no concrete bus binding `{}`",
                        test.name, actor.instance, actor.bus_field
                    ))
                })?;
            let transactor = prog
                .transactors
                .get(actor.transactor.index())
                .ok_or_else(|| {
                    BusAccessPlanError(format!(
                        "test `{}` target responder `{}` references missing transactor x{}",
                        test.name, actor.instance, actor.transactor.0
                    ))
                })?;
            for method in &transactor.target_methods {
                if !actor.active && matches!(method.activation, crate::ir::Activation::ActiveOnly) {
                    continue;
                }
                let function = prog.functions.get(method.function.index()).ok_or_else(|| {
                    BusAccessPlanError(format!(
                        "test `{}` target responder `{}` method `{}` references missing function fn{}",
                        test.name, actor.instance, method.name, method.function.0
                    ))
                })?;
                let bus_method = binding
                    .methods
                    .iter()
                    .find(|candidate| candidate.name == method.name)
                    .ok_or_else(|| {
                        BusAccessPlanError(format!(
                            "test `{}` target responder `{}` serves missing bus method `{}.{}`",
                            test.name, actor.instance, binding.field, method.name
                        ))
                    })?;
                let mut signals = method
                    .args
                    .iter()
                    .cloned()
                    .zip(bus_method.arg_types.iter().cloned())
                    .map(|(signal, ty)| (signal, ty, BusAccessOperation::Read, PortDirection::Out))
                    .collect::<Vec<_>>();
                signals.extend([
                    (
                        "req_valid".to_string(),
                        IrType::Bool,
                        BusAccessOperation::Read,
                        PortDirection::Out,
                    ),
                    (
                        "req_ready".to_string(),
                        IrType::Bool,
                        BusAccessOperation::Write,
                        PortDirection::In,
                    ),
                    (
                        "rsp_valid".to_string(),
                        IrType::Bool,
                        BusAccessOperation::Write,
                        PortDirection::In,
                    ),
                    (
                        "rsp_ready".to_string(),
                        IrType::Bool,
                        BusAccessOperation::Read,
                        PortDirection::Out,
                    ),
                ]);
                if let Some(ret) = &bus_method.ret_type {
                    signals.push((
                        "rsp_data".to_string(),
                        ret.clone(),
                        BusAccessOperation::Write,
                        PortDirection::In,
                    ));
                }
                if let crate::ir::TlmMethodMode::OutOfOrder { tags } = bus_method.mode {
                    let width = (u64::BITS - tags.saturating_sub(1).leading_zeros()).max(1);
                    signals.push((
                        "req_tag".to_string(),
                        IrType::UInt(Some(width)),
                        BusAccessOperation::Read,
                        PortDirection::Out,
                    ));
                    signals.push((
                        "rsp_tag".to_string(),
                        IrType::UInt(Some(width)),
                        BusAccessOperation::Write,
                        PortDirection::In,
                    ));
                }
                for (signal, value_type, operation, direction) in signals {
                    let logical = LogicalUse {
                        selector: BindingSelector::Concrete {
                            binding: binding.id,
                            field: binding.field.clone(),
                        },
                        channel: method.name.clone(),
                        signal: signal.clone(),
                        operation,
                        direction: Some(direction),
                        value_type: value_type.clone(),
                        lane: BusLaneUse::None,
                    };
                    let physical = physical_name(binding, &method.name, &signal);
                    let port = interface.port_by_physical_name(&physical).ok_or_else(|| {
                        BusAccessPlanError(format!(
                            "target responder `{}` in test `{}` maps `{}.{signal}` to DUT port `{physical}`, which is absent from the interface catalog",
                            actor.instance, test.name, method.name
                        ))
                    })?;
                    validate_physical_access(prog, function, &logical, port, &physical)?;
                    let entry = BusAccessEntry {
                        test: test.id,
                        test_name: test.name.clone(),
                        testbench: test.testbench,
                        function: method.function,
                        function_name: function.name.clone(),
                        binding: binding.id,
                        field: binding.field.clone(),
                        bus: binding.bus.clone(),
                        channel: method.name.clone(),
                        signal,
                        physical_port: physical,
                        operation,
                        direction: Some(direction),
                        value_type,
                    };
                    if !accesses.contains(&entry) {
                        accesses.push(entry);
                    }
                }
            }
        }
    }
    Ok(())
}

fn collect_function_uses(
    prog: &TbProgram,
    function: &TbFunction,
) -> Result<Vec<LogicalUse>, BusAccessPlanError> {
    let mut uses = Vec::new();
    for block in &function.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::DutWrite(port, _) => {
                    add_port_use(&mut uses, port, BusAccessOperation::Write)?;
                }
                Stmt::DutRead(_, port) => {
                    add_port_use(&mut uses, port, BusAccessOperation::Read)?;
                }
                Stmt::TlmFork(desc) => add_tlm_uses(prog, function, &mut uses, desc)?,
                Stmt::TlmJoinAll(pending) => {
                    for desc in pending {
                        add_tlm_uses(prog, function, &mut uses, desc)?;
                    }
                }
                _ => {}
            }
            crate::ir::visit::try_visit_stmt_exprs(stmt, &mut |expr| {
                crate::ir::visit::try_walk_expr(expr, &mut |node| match node {
                    Expr::Port(port) => add_port_use(&mut uses, port, BusAccessOperation::Read),
                    Expr::PortSnapshotLane { port, .. } => add_port_use_with_lane(
                        &mut uses,
                        port,
                        BusAccessOperation::Read,
                        BusLaneUse::Dynamic,
                    ),
                    Expr::Call(
                        crate::ir::CallTarget::TransactorMethod {
                            bus_field,
                            method,
                            target,
                        },
                        _,
                    ) => add_tlm_use(prog, function, &mut uses, bus_field, method, target),
                    _ => Ok(()),
                })
            })?;
        }
        crate::ir::visit::try_visit_terminator_exprs(&block.terminator, &mut |expr| {
            crate::ir::visit::try_walk_expr(expr, &mut |node| match node {
                Expr::Port(port) => add_port_use(&mut uses, port, BusAccessOperation::Read),
                Expr::PortSnapshotLane { port, .. } => add_port_use_with_lane(
                    &mut uses,
                    port,
                    BusAccessOperation::Read,
                    BusLaneUse::Dynamic,
                ),
                Expr::Call(
                    crate::ir::CallTarget::TransactorMethod {
                        bus_field,
                        method,
                        target,
                    },
                    _,
                ) => add_tlm_use(prog, function, &mut uses, bus_field, method, target),
                _ => Ok(()),
            })
        })?;
    }
    Ok(uses)
}

fn add_port_use(
    uses: &mut Vec<LogicalUse>,
    port: &PortRef,
    operation: BusAccessOperation,
) -> Result<(), BusAccessPlanError> {
    let lane = match port.lane.as_ref() {
        None => BusLaneUse::None,
        Some(LaneIndex::Const(index)) => BusLaneUse::Constant(*index),
        Some(LaneIndex::Var(_)) => BusLaneUse::Dynamic,
    };
    add_port_use_with_lane(uses, port, operation, lane)
}

fn add_port_use_with_lane(
    uses: &mut Vec<LogicalUse>,
    port: &PortRef,
    operation: BusAccessOperation,
    lane: BusLaneUse,
) -> Result<(), BusAccessPlanError> {
    let selector = match &port.origin {
        PortOrigin::Dut => return Ok(()),
        PortOrigin::BusBinding { binding, field } => BindingSelector::Concrete {
            binding: *binding,
            field: field.clone(),
        },
        PortOrigin::BoundBus => BindingSelector::Bound { field: None },
    };
    let (channel, signal) = logical_port_path(port)?;
    let value_type = port
        .value_type
        .clone()
        .or_else(|| Some(IrType::UInt(port.width)))
        .ok_or_else(|| BusAccessPlanError("bus access has no logical value type".to_string()))?;
    uses.push(LogicalUse {
        selector,
        channel,
        signal,
        operation,
        direction: port.direction,
        value_type,
        lane,
    });
    Ok(())
}

fn logical_port_path(port: &PortRef) -> Result<(String, String), BusAccessPlanError> {
    match port.port_path.as_slice() {
        [_, channel, signal] => Ok((channel.clone(), signal.clone())),
        [_, signal] | [signal] => Ok((String::new(), signal.clone())),
        _ => Err(BusAccessPlanError(format!(
            "bus-bound access has malformed logical path `{}`",
            port.port_path.join(".")
        ))),
    }
}

fn add_tlm_uses(
    prog: &TbProgram,
    function: &TbFunction,
    uses: &mut Vec<LogicalUse>,
    desc: &crate::ir::TlmForkDesc,
) -> Result<(), BusAccessPlanError> {
    add_tlm_use(
        prog,
        function,
        uses,
        &desc.bus_field,
        &desc.method,
        &desc.target,
    )
}

fn add_tlm_use(
    prog: &TbProgram,
    function: &TbFunction,
    uses: &mut Vec<LogicalUse>,
    bus_field: &str,
    method: &str,
    target: &TransactorMethodTarget,
) -> Result<(), BusAccessPlanError> {
    let selector = match target {
        TransactorMethodTarget::ConcreteBusBinding { binding, field } => {
            BindingSelector::Concrete {
                binding: *binding,
                field: field.clone(),
            }
        }
        TransactorMethodTarget::TestbenchBusField {
            testbench,
            field,
            bus,
        } => BindingSelector::TestbenchField {
            testbench: *testbench,
            field: field.clone(),
            bus: bus.clone(),
        },
        TransactorMethodTarget::BoundBus => BindingSelector::Bound {
            field: Some(bus_field.to_string()),
        },
        TransactorMethodTarget::Callable { .. } => return Ok(()),
    };
    let schema = tlm_schema_for_selector(prog, function, &selector, bus_field, method)?;
    for (name, ty) in schema.args.iter().zip(&schema.arg_types) {
        uses.push(LogicalUse {
            selector: selector.clone(),
            channel: method.to_string(),
            signal: name.clone(),
            operation: BusAccessOperation::Write,
            direction: Some(PortDirection::In),
            value_type: ty.clone(),
            lane: BusLaneUse::None,
        });
    }
    for (signal, operation) in [
        ("req_valid", BusAccessOperation::Write),
        ("req_ready", BusAccessOperation::Read),
        ("rsp_ready", BusAccessOperation::Write),
        ("rsp_valid", BusAccessOperation::Read),
    ] {
        uses.push(LogicalUse {
            selector: selector.clone(),
            channel: method.to_string(),
            signal: signal.to_string(),
            operation,
            direction: Some(match operation {
                BusAccessOperation::Read => PortDirection::Out,
                BusAccessOperation::Write => PortDirection::In,
            }),
            value_type: IrType::Bool,
            lane: BusLaneUse::None,
        });
    }
    if let Some(ret) = &schema.ret_type {
        uses.push(LogicalUse {
            selector: selector.clone(),
            channel: method.to_string(),
            signal: "rsp_data".to_string(),
            operation: BusAccessOperation::Read,
            direction: Some(PortDirection::Out),
            value_type: ret.clone(),
            lane: BusLaneUse::None,
        });
    }
    if let crate::ir::TlmMethodMode::OutOfOrder { tags } = schema.mode {
        let width = (u64::BITS - tags.saturating_sub(1).leading_zeros()).max(1);
        for (signal, operation) in [
            ("req_tag", BusAccessOperation::Write),
            ("rsp_tag", BusAccessOperation::Read),
        ] {
            uses.push(LogicalUse {
                selector: selector.clone(),
                channel: method.to_string(),
                signal: signal.to_string(),
                operation,
                direction: Some(match operation {
                    BusAccessOperation::Read => PortDirection::Out,
                    BusAccessOperation::Write => PortDirection::In,
                }),
                value_type: IrType::UInt(Some(width)),
                lane: BusLaneUse::None,
            });
        }
    }
    Ok(())
}

fn tlm_schema_for_selector<'a>(
    prog: &'a TbProgram,
    function: &TbFunction,
    selector: &BindingSelector,
    bus_field: &str,
    method: &str,
) -> Result<&'a crate::ir::TlmMethodSchema, BusAccessPlanError> {
    let bindings = binding_contexts(prog, function, selector)?;
    let binding = bindings.first().ok_or_else(|| {
        BusAccessPlanError(format!(
            "fn{} `{}` has no concrete binding for `{bus_field}.{method}`",
            function.id.0, function.name
        ))
    })?;
    binding
        .binding
        .methods
        .iter()
        .find(|candidate| candidate.name == method)
        .ok_or_else(|| {
            BusAccessPlanError(format!(
                "fn{} `{}` binding `{bus_field}` has no TLM method `{method}`",
                function.id.0, function.name
            ))
        })
}

struct BindingContext<'a> {
    test: TestId,
    test_name: &'a str,
    testbench: TestbenchId,
    binding: &'a BusBindingSchema,
}

fn binding_contexts<'a>(
    prog: &'a TbProgram,
    function: &TbFunction,
    selector: &BindingSelector,
) -> Result<Vec<BindingContext<'a>>, BusAccessPlanError> {
    match selector {
        BindingSelector::TestbenchField {
            testbench,
            field,
            bus,
        } => {
            let mut contexts = Vec::new();
            for test in &prog.tests {
                let schema = prog
                    .testbenches
                    .get(test.testbench.index())
                    .ok_or_else(|| {
                        BusAccessPlanError(format!(
                            "test `{}` references missing testbench tb{}",
                            test.name, test.testbench.0
                        ))
                    })?;
                if schema.type_id != *testbench {
                    continue;
                }
                let binding = schema
                    .bus_bindings
                    .iter()
                    .find(|binding| binding.field == *field && binding.bus == *bus)
                    .ok_or_else(|| {
                        BusAccessPlanError(format!(
                            "test `{}` has no `{bus}` binding for reusable field `{field}` used by fn{} `{}`",
                            test.name, function.id.0, function.name
                        ))
                    })?;
                contexts.push(BindingContext {
                    test: test.id,
                    test_name: &test.name,
                    testbench: test.testbench,
                    binding,
                });
            }
            Ok(contexts)
        }
        BindingSelector::Concrete { binding, field } => {
            if let FunctionKind::TestbenchMethod { testbench, .. } = function.kind {
                let mut contexts = Vec::new();
                for test in &prog.tests {
                    let schema = prog
                        .testbenches
                        .get(test.testbench.index())
                        .ok_or_else(|| {
                            BusAccessPlanError(format!(
                                "test `{}` references missing testbench tb{}",
                                test.name, test.testbench.0
                            ))
                        })?;
                    if schema.type_id != testbench {
                        continue;
                    }
                    let resolved = schema
                        .bus_bindings
                        .iter()
                        .find(|candidate| candidate.field == *field)
                        .ok_or_else(|| {
                            BusAccessPlanError(format!(
                                "test `{}` has no binding for reusable field `{field}` used by fn{} `{}`",
                                test.name, function.id.0, function.name
                            ))
                        })?;
                    contexts.push(BindingContext {
                        test: test.id,
                        test_name: &test.name,
                        testbench: test.testbench,
                        binding: resolved,
                    });
                }
                return Ok(contexts);
            }
            let owner = function.owner.ok_or_else(|| {
                BusAccessPlanError(format!(
                    "fn{} `{}` has a concrete bus access without a testbench owner",
                    function.id.0, function.name
                ))
            })?;
            let schema = prog.testbenches.get(owner.index()).ok_or_else(|| {
                BusAccessPlanError(format!(
                    "fn{} `{}` references missing testbench tb{}",
                    function.id.0, function.name, owner.0
                ))
            })?;
            let resolved = schema
                .bus_binding(*binding)
                .filter(|candidate| candidate.field == *field)
                .ok_or_else(|| {
                    BusAccessPlanError(format!(
                        "fn{} `{}` references missing concrete bus binding bb{} `{field}`",
                        function.id.0, function.name, binding.0
                    ))
                })?;
            contexts_for_testbench(prog, owner, resolved)
        }
        BindingSelector::Bound { field } => {
            let owner = match function.kind {
                FunctionKind::TransactorBody { transactor, .. } => {
                    crate::ir::BoundBusOwner::Transactor(transactor)
                }
                FunctionKind::ComponentMethod { component, .. } => {
                    crate::ir::BoundBusOwner::Component(component)
                }
                _ => {
                    return Err(BusAccessPlanError(format!(
                        "fn{} `{}` uses a bound bus without a transactor/component owner",
                        function.id.0, function.name
                    )))
                }
            };
            let mut contexts = Vec::new();
            for test in &prog.tests {
                let schema = prog
                    .testbenches
                    .get(test.testbench.index())
                    .ok_or_else(|| {
                        BusAccessPlanError(format!(
                            "test `{}` references missing testbench tb{}",
                            test.name, test.testbench.0
                        ))
                    })?;
                if let Some(field) = field {
                    let owns_target = match owner {
                        crate::ir::BoundBusOwner::Transactor(transactor) => schema
                            .target_tlm_actors
                            .iter()
                            .any(|actor| actor.transactor == transactor),
                        crate::ir::BoundBusOwner::Component(_) => false,
                    };
                    if owns_target {
                        if let Some(binding) = schema
                            .bus_bindings
                            .iter()
                            .find(|binding| binding.field == *field)
                        {
                            contexts.push(BindingContext {
                                test: test.id,
                                test_name: &test.name,
                                testbench: test.testbench,
                                binding,
                            });
                            continue;
                        }
                    }
                }
                for instance in schema
                    .bound_bus_instances
                    .iter()
                    .filter(|instance| instance.owner == owner)
                {
                    let binding = schema.bus_binding(instance.binding).ok_or_else(|| {
                        BusAccessPlanError(format!(
                            "test `{}` bound owner {owner:?} references missing binding bb{}",
                            test.name, instance.binding.0
                        ))
                    })?;
                    contexts.push(BindingContext {
                        test: test.id,
                        test_name: &test.name,
                        testbench: test.testbench,
                        binding,
                    });
                }
            }
            Ok(contexts)
        }
    }
}

fn contexts_for_testbench<'a>(
    prog: &'a TbProgram,
    testbench: TestbenchId,
    binding: &'a BusBindingSchema,
) -> Result<Vec<BindingContext<'a>>, BusAccessPlanError> {
    let contexts = prog
        .tests
        .iter()
        .filter(|test| test.testbench == testbench)
        .map(|test| BindingContext {
            test: test.id,
            test_name: &test.name,
            testbench,
            binding,
        })
        .collect::<Vec<_>>();
    if contexts.is_empty() {
        return Err(BusAccessPlanError(format!(
            "testbench tb{} owns bus binding `{}` but no test references it",
            testbench.0, binding.field
        )));
    }
    Ok(contexts)
}

fn physical_name(binding: &BusBindingSchema, channel: &str, signal: &str) -> String {
    if channel.is_empty() {
        format!("{}_{}", binding.field, signal)
    } else {
        binding.wire_name(channel, signal)
    }
}

fn validate_physical_access(
    prog: &TbProgram,
    function: &TbFunction,
    logical: &LogicalUse,
    port: &DutInterfacePort,
    physical: &str,
) -> Result<(), BusAccessPlanError> {
    let expected_direction = logical.direction.unwrap_or(match logical.operation {
        BusAccessOperation::Read => port.direction(),
        BusAccessOperation::Write => PortDirection::In,
    });
    if logical.operation == BusAccessOperation::Write
        && logical.direction == Some(PortDirection::Out)
    {
        return Err(BusAccessPlanError(format!(
            "bus access in fn{} `{}` writes target-driven signal `{}.{}`",
            function.id.0, function.name, logical.channel, logical.signal
        )));
    }
    if port.direction() != expected_direction {
        return Err(BusAccessPlanError(format!(
            "bus access in fn{} `{}` maps `{}.{}` {:?} to DUT port `{physical}` with direction {:?}, expected {:?}",
            function.id.0,
            function.name,
            logical.channel,
            logical.signal,
            logical.operation,
            port.direction(),
            expected_direction,
        )));
    }
    let actual = if logical.lane != BusLaneUse::None {
        port.packed_lane_type()
            .cloned()
            .or_else(|| {
                port.packed_lane_width()
                    .map(|width| IrType::UInt(Some(width)))
            })
            .ok_or_else(|| {
                BusAccessPlanError(format!(
                    "bus access in fn{} `{}` indexes scalar DUT port `{physical}`",
                    function.id.0, function.name
                ))
            })?
    } else {
        port.value_type().clone()
    };
    if !wire_type_compatible(prog, &logical.value_type, &actual) {
        return Err(BusAccessPlanError(format!(
            "bus access in fn{} `{}` maps `{}.{}` type {:?} to DUT port `{physical}` type {actual:?}",
            function.id.0,
            function.name,
            logical.channel,
            logical.signal,
            logical.value_type,
        )));
    }
    if logical.lane != BusLaneUse::None
        && port.packed_lane_width().is_none()
        && port.unpacked_elements().is_none()
    {
        return Err(BusAccessPlanError(format!(
            "bus access in fn{} `{}` indexes scalar DUT port `{physical}`",
            function.id.0, function.name
        )));
    }
    if port.packed_lane_width().is_some_and(|width| width > 64) && logical.lane != BusLaneUse::None
    {
        return Err(BusAccessPlanError(format!(
            "bus access in fn{} `{}` uses {physical} with a packed lane width greater than 64 bits",
            function.id.0, function.name
        )));
    }
    if let BusLaneUse::Constant(index) = logical.lane {
        let elements = port.unpacked_elements().or_else(|| {
            port.packed_lane_width()
                .and_then(|lane| port.resolved_width()?.checked_div(lane))
        });
        if elements.is_some_and(|elements| index >= u64::from(elements)) {
            return Err(BusAccessPlanError(format!(
                "bus access in fn{} `{}` indexes DUT port `{physical}` at {index}, outside its {elements:?}-element shape",
                function.id.0, function.name
            )));
        }
    }
    Ok(())
}

fn wire_type_compatible(prog: &TbProgram, expected: &IrType, actual: &IrType) -> bool {
    match (expected, actual) {
        (IrType::Bool, IrType::Bool | IrType::UInt(Some(1)))
        | (IrType::UInt(Some(1)), IrType::Bool) => true,
        (IrType::UInt(Some(lhs)), IrType::UInt(Some(rhs)))
        | (IrType::SInt(Some(lhs)), IrType::SInt(Some(rhs))) => lhs == rhs,
        (IrType::Record(_), IrType::UInt(Some(width)))
        | (IrType::FixedVec { .. }, IrType::UInt(Some(width))) => {
            packed_width(prog, expected) == Some(*width)
        }
        _ => expected == actual,
    }
}

fn packed_width(prog: &TbProgram, ty: &IrType) -> Option<u32> {
    match ty {
        IrType::Bool => Some(1),
        IrType::UInt(width) | IrType::SInt(width) => *width,
        IrType::Record(record) => {
            prog.records
                .get(record.index())?
                .fields
                .iter()
                .try_fold(0u32, |total, field| {
                    let width = packed_width(prog, &field.ty)?;
                    total.checked_add(width.checked_mul(field.vec_len.unwrap_or(1) as u32)?)
                })
        }
        IrType::FixedVec { elem, len } => packed_width(prog, elem)?.checked_mul(*len as u32),
        _ => None,
    }
}
