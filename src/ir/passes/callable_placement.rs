//! Whole-program callable ownership and common-object placement analysis.

use crate::ir::{
    BusBindingId, ComponentBase, ComponentId, ConnectEdgeSchema, ConnectSink, EventChannelRef,
    Expr, FunctionId, FunctionKind, MethodHookTarget, Stmt, TbFunction, TbProgram, Terminator,
    TestCallableMember, TestId, TestSchema, TestbenchId, TestbenchSchema, TestbenchTypeId,
    TransactorId, TransactorMethodTarget,
};
use std::collections::{BTreeSet, VecDeque};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ComponentCallableMember {
    Method(usize),
    OnHandler(usize),
    PeriodicHandler(usize),
    CycleHandler(usize),
    Watchdog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TransactorCallableMember {
    Method(usize),
    TargetMethod(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CallableOwner {
    Suite,
    Test {
        id: TestId,
        test: String,
        testbench: TestbenchId,
    },
    TestbenchType(TestbenchTypeId),
    Component {
        component: ComponentId,
        member: ComponentCallableMember,
    },
    Transactor {
        transactor: TransactorId,
        member: TransactorCallableMember,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CapsulePlacementReason {
    TestBody,
    TestHook,
    TargetResponder,
    ConcreteBusBinding { binding: BusBindingId },
    LifecycleService,
    Dependency { function: FunctionId },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InvalidPlacementReason {
    MissingOwner,
    MultipleOwners {
        count: usize,
    },
    OwnerKindMismatch,
    MissingDependency {
        target: String,
    },
    UnsupportedTransactorBody,
    UnsupportedLifecycleHandler,
    RegblockState {
        testbench: TestbenchId,
        callback_bearing: bool,
    },
    MissingConcreteBusBinding,
    ConflictingBusBindings {
        first_test: String,
        first_binding: BusBindingId,
        second_test: String,
        second_binding: BusBindingId,
    },
    ConflictingCapsules {
        first_test: String,
        second_test: String,
        dependency: FunctionId,
    },
    InvalidDependency {
        function: FunctionId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallablePlacement {
    Common,
    CapsuleLocal {
        test: String,
        reason: CapsulePlacementReason,
    },
    CapsuleScoped {
        reason: CapsulePlacementReason,
    },
    Invalid {
        reason: InvalidPlacementReason,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallableKind {
    Run,
    Check,
    SamplerAuto,
    Helper,
    TestbenchMethod,
    ComponentMethod,
    TransactorMethod,
    Tseq,
    TestHook,
    TestbenchLifecycle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallablePlan {
    pub function: FunctionId,
    pub name: String,
    pub kind: CallableKind,
    pub owner: CallableOwner,
    pub placement: CallablePlacement,
    pub dependencies: Vec<FunctionId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallableCatalog {
    callables: Vec<CallablePlan>,
}

impl CallableCatalog {
    pub fn callables(&self) -> &[CallablePlan] {
        &self.callables
    }

    pub fn callable(&self, function: FunctionId) -> Option<&CallablePlan> {
        self.callables
            .get(function.index())
            .filter(|entry| entry.function == function)
    }

    /// Return one deterministic callable chain from `root` to a body that
    /// advances simulation time. The callable graph is the same typed graph
    /// used for common-object placement, so helper, TSeq, testbench,
    /// component, and transactor edges cannot drift between analyses.
    pub fn advance_time_chain(
        &self,
        prog: &TbProgram,
        root: FunctionId,
    ) -> Result<Option<Vec<FunctionId>>, PlacementError> {
        let root = self.callable(root).ok_or_else(|| {
            PlacementError(format!(
                "time-effect analysis references missing callable fn{}",
                root.0
            ))
        })?;
        let mut queue = std::collections::VecDeque::from([vec![root.function]]);
        let mut visited = BTreeSet::new();
        while let Some(path) = queue.pop_front() {
            let function = *path.last().expect("effect path is never empty");
            if !visited.insert(function) {
                continue;
            }
            let body = prog
                .functions
                .get(function.index())
                .filter(|body| body.id == function)
                .ok_or_else(|| {
                    PlacementError(format!(
                        "time-effect analysis references missing callable fn{}",
                        function.0
                    ))
                })?;
            if body.blocks.iter().any(|block| {
                matches!(
                    block.terminator,
                    Terminator::WaitCycles(..)
                        | Terminator::WaitCyclesSync(..)
                        | Terminator::WaitUntil { .. }
                        | Terminator::WaitUntilTimeout { .. }
                        | Terminator::WaitTimePs(..)
                )
            }) {
                return Ok(Some(path));
            }
            let callable = self.callable(function).ok_or_else(|| {
                PlacementError(format!(
                    "time-effect analysis references uncatalogued callable fn{}",
                    function.0
                ))
            })?;
            for dependency in &callable.dependencies {
                let mut dependency_path = path.clone();
                dependency_path.push(*dependency);
                queue.push_back(dependency_path);
            }
        }
        Ok(None)
    }

    /// Return one deterministic effect chain from `root` to a synchronous
    /// operation that advances simulation time while `test` is selected.
    /// In addition to ordinary callable edges, this follows the runtime
    /// fanout installed by that test's event and method-hook subscriptions.
    pub fn advance_time_chain_for_test(
        &self,
        prog: &TbProgram,
        root: FunctionId,
        test: &TestSchema,
    ) -> Result<Option<Vec<String>>, PlacementError> {
        let root_callable = self.callable(root).ok_or_else(|| {
            PlacementError(format!(
                "time-effect analysis references missing callable fn{}",
                root.0
            ))
        })?;
        let root_receivers = effect_root_receivers(prog, test, root_callable)?;
        let mut queue = root_receivers
            .into_iter()
            .map(|receiver| {
                vec![EffectNode {
                    function: root_callable.function,
                    receiver,
                }]
            })
            .collect::<VecDeque<_>>();
        let mut visited = BTreeSet::new();
        while let Some(path) = queue.pop_front() {
            let node = path.last().expect("effect path is never empty");
            if !visited.insert(node.clone()) {
                continue;
            }
            let body = prog
                .functions
                .get(node.function.index())
                .filter(|body| body.id == node.function)
                .ok_or_else(|| {
                    PlacementError(format!(
                        "time-effect analysis references missing callable fn{}",
                        node.function.0
                    ))
                })?;
            if body.blocks.iter().any(|block| {
                matches!(
                    block.terminator,
                    Terminator::WaitCycles(..)
                        | Terminator::WaitCyclesSync(..)
                        | Terminator::WaitUntil { .. }
                        | Terminator::WaitUntilTimeout { .. }
                        | Terminator::WaitTimePs(..)
                )
            }) {
                return Ok(Some(effect_node_path_names(prog, &path)));
            }
            if let Some(effect) = intrinsic_time_advance(body)? {
                let mut names = effect_node_path_names(prog, &path);
                names.push(effect.to_string());
                return Ok(Some(names));
            }
            let dependencies = effect_dependencies(prog, body, test, &node.receiver)?;
            for dependency in dependencies {
                let mut dependency_path = path.clone();
                dependency_path.push(dependency);
                queue.push_back(dependency_path);
            }
        }
        Ok(None)
    }

    pub fn testbench_method_order(
        &self,
        testbench: TestbenchTypeId,
    ) -> Result<Vec<FunctionId>, PlacementError> {
        self.dependency_order(
            |entry| {
                entry.owner == CallableOwner::TestbenchType(testbench)
                    && entry.kind == CallableKind::TestbenchMethod
            },
            "testbench method",
        )
    }

    pub fn component_method_order(&self) -> Result<Vec<FunctionId>, PlacementError> {
        self.dependency_order(
            |entry| {
                matches!(
                    entry.owner,
                    CallableOwner::Component {
                        member: ComponentCallableMember::Method(_),
                        ..
                    }
                )
            },
            "component method",
        )
    }

    fn dependency_order(
        &self,
        include: impl Fn(&CallablePlan) -> bool,
        label: &str,
    ) -> Result<Vec<FunctionId>, PlacementError> {
        let selected = self.callables.iter().map(include).collect::<Vec<_>>();
        let mut state = vec![0u8; self.callables.len()];
        let mut stack = Vec::new();
        let mut order = Vec::new();

        fn visit(
            catalog: &CallableCatalog,
            selected: &[bool],
            label: &str,
            node: usize,
            state: &mut [u8],
            stack: &mut Vec<usize>,
            order: &mut Vec<FunctionId>,
        ) -> Result<(), PlacementError> {
            match state[node] {
                2 => return Ok(()),
                1 => {
                    let start = stack.iter().position(|entry| *entry == node).unwrap_or(0);
                    let mut cycle = stack[start..]
                        .iter()
                        .map(|entry| catalog.callables[*entry].name.as_str())
                        .collect::<Vec<_>>();
                    cycle.push(catalog.callables[node].name.as_str());
                    return Err(PlacementError(format!(
                        "{label} dependency cycle: {}",
                        cycle.join(" -> ")
                    )));
                }
                _ => {}
            }
            state[node] = 1;
            stack.push(node);
            for dependency in &catalog.callables[node].dependencies {
                let dependency = dependency.index();
                if selected.get(dependency).copied().unwrap_or(false) {
                    visit(catalog, selected, label, dependency, state, stack, order)?;
                }
            }
            stack.pop();
            state[node] = 2;
            order.push(catalog.callables[node].function);
            Ok(())
        }

        for node in 0..self.callables.len() {
            if selected[node] {
                visit(
                    self, &selected, label, node, &mut state, &mut stack, &mut order,
                )?;
            }
        }
        Ok(order)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct EffectNode {
    function: FunctionId,
    receiver: EffectReceiver,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EffectReceiver {
    None,
    Component {
        component: ComponentId,
        path: Vec<String>,
        mode: Option<crate::ir::ComponentInstanceMode>,
    },
    DetachedComponent {
        component: ComponentId,
        mode: Option<crate::ir::ComponentInstanceMode>,
    },
    Transactor {
        transactor: TransactorId,
        field: String,
        mode: crate::ir::ComponentInstanceMode,
    },
}

fn effect_node_path_names(prog: &TbProgram, path: &[EffectNode]) -> Vec<String> {
    path.iter()
        .map(|node| prog.function(node.function).name.clone())
        .collect()
}

fn intrinsic_time_advance(function: &TbFunction) -> Result<Option<&'static str>, PlacementError> {
    let mut effect = None;
    for block in &function.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::TlmFork(_) => return Ok(Some("forked TLM request")),
                Stmt::TlmJoinAll(_) => return Ok(Some("TLM join")),
                _ => {}
            }
            visit_stmt_exprs(stmt, &mut |expr| {
                if effect.is_none() {
                    effect = intrinsic_expr_time_advance(expr);
                }
                Ok(())
            })?;
            if effect.is_some() {
                return Ok(effect);
            }
        }
        visit_terminator_exprs(&block.terminator, &mut |expr| {
            if effect.is_none() {
                effect = intrinsic_expr_time_advance(expr);
            }
            Ok(())
        })?;
        if effect.is_some() {
            return Ok(effect);
        }
    }
    Ok(None)
}

fn intrinsic_expr_time_advance(expr: &Expr) -> Option<&'static str> {
    if matches!(
        expr,
        Expr::Call(
            crate::ir::CallTarget::TransactorMethod {
                target: TransactorMethodTarget::ConcreteBusBinding { .. }
                    | TransactorMethodTarget::TestbenchBusField { .. }
                    | TransactorMethodTarget::BoundBus,
                ..
            },
            _
        )
    ) {
        return Some("blocking TLM call");
    }
    let mut effect = None;
    visit_expr_children(expr, &mut |child| {
        if effect.is_none() {
            effect = intrinsic_expr_time_advance(child);
        }
        Ok(())
    })
    .expect("intrinsic time-effect expression walk cannot fail");
    effect
}

fn effect_root_receivers(
    prog: &TbProgram,
    test: &TestSchema,
    root: &CallablePlan,
) -> Result<Vec<EffectReceiver>, PlacementError> {
    let body = prog.function(root.function);
    match body.kind {
        FunctionKind::ComponentMethod { component, .. } => {
            let activation = component_function_activation(prog, component, root.function)?;
            Ok(
                concrete_component_instances(prog, testbench_for_test(prog, test)?)?
                    .into_iter()
                    .filter(|receiver| {
                        matches!(
                            receiver,
                            EffectReceiver::Component {
                                component: found,
                                mode,
                                ..
                            } if *found == component
                                && crate::ir::component_mode_includes_activation(*mode, activation)
                        )
                    })
                    .collect(),
            )
        }
        FunctionKind::TransactorBody { transactor, .. } => {
            let testbench = testbench_for_test(prog, test)?;
            Ok(testbench
                .transactor_fields
                .iter()
                .filter(|(_, found)| *found == transactor)
                .map(|(field, _)| EffectReceiver::Transactor {
                    transactor,
                    field: field.clone(),
                    mode: transactor_field_mode(testbench, field),
                })
                .collect())
        }
        _ => Ok(vec![EffectReceiver::None]),
    }
}

fn effect_dependencies(
    prog: &TbProgram,
    function: &TbFunction,
    test: &TestSchema,
    receiver: &EffectReceiver,
) -> Result<BTreeSet<EffectNode>, PlacementError> {
    let mut dependencies = hook_subscribers_for_callable(prog, function, test, receiver)?;
    for block in &function.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::ComponentEmit { base, event, .. } => {
                    let source =
                        component_receiver_for_base(prog, function, test, receiver, base, None)?;
                    dependencies.extend(event_dispatch_dependencies(prog, test, source, event)?);
                }
                Stmt::ComponentCall {
                    base,
                    component,
                    function: target,
                    ..
                } => {
                    dependencies.insert(EffectNode {
                        function: *target,
                        receiver: component_receiver_for_base(
                            prog,
                            function,
                            test,
                            receiver,
                            base,
                            Some(*component),
                        )?,
                    });
                }
                Stmt::TestbenchCall {
                    function: target, ..
                }
                | Stmt::RecordWriteCb {
                    callback: Some(target),
                    ..
                } => {
                    dependencies.insert(EffectNode {
                        function: *target,
                        receiver: EffectReceiver::None,
                    });
                }
                _ => {}
            }
            visit_stmt_exprs(stmt, &mut |expr| {
                collect_effect_expr_dependencies(
                    prog,
                    function,
                    test,
                    receiver,
                    expr,
                    &mut dependencies,
                )
            })?;
        }
        visit_terminator_exprs(&block.terminator, &mut |expr| {
            collect_effect_expr_dependencies(
                prog,
                function,
                test,
                receiver,
                expr,
                &mut dependencies,
            )
        })?;
    }
    Ok(dependencies)
}

fn collect_effect_expr_dependencies(
    prog: &TbProgram,
    function: &TbFunction,
    test: &TestSchema,
    receiver: &EffectReceiver,
    expr: &Expr,
    dependencies: &mut BTreeSet<EffectNode>,
) -> Result<(), PlacementError> {
    if let Expr::Call(target, _) = expr {
        let dependency = match target {
            crate::ir::CallTarget::Helper {
                function: target, ..
            }
            | crate::ir::CallTarget::Tseq {
                function: target, ..
            } => Some(EffectNode {
                function: *target,
                receiver: EffectReceiver::None,
            }),
            crate::ir::CallTarget::TransactorSelfMethod {
                transactor,
                function: target,
                ..
            } => Some(EffectNode {
                function: *target,
                receiver: match receiver {
                    EffectReceiver::Transactor {
                        transactor: found,
                        field,
                        mode,
                    } if found == transactor => EffectReceiver::Transactor {
                        transactor: *transactor,
                        field: field.clone(),
                        mode: *mode,
                    },
                    _ => EffectReceiver::None,
                },
            }),
            crate::ir::CallTarget::TransactorMethod {
                bus_field,
                target:
                    TransactorMethodTarget::Callable {
                        transactor,
                        function: target,
                    },
                ..
            } => Some(EffectNode {
                function: *target,
                receiver: EffectReceiver::Transactor {
                    transactor: *transactor,
                    field: bus_field.clone(),
                    mode: transactor_field_mode(testbench_for_test(prog, test)?, bus_field),
                },
            }),
            crate::ir::CallTarget::Builtin(_)
            | crate::ir::CallTarget::ExternFn { .. }
            | crate::ir::CallTarget::TransactorMethod { .. } => None,
        };
        if let Some(dependency) = dependency {
            dependencies.insert(dependency);
        }
    }
    visit_expr_children(expr, &mut |child| {
        collect_effect_expr_dependencies(prog, function, test, receiver, child, dependencies)
    })
}

fn hook_subscribers_for_callable(
    prog: &TbProgram,
    function: &TbFunction,
    test: &TestSchema,
    receiver: &EffectReceiver,
) -> Result<BTreeSet<EffectNode>, PlacementError> {
    let method = match &function.kind {
        FunctionKind::ComponentMethod { component, .. } => {
            prog.components.get(component.index()).and_then(|schema| {
                schema
                    .methods
                    .iter()
                    .find(|method| method.function == function.id)
                    .map(|method| method.name.as_str())
            })
        }
        FunctionKind::TransactorBody { transactor, .. } => {
            prog.transactors.get(transactor.index()).and_then(|schema| {
                schema
                    .methods
                    .iter()
                    .find(|method| method.function == function.id)
                    .map(|method| method.name.as_str())
            })
        }
        _ => None,
    };
    let Some(method) = method else {
        return Ok(BTreeSet::new());
    };
    let mut subscribers = BTreeSet::new();
    for test_function in selected_test_functions(prog, test)? {
        for block in &test_function.blocks {
            for stmt in &block.stmts {
                let Stmt::MethodHookSubscribe {
                    target, handler, ..
                } = stmt
                else {
                    continue;
                };
                let matches = match (target, receiver) {
                    (
                        MethodHookTarget::Component {
                            base,
                            component,
                            method: target_method,
                        },
                        EffectReceiver::Component { .. },
                    ) if target_method == method => {
                        component_receiver_for_base(
                            prog,
                            test_function,
                            test,
                            &EffectReceiver::None,
                            base,
                            Some(*component),
                        )? == *receiver
                    }
                    (
                        MethodHookTarget::Transactor {
                            field,
                            transactor,
                            method: target_method,
                        },
                        EffectReceiver::Transactor {
                            transactor: receiver_transactor,
                            field: receiver_field,
                            ..
                        },
                    ) => {
                        target_method == method
                            && transactor == receiver_transactor
                            && field == receiver_field
                    }
                    _ => false,
                };
                if matches {
                    subscribers.insert(EffectNode {
                        function: *handler,
                        receiver: EffectReceiver::None,
                    });
                }
            }
        }
    }
    Ok(subscribers)
}

fn event_dispatch_dependencies(
    prog: &TbProgram,
    test: &TestSchema,
    source: EffectReceiver,
    event: &str,
) -> Result<BTreeSet<EffectNode>, PlacementError> {
    let mut dependencies = BTreeSet::new();
    let mut pending = VecDeque::from([(source, event.to_string())]);
    let mut visited = BTreeSet::new();
    let testbench = testbench_for_test(prog, test)?;
    let instances = concrete_component_instances(prog, testbench)?;
    while let Some((source, event)) = pending.pop_front() {
        if !visited.insert((source.clone(), event.clone())) {
            continue;
        }
        let (component, mode) = match &source {
            EffectReceiver::Component {
                component, mode, ..
            }
            | EffectReceiver::DetachedComponent { component, mode } => (*component, *mode),
            _ => {
                return Err(PlacementError(
                    "time-effect analysis found a component event without a component receiver"
                        .to_string(),
                ));
            }
        };
        let schema = prog.components.get(component.index()).ok_or_else(|| {
            PlacementError(format!(
                "time-effect analysis references missing component c{}",
                component.0
            ))
        })?;
        dependencies.extend(
            schema
                .on_handlers
                .iter()
                .filter(|handler| {
                    handler.event == event
                        && crate::ir::component_mode_includes_activation(mode, handler.activation)
                })
                .map(|handler| EffectNode {
                    function: handler.function,
                    receiver: source.clone(),
                }),
        );
        if matches!(source, EffectReceiver::DetachedComponent { .. }) {
            continue;
        }
        for test_function in selected_test_functions(prog, test)? {
            for block in &test_function.blocks {
                for stmt in &block.stmts {
                    if let Stmt::EventSubscribe {
                        event:
                            EventChannelRef::Component {
                                base,
                                component: target,
                                event: target_event,
                                ..
                            },
                        handler,
                        ..
                    } = stmt
                    {
                        let subscribed = component_receiver_for_base(
                            prog,
                            test_function,
                            test,
                            &EffectReceiver::None,
                            base,
                            Some(*target),
                        )?;
                        if subscribed == source && *target_event == event {
                            dependencies.insert(EffectNode {
                                function: *handler,
                                receiver: EffectReceiver::None,
                            });
                        }
                    }
                }
            }
        }
        for edge in &testbench.connects {
            let edge_source = testbench_connect_endpoint(prog, testbench, &edge.src_path)?;
            let edge_sink = testbench_connect_endpoint(prog, testbench, &edge.sink_path)?;
            if connect_edge_is_enabled(edge, &edge_source, &edge_sink)
                && edge_source == source
                && edge.src_event == event
            {
                add_connect_sink(prog, edge, edge_sink, &mut dependencies, &mut pending)?;
            }
        }
        for owner in &instances {
            let EffectReceiver::Component {
                component: owner_component,
                ..
            } = owner
            else {
                continue;
            };
            let owner_schema = prog
                .components
                .get(owner_component.index())
                .ok_or_else(|| {
                    PlacementError(format!(
                        "time-effect analysis references missing component c{}",
                        owner_component.0
                    ))
                })?;
            for edge in &owner_schema.connects {
                let edge_source = component_connect_endpoint(prog, owner, &edge.src_path)?;
                let edge_sink = component_connect_endpoint(prog, owner, &edge.sink_path)?;
                if connect_edge_is_enabled(edge, &edge_source, &edge_sink)
                    && edge_source == source
                    && edge.src_event == event
                {
                    add_connect_sink(prog, edge, edge_sink, &mut dependencies, &mut pending)?;
                }
            }
        }
    }
    Ok(dependencies)
}

fn add_connect_sink(
    prog: &TbProgram,
    edge: &ConnectEdgeSchema,
    sink_receiver: EffectReceiver,
    dependencies: &mut BTreeSet<EffectNode>,
    pending: &mut VecDeque<(EffectReceiver, String)>,
) -> Result<(), PlacementError> {
    let sink = prog
        .components
        .get(edge.sink_component.index())
        .ok_or_else(|| {
            PlacementError(format!(
                "time-effect analysis connect references missing component c{}",
                edge.sink_component.0
            ))
        })?;
    match &edge.sink {
        ConnectSink::Method { method } => {
            let function = sink
                .methods
                .iter()
                .find(|candidate| candidate.name == *method)
                .map(|candidate| candidate.function)
                .ok_or_else(|| {
                    PlacementError(format!(
                        "time-effect analysis connect references missing method `{method}` on component `{}`",
                        sink.name
                    ))
                })?;
            dependencies.insert(EffectNode {
                function,
                receiver: sink_receiver,
            });
        }
        ConnectSink::Event { event } => {
            pending.push_back((sink_receiver, event.clone()));
        }
    }
    Ok(())
}

fn connect_edge_is_enabled(
    edge: &ConnectEdgeSchema,
    source: &EffectReceiver,
    sink: &EffectReceiver,
) -> bool {
    crate::ir::component_connect_modes_enabled(
        component_receiver_mode(source),
        component_receiver_mode(sink),
        edge,
    )
}

fn component_receiver_mode(receiver: &EffectReceiver) -> Option<crate::ir::ComponentInstanceMode> {
    match receiver {
        EffectReceiver::Component { mode, .. } | EffectReceiver::DetachedComponent { mode, .. } => {
            *mode
        }
        _ => None,
    }
}

fn component_function_activation(
    prog: &TbProgram,
    component: ComponentId,
    function: FunctionId,
) -> Result<crate::ir::Activation, PlacementError> {
    let schema = prog.components.get(component.index()).ok_or_else(|| {
        PlacementError(format!(
            "time-effect analysis references missing component c{}",
            component.0
        ))
    })?;
    schema
        .methods
        .iter()
        .find(|member| member.function == function)
        .map(|member| member.activation)
        .or_else(|| {
            schema
                .on_handlers
                .iter()
                .find(|member| member.function == function)
                .map(|member| member.activation)
        })
        .or_else(|| {
            schema
                .periodic_handlers
                .iter()
                .find(|member| member.function == function)
                .map(|member| member.activation)
        })
        .or_else(|| {
            schema
                .cycle_handlers
                .iter()
                .find(|member| member.function == function)
                .map(|member| member.activation)
        })
        .or_else(|| {
            schema
                .watchdog
                .as_ref()
                .filter(|member| member.function == function)
                .map(|member| member.activation)
        })
        .ok_or_else(|| {
            PlacementError(format!(
                "time-effect analysis component `{}` does not own fn{}",
                schema.name, function.0
            ))
        })
}

fn component_local_mode(
    function: &TbFunction,
    local: crate::ir::LocalId,
) -> Option<crate::ir::ComponentInstanceMode> {
    function
        .blocks
        .iter()
        .flat_map(|block| &block.stmts)
        .find_map(|stmt| match stmt {
            Stmt::ComponentInit {
                local: initialized,
                mode,
                ..
            } if *initialized == local => *mode,
            _ => None,
        })
}

fn transactor_field_mode(
    testbench: &TestbenchSchema,
    field: &str,
) -> crate::ir::ComponentInstanceMode {
    if testbench.passive_transactor_fields.contains(field) {
        crate::ir::ComponentInstanceMode::Passive
    } else {
        crate::ir::ComponentInstanceMode::Active
    }
}

fn selected_test_functions<'a>(
    prog: &'a TbProgram,
    test: &TestSchema,
) -> Result<Vec<&'a TbFunction>, PlacementError> {
    [Some(test.run), test.check]
        .into_iter()
        .flatten()
        .map(|function| {
            prog.functions
                .get(function.index())
                .filter(|body| body.id == function)
                .ok_or_else(|| {
                    PlacementError(format!(
                        "time-effect analysis test `{}` references missing fn{}",
                        test.name, function.0
                    ))
                })
        })
        .collect()
}

fn testbench_for_test<'a>(
    prog: &'a TbProgram,
    test: &TestSchema,
) -> Result<&'a TestbenchSchema, PlacementError> {
    prog.testbenches.get(test.testbench.index()).ok_or_else(|| {
        PlacementError(format!(
            "time-effect analysis test `{}` references missing testbench tb{}",
            test.name, test.testbench.0
        ))
    })
}

fn concrete_component_instances(
    prog: &TbProgram,
    testbench: &TestbenchSchema,
) -> Result<Vec<EffectReceiver>, PlacementError> {
    fn visit(
        prog: &TbProgram,
        component: ComponentId,
        path: Vec<String>,
        mode: Option<crate::ir::ComponentInstanceMode>,
        stack: &mut BTreeSet<ComponentId>,
        out: &mut Vec<EffectReceiver>,
    ) -> Result<(), PlacementError> {
        if !stack.insert(component) {
            return Err(PlacementError(format!(
                "time-effect analysis found a recursive component at `{}`",
                path.join(".")
            )));
        }
        out.push(EffectReceiver::Component {
            component,
            path: path.clone(),
            mode,
        });
        let schema = prog.components.get(component.index()).ok_or_else(|| {
            PlacementError(format!(
                "time-effect analysis references missing component c{}",
                component.0
            ))
        })?;
        for field in &schema.fields {
            if let crate::ir::ComponentFieldKind::Sub {
                component: child, ..
            } = field.kind
            {
                let mut child_path = path.clone();
                child_path.push(field.name.clone());
                let child_mode = crate::ir::resolve_component_path_mode(
                    &prog.components,
                    component,
                    mode,
                    std::slice::from_ref(&field.name),
                )
                .map_err(|error| PlacementError(error.to_string()))?
                .effective_mode;
                visit(prog, child, child_path, child_mode, stack, out)?;
            }
        }
        stack.remove(&component);
        Ok(())
    }

    let mut out = Vec::new();
    for binding in &testbench.component_fields {
        visit(
            prog,
            binding.component,
            vec![binding.field.clone()],
            binding.mode,
            &mut BTreeSet::new(),
            &mut out,
        )?;
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn component_receiver_for_base(
    prog: &TbProgram,
    function: &TbFunction,
    test: &TestSchema,
    receiver: &EffectReceiver,
    base: &ComponentBase,
    expected: Option<ComponentId>,
) -> Result<EffectReceiver, PlacementError> {
    let resolved = match base {
        ComponentBase::SelfField => match receiver {
            EffectReceiver::Component { .. } | EffectReceiver::DetachedComponent { .. } => {
                receiver.clone()
            }
            _ => {
                return Err(PlacementError(format!(
                    "time-effect analysis found a self component without a receiver in fn{}",
                    function.id.0
                )));
            }
        },
        ComponentBase::Local(local) => {
            match function.locals.get(local.index()).map(|slot| &slot.ty) {
                Some(crate::ir::IrType::Component(component)) => {
                    EffectReceiver::DetachedComponent {
                        component: *component,
                        mode: component_local_mode(function, *local),
                    }
                }
                _ => {
                    return Err(PlacementError(format!(
                        "time-effect analysis found an invalid component local %{} in fn{}",
                        local.0, function.id.0
                    )));
                }
            }
        }
        ComponentBase::Path(path) if path.first().is_some_and(|segment| segment == "self") => {
            component_connect_endpoint(prog, receiver, &path[1..])?
        }
        ComponentBase::Path(path) => {
            testbench_connect_endpoint(prog, testbench_for_test(prog, test)?, path)?
        }
    };
    if let Some(expected) = expected {
        let found = match resolved {
            EffectReceiver::Component { component, .. }
            | EffectReceiver::DetachedComponent { component, .. } => component,
            _ => {
                return Err(PlacementError(format!(
                    "time-effect analysis expected component c{} in fn{}",
                    expected.0, function.id.0
                )));
            }
        };
        if found != expected {
            return Err(PlacementError(format!(
                "time-effect analysis component receiver c{} does not match expected c{} in fn{}",
                found.0, expected.0, function.id.0
            )));
        }
    }
    Ok(resolved)
}

fn testbench_connect_endpoint(
    prog: &TbProgram,
    testbench: &TestbenchSchema,
    path: &[String],
) -> Result<EffectReceiver, PlacementError> {
    let (head, tail) = path.split_first().ok_or_else(|| {
        PlacementError(format!(
            "time-effect analysis testbench `{}` has an empty component path",
            testbench.name
        ))
    })?;
    let binding = testbench
        .component_fields
        .iter()
        .find(|binding| binding.field == *head)
        .ok_or_else(|| {
            PlacementError(format!(
                "time-effect analysis testbench `{}` has no component field `{head}`",
                testbench.name
            ))
        })?;
    let resolved = crate::ir::resolve_component_path_mode(
        &prog.components,
        binding.component,
        binding.mode,
        tail,
    )
    .map_err(|error| PlacementError(error.to_string()))?;
    Ok(EffectReceiver::Component {
        component: resolved.component,
        path: path.to_vec(),
        mode: resolved.effective_mode,
    })
}

fn component_connect_endpoint(
    prog: &TbProgram,
    owner: &EffectReceiver,
    path: &[String],
) -> Result<EffectReceiver, PlacementError> {
    match owner {
        EffectReceiver::Component {
            component,
            path: owner_path,
            mode,
        } => {
            let resolved =
                crate::ir::resolve_component_path_mode(&prog.components, *component, *mode, path)
                    .map_err(|error| PlacementError(error.to_string()))?;
            let mut target_path = owner_path.clone();
            target_path.extend_from_slice(path);
            Ok(EffectReceiver::Component {
                component: resolved.component,
                path: target_path,
                mode: resolved.effective_mode,
            })
        }
        EffectReceiver::DetachedComponent { component, mode } => {
            let resolved =
                crate::ir::resolve_component_path_mode(&prog.components, *component, *mode, path)
                    .map_err(|error| PlacementError(error.to_string()))?;
            Ok(EffectReceiver::DetachedComponent {
                component: resolved.component,
                mode: resolved.effective_mode,
            })
        }
        _ => Err(PlacementError(
            "time-effect analysis found a relative component path without a component receiver"
                .to_string(),
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementError(pub String);

impl fmt::Display for PlacementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PlacementError {}

pub fn analyze(prog: &TbProgram) -> Result<CallableCatalog, PlacementError> {
    let claims = collect_owner_claims(prog)?;
    let mut entries = Vec::with_capacity(prog.functions.len());
    for (index, function) in prog.functions.iter().enumerate() {
        if function.id.index() != index {
            return Err(PlacementError(format!(
                "callable fn{} `{}` is stored at function-table index {index}",
                function.id.0, function.name
            )));
        }
        let owner = resolve_owner(prog, function, &claims[index])?;
        let kind = callable_kind(&function.kind);
        let placement = seed_placement(prog, function, &owner);
        let dependencies = dependencies(prog, function)?;
        entries.push(CallablePlan {
            function: function.id,
            name: function.name.clone(),
            kind,
            owner,
            placement,
            dependencies,
        });
    }

    let components = strongly_connected_components(&entries);
    propagate_placements(&mut entries, &components);
    Ok(CallableCatalog { callables: entries })
}

fn callable_kind(kind: &FunctionKind) -> CallableKind {
    match kind {
        FunctionKind::TestBody {
            member: TestCallableMember::Run,
            ..
        } => CallableKind::Run,
        FunctionKind::TestBody {
            member: TestCallableMember::Check,
            ..
        } => CallableKind::Check,
        FunctionKind::SamplerAuto { .. } => CallableKind::SamplerAuto,
        FunctionKind::Helper => CallableKind::Helper,
        FunctionKind::TestbenchMethod { .. } => CallableKind::TestbenchMethod,
        FunctionKind::ComponentMethod { .. } => CallableKind::ComponentMethod,
        FunctionKind::TransactorBody { .. } => CallableKind::TransactorMethod,
        FunctionKind::Tseq { .. } => CallableKind::Tseq,
        FunctionKind::TestHook { .. } => CallableKind::TestHook,
        FunctionKind::TestbenchLifecycle { .. } => CallableKind::TestbenchLifecycle,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum OwnerClaimSlot {
    Run,
    Check,
    TestbenchMethod,
    Component,
    Transactor,
    TestHook(crate::ir::TestHookMember),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OwnerClaim {
    owner: CallableOwner,
    slot: OwnerClaimSlot,
}

fn collect_owner_claims(prog: &TbProgram) -> Result<Vec<Vec<OwnerClaim>>, PlacementError> {
    let mut claims = vec![Vec::new(); prog.functions.len()];
    let mut push = |function: FunctionId, claim: OwnerClaim, source: String| {
        let Some(slot) = claims.get_mut(function.index()) else {
            return Err(PlacementError(format!(
                "{source} references missing callable fn{}",
                function.0
            )));
        };
        slot.push(claim);
        Ok(())
    };
    for (test_index, test) in prog.tests.iter().enumerate() {
        let id = TestId(test_index as u32);
        if test.id != id {
            return Err(PlacementError(format!(
                "test table slot t{test_index} carries mismatched id t{}",
                test.id.0
            )));
        }
        let owner = CallableOwner::Test {
            id,
            test: test.name.clone(),
            testbench: test.testbench,
        };
        push(
            test.run,
            OwnerClaim {
                owner: owner.clone(),
                slot: OwnerClaimSlot::Run,
            },
            format!("test `{}` run slot", test.name),
        )?;
        if let Some(check) = test.check {
            push(
                check,
                OwnerClaim {
                    owner: owner.clone(),
                    slot: OwnerClaimSlot::Check,
                },
                format!("test `{}` check slot", test.name),
            )?;
        }
        if let Some(testbench) = prog.testbenches.get(test.testbench.index()) {
            for binding in &testbench.regblock_bindings {
                for (register, callback) in &binding.callbacks {
                    push(
                        *callback,
                        OwnerClaim {
                            owner: owner.clone(),
                            slot: OwnerClaimSlot::TestHook(
                                crate::ir::TestHookMember::RegblockWrite {
                                    binding: binding.field.clone(),
                                    register: register.clone(),
                                },
                            ),
                        },
                        format!(
                            "test `{}` register callback `{}.{register}`",
                            test.name, binding.field
                        ),
                    )?;
                }
            }
        }
    }
    for (index, schema) in prog.testbench_types.iter().enumerate() {
        for method in &schema.methods {
            push(
                method.function,
                OwnerClaim {
                    owner: CallableOwner::TestbenchType(TestbenchTypeId(index as u32)),
                    slot: OwnerClaimSlot::TestbenchMethod,
                },
                format!("testbench type `{}` method `{}`", schema.name, method.name),
            )?;
        }
    }
    for (index, schema) in prog.components.iter().enumerate() {
        let component = ComponentId(index as u32);
        for (member, method) in schema.methods.iter().enumerate() {
            push(
                method.function,
                OwnerClaim {
                    owner: CallableOwner::Component {
                        component,
                        member: ComponentCallableMember::Method(member),
                    },
                    slot: OwnerClaimSlot::Component,
                },
                format!("component `{}` method `{}`", schema.name, method.name),
            )?;
        }
        for (member, handler) in schema.on_handlers.iter().enumerate() {
            push(
                handler.function,
                OwnerClaim {
                    owner: CallableOwner::Component {
                        component,
                        member: ComponentCallableMember::OnHandler(member),
                    },
                    slot: OwnerClaimSlot::Component,
                },
                format!("component `{}` on-handler {member}", schema.name),
            )?;
        }
        for (member, handler) in schema.periodic_handlers.iter().enumerate() {
            push(
                handler.function,
                OwnerClaim {
                    owner: CallableOwner::Component {
                        component,
                        member: ComponentCallableMember::PeriodicHandler(member),
                    },
                    slot: OwnerClaimSlot::Component,
                },
                format!("component `{}` periodic handler {member}", schema.name),
            )?;
        }
        for (member, handler) in schema.cycle_handlers.iter().enumerate() {
            push(
                handler.function,
                OwnerClaim {
                    owner: CallableOwner::Component {
                        component,
                        member: ComponentCallableMember::CycleHandler(member),
                    },
                    slot: OwnerClaimSlot::Component,
                },
                format!("component `{}` cycle handler {member}", schema.name),
            )?;
        }
        if let Some(handler) = &schema.watchdog {
            push(
                handler.function,
                OwnerClaim {
                    owner: CallableOwner::Component {
                        component,
                        member: ComponentCallableMember::Watchdog,
                    },
                    slot: OwnerClaimSlot::Component,
                },
                format!("component `{}` watchdog", schema.name),
            )?;
        }
    }
    for (index, schema) in prog.transactors.iter().enumerate() {
        let transactor = TransactorId(index as u32);
        for (member, method) in schema.methods.iter().enumerate() {
            push(
                method.function,
                OwnerClaim {
                    owner: CallableOwner::Transactor {
                        transactor,
                        member: TransactorCallableMember::Method(member),
                    },
                    slot: OwnerClaimSlot::Transactor,
                },
                format!("transactor `{}` method `{}`", schema.name, method.name),
            )?;
        }
        for (member, method) in schema.target_methods.iter().enumerate() {
            push(
                method.function,
                OwnerClaim {
                    owner: CallableOwner::Transactor {
                        transactor,
                        member: TransactorCallableMember::TargetMethod(member),
                    },
                    slot: OwnerClaimSlot::Transactor,
                },
                format!(
                    "transactor `{}` target method `{}`",
                    schema.name, method.name
                ),
            )?;
        }
    }
    let test_owner_for_tb = |testbench: crate::ir::TestbenchId| {
        let owners = prog
            .tests
            .iter()
            .filter(|test| test.testbench == testbench)
            .collect::<Vec<_>>();
        (owners.len() == 1).then(|| CallableOwner::Test {
            id: owners[0].id,
            test: owners[0].name.clone(),
            testbench,
        })
    };
    for function in &prog.functions {
        let FunctionKind::TestBody {
            test,
            name,
            member: _,
        } = &function.kind
        else {
            continue;
        };
        let Some(schema) = prog.tests.get(test.index()) else {
            continue;
        };
        if schema.id != *test || schema.name != *name || Some(schema.testbench) != function.owner {
            continue;
        }
        let owner = CallableOwner::Test {
            id: schema.id,
            test: schema.name.clone(),
            testbench: schema.testbench,
        };
        for block in &function.blocks {
            for stmt in &block.stmts {
                let hook = match stmt {
                    Stmt::EventSubscribe { site, handler, .. } => Some((
                        *handler,
                        crate::ir::TestHookMember::EventSubscription(site.clone()),
                        "event subscription",
                    )),
                    Stmt::MethodHookSubscribe { site, handler, .. } => Some((
                        *handler,
                        crate::ir::TestHookMember::MethodSubscription(site.clone()),
                        "method subscription",
                    )),
                    Stmt::CycleHandler(handler) => {
                        prog.cycle_handlers.get(handler.index()).map(|schema| {
                            (
                                schema.function,
                                crate::ir::TestHookMember::StatementCycle(schema.site.clone()),
                                "statement cycle handler",
                            )
                        })
                    }
                    _ => None,
                };
                if let Some((handler, member, description)) = hook {
                    push(
                        handler,
                        OwnerClaim {
                            owner: owner.clone(),
                            slot: OwnerClaimSlot::TestHook(member),
                        },
                        format!("fn{} {description}", function.id.0),
                    )?;
                }
            }
        }
    }
    for (testbench_index, testbench) in prog.testbenches.iter().enumerate() {
        let testbench_id = crate::ir::TestbenchId(testbench_index as u32);
        let Some(owner) = test_owner_for_tb(testbench_id) else {
            continue;
        };
        for (service, schema) in testbench.periodic_services.iter().enumerate() {
            push(
                schema.function,
                OwnerClaim {
                    owner: owner.clone(),
                    slot: OwnerClaimSlot::TestHook(crate::ir::TestHookMember::TestbenchPeriodic {
                        service: service as u32,
                    }),
                },
                format!("testbench `{}` periodic service {service}", testbench.name),
            )?;
        }
        for (service, schema) in testbench.cycle_services.iter().enumerate() {
            push(
                schema.function,
                OwnerClaim {
                    owner: owner.clone(),
                    slot: OwnerClaimSlot::TestHook(crate::ir::TestHookMember::TestbenchCycle {
                        service: service as u32,
                    }),
                },
                format!("testbench `{}` cycle service {service}", testbench.name),
            )?;
        }
    }
    Ok(claims)
}

fn resolve_owner(
    prog: &TbProgram,
    function: &TbFunction,
    claims: &[OwnerClaim],
) -> Result<CallableOwner, PlacementError> {
    match &function.kind {
        FunctionKind::Helper | FunctionKind::Tseq { .. } => {
            if function.owner.is_some() || !claims.is_empty() {
                return Err(owner_kind_error(function));
            }
            Ok(CallableOwner::Suite)
        }
        FunctionKind::TestbenchLifecycle { testbench, .. } => {
            if !claims.is_empty()
                || function.owner != Some(*testbench)
                || prog.testbenches.get(testbench.index()).is_none()
            {
                return Err(owner_kind_error(function));
            }
            Ok(CallableOwner::TestbenchType(
                prog.testbenches[testbench.index()].type_id,
            ))
        }
        FunctionKind::TestbenchMethod {
            testbench,
            method,
            ref name,
        } => {
            if claims.len() != 1 {
                return Err(owner_error(function, claims.len()));
            }
            if claims[0]
                != (OwnerClaim {
                    owner: CallableOwner::TestbenchType(*testbench),
                    slot: OwnerClaimSlot::TestbenchMethod,
                })
                || function.owner.is_some()
                || prog
                    .testbench_types
                    .get(testbench.index())
                    .and_then(|schema| schema.methods.get(method.index()))
                    .is_none_or(|schema| schema.function != function.id || schema.name != *name)
            {
                return Err(owner_kind_error(function));
            }
            Ok(CallableOwner::TestbenchType(*testbench))
        }
        FunctionKind::TestBody { test, member, name } => {
            if claims.len() != 1 {
                return Err(owner_error(function, claims.len()));
            }
            let expected_slot = match member {
                TestCallableMember::Run => OwnerClaimSlot::Run,
                TestCallableMember::Check => OwnerClaimSlot::Check,
            };
            let OwnerClaim {
                owner:
                    owner @ CallableOwner::Test {
                        id,
                        test: owner_name,
                        testbench,
                    },
                slot,
            } = &claims[0]
            else {
                return Err(owner_kind_error(function));
            };
            let schema = prog.tests.get(test.index());
            let expected_function = schema.map(|schema| match member {
                TestCallableMember::Run => Some(schema.run),
                TestCallableMember::Check => schema.check,
            });
            let expected_function_name = format!(
                "{}_{}",
                match member {
                    TestCallableMember::Run => "run",
                    TestCallableMember::Check => "check",
                },
                name
            );
            if *slot != expected_slot
                || test != id
                || name != owner_name
                || function.owner != Some(*testbench)
                || function.name != expected_function_name
                || schema.is_none_or(|schema| {
                    schema.id != *test
                        || schema.name != *name
                        || schema.testbench != *testbench
                        || expected_function != Some(Some(function.id))
                })
            {
                return Err(owner_kind_error(function));
            }
            Ok(owner.clone())
        }
        FunctionKind::SamplerAuto { .. } => {
            if !claims.is_empty() {
                return Err(owner_kind_error(function));
            }
            let Some(testbench) = function.owner else {
                return Err(owner_error(function, 0));
            };
            let owners = prog
                .tests
                .iter()
                .filter(|test| test.testbench == testbench)
                .collect::<Vec<_>>();
            if owners.len() != 1 {
                return Err(owner_error(function, owners.len()));
            }
            Ok(CallableOwner::Test {
                id: owners[0].id,
                test: owners[0].name.clone(),
                testbench,
            })
        }
        FunctionKind::TestHook { member } => {
            if claims.len() != 1 || member == &crate::ir::TestHookMember::Pending {
                return Err(owner_error(function, claims.len()));
            }
            let OwnerClaim {
                owner:
                    owner @ CallableOwner::Test {
                        testbench, test, ..
                    },
                slot: OwnerClaimSlot::TestHook(claimed),
            } = &claims[0]
            else {
                return Err(owner_kind_error(function));
            };
            if claimed != member
                || function.owner != Some(*testbench)
                || function.name != member.function_name(test)
                || match member {
                    crate::ir::TestHookMember::EventSubscription(site)
                    | crate::ir::TestHookMember::MethodSubscription(site)
                    | crate::ir::TestHookMember::StatementCycle(site) => site.owner.test != *test,
                    _ => false,
                }
            {
                return Err(owner_kind_error(function));
            }
            Ok(owner.clone())
        }
        FunctionKind::ComponentMethod {
            component,
            member,
            ref method_name,
        } => {
            if claims.len() != 1 {
                return Err(owner_error(function, claims.len()));
            }
            let OwnerClaim {
                owner:
                    owner @ CallableOwner::Component {
                        component: claimed_component,
                        ..
                    },
                slot: OwnerClaimSlot::Component,
            } = &claims[0]
            else {
                return Err(owner_kind_error(function));
            };
            let expected_owner = prog
                .components
                .get(component.index())
                .and_then(|schema| component_callable_at(schema, *member));
            let expected_method_name = match expected_owner.map(|(_, member)| member) {
                Some(ComponentCallableMember::Method(index)) => prog.components[component.index()]
                    .methods
                    .get(index)
                    .map(|method| method.name.as_str()),
                Some(_) => None,
                None => None,
            };
            if *claimed_component != *component
                || function.owner.is_some()
                || method_name.as_deref() != expected_method_name
                || expected_owner.is_none_or(|(expected_function, expected_member)| {
                    expected_function != function.id
                        || owner
                            != &(CallableOwner::Component {
                                component: *component,
                                member: expected_member,
                            })
                })
            {
                return Err(owner_kind_error(function));
            }
            Ok(owner.clone())
        }
        FunctionKind::TransactorBody {
            transactor,
            member,
            ref name,
        } => {
            if claims.len() != 1 {
                return Err(owner_error(function, claims.len()));
            }
            let OwnerClaim {
                owner:
                    owner @ CallableOwner::Transactor {
                        transactor: claimed_transactor,
                        ..
                    },
                slot: OwnerClaimSlot::Transactor,
            } = &claims[0]
            else {
                return Err(owner_kind_error(function));
            };
            let expected_owner = prog
                .transactors
                .get(transactor.index())
                .and_then(|schema| transactor_callable_at(schema, *member));
            let schema = &prog.transactors[transactor.index()];
            let expected_function_name = expected_owner.map(|(_, member, method)| match member {
                TransactorCallableMember::Method(_) => {
                    format!("{}_{}", schema.emission_name(), method)
                }
                TransactorCallableMember::TargetMethod(_) => {
                    format!("{}_target_{}", schema.emission_name(), method)
                }
            });
            if *claimed_transactor != *transactor
                || function.owner.is_some()
                || expected_function_name.as_deref() != Some(function.name.as_str())
                || expected_owner.is_none_or(
                    |(expected_function, expected_member, expected_name)| {
                        expected_function != function.id
                            || expected_name != name
                            || owner
                                != &(CallableOwner::Transactor {
                                    transactor: *transactor,
                                    member: expected_member,
                                })
                    },
                )
            {
                return Err(owner_kind_error(function));
            }
            Ok(owner.clone())
        }
    }
}

fn component_callable_at(
    schema: &crate::ir::ComponentSchema,
    member: crate::ir::ComponentCallableId,
) -> Option<(FunctionId, ComponentCallableMember)> {
    let mut index = member.index();
    if let Some(method) = schema.methods.get(index) {
        return Some((method.function, ComponentCallableMember::Method(index)));
    }
    index = index.checked_sub(schema.methods.len())?;
    if let Some(handler) = schema.on_handlers.get(index) {
        return Some((handler.function, ComponentCallableMember::OnHandler(index)));
    }
    index = index.checked_sub(schema.on_handlers.len())?;
    if let Some(handler) = schema.periodic_handlers.get(index) {
        return Some((
            handler.function,
            ComponentCallableMember::PeriodicHandler(index),
        ));
    }
    index = index.checked_sub(schema.periodic_handlers.len())?;
    if let Some(handler) = schema.cycle_handlers.get(index) {
        return Some((
            handler.function,
            ComponentCallableMember::CycleHandler(index),
        ));
    }
    index = index.checked_sub(schema.cycle_handlers.len())?;
    if index == 0 {
        schema
            .watchdog
            .as_ref()
            .map(|handler| (handler.function, ComponentCallableMember::Watchdog))
    } else {
        None
    }
}

fn transactor_callable_at(
    schema: &crate::ir::TransactorSchema,
    member: crate::ir::TransactorCallableId,
) -> Option<(FunctionId, TransactorCallableMember, &str)> {
    let index = member.index();
    if let Some(method) = schema.methods.get(index) {
        return Some((
            method.function,
            TransactorCallableMember::Method(index),
            method.name.as_str(),
        ));
    }
    let index = index.checked_sub(schema.methods.len())?;
    schema.target_methods.get(index).map(|method| {
        (
            method.function,
            TransactorCallableMember::TargetMethod(index),
            method.name.as_str(),
        )
    })
}

fn owner_error(function: &TbFunction, claims: usize) -> PlacementError {
    let detail = if claims == 0 {
        "missing owner".to_string()
    } else {
        format!("{claims} owners")
    };
    PlacementError(format!(
        "callable fn{} `{}` must have exactly one owner; found {detail}",
        function.id.0, function.name
    ))
}

fn owner_kind_error(function: &TbFunction) -> PlacementError {
    PlacementError(format!(
        "callable fn{} `{}` has an owner whose schema does not match {:?}",
        function.id.0, function.name, function.kind
    ))
}

fn seed_placement(
    prog: &TbProgram,
    function: &TbFunction,
    owner: &CallableOwner,
) -> CallablePlacement {
    if let Some(reason) = regblock_state_reason(prog, owner) {
        // Test-owned run/check/hook bodies already live in their concrete
        // capsule, where the regblock mirror and frontdoor binding are
        // unambiguous. Reusable testbench methods still need an explicit RAL
        // receiver before they can be shared safely.
        if matches!(owner, CallableOwner::TestbenchType(_)) {
            return CallablePlacement::Invalid { reason };
        }
    }
    if matches!(
        owner,
        CallableOwner::Transactor {
            member: TransactorCallableMember::TargetMethod(_),
            ..
        }
    ) {
        return CallablePlacement::CapsuleScoped {
            reason: CapsulePlacementReason::TargetResponder,
        };
    }
    let concrete_bus_fields = function_concrete_bus_fields(function);
    if !concrete_bus_fields.is_empty() && matches!(owner, CallableOwner::TestbenchType(_)) {
        return concrete_bus_placement(prog, owner, &concrete_bus_fields);
    }
    let bound_transactor_method = match owner {
        CallableOwner::Transactor { transactor, .. } => prog
            .transactors
            .get(transactor.index())
            .is_some_and(|schema| schema.bound_bus.is_some()),
        _ => false,
    };
    if function_uses_bound_bus(function) || bound_transactor_method {
        return bound_bus_placement(prog, owner);
    }
    match (&function.kind, owner) {
        (FunctionKind::Helper | FunctionKind::Tseq { .. }, CallableOwner::Suite) => {
            CallablePlacement::Common
        }
        (FunctionKind::TestbenchMethod { testbench, .. }, CallableOwner::TestbenchType(owner))
            if testbench == owner =>
        {
            CallablePlacement::Common
        }
        (FunctionKind::TestbenchLifecycle { .. }, CallableOwner::TestbenchType(_)) => {
            CallablePlacement::CapsuleScoped {
                reason: CapsulePlacementReason::LifecycleService,
            }
        }
        (FunctionKind::TestBody { .. }, CallableOwner::Test { test, .. }) => {
            CallablePlacement::CapsuleLocal {
                test: test.clone(),
                reason: CapsulePlacementReason::TestBody,
            }
        }
        (FunctionKind::SamplerAuto { .. }, CallableOwner::Test { test, .. }) => {
            CallablePlacement::CapsuleLocal {
                test: test.clone(),
                reason: CapsulePlacementReason::LifecycleService,
            }
        }
        (FunctionKind::TestHook { .. }, CallableOwner::Test { test, .. }) => {
            CallablePlacement::CapsuleLocal {
                test: test.clone(),
                reason: CapsulePlacementReason::TestHook,
            }
        }
        (FunctionKind::ComponentMethod { .. }, CallableOwner::Component { .. }) => {
            CallablePlacement::Common
        }
        (FunctionKind::TransactorBody { .. }, CallableOwner::Transactor { .. }) => {
            CallablePlacement::Common
        }
        _ => CallablePlacement::Invalid {
            reason: InvalidPlacementReason::OwnerKindMismatch,
        },
    }
}

fn concrete_bus_placement(
    prog: &TbProgram,
    owner: &CallableOwner,
    fields: &BTreeSet<String>,
) -> CallablePlacement {
    let CallableOwner::TestbenchType(testbench_type) = owner else {
        return CallablePlacement::Invalid {
            reason: InvalidPlacementReason::MissingConcreteBusBinding,
        };
    };
    let mut reference: Option<(String, Vec<&crate::ir::BusBindingSchema>)> = None;
    let mut test_count = 0usize;
    for test in &prog.tests {
        let Some(testbench) = prog.testbenches.get(test.testbench.index()) else {
            continue;
        };
        if testbench.type_id != *testbench_type {
            continue;
        }
        test_count += 1;
        let mut bindings = Vec::with_capacity(fields.len());
        for field in fields {
            let Some(binding) = testbench
                .bus_bindings
                .iter()
                .find(|binding| binding.field == *field)
            else {
                return CallablePlacement::Invalid {
                    reason: InvalidPlacementReason::MissingConcreteBusBinding,
                };
            };
            bindings.push(binding);
        }
        if let Some((first_test, first_bindings)) = &reference {
            if let Some((first, second)) = first_bindings
                .iter()
                .zip(&bindings)
                .find(|(first, second)| !same_bus_binding_semantics(first, second))
            {
                return CallablePlacement::Invalid {
                    reason: InvalidPlacementReason::ConflictingBusBindings {
                        first_test: first_test.clone(),
                        first_binding: first.id,
                        second_test: test.name.clone(),
                        second_binding: second.id,
                    },
                };
            }
        } else {
            reference = Some((test.name.clone(), bindings));
        }
    }
    match (test_count, reference) {
        (0, _) | (_, None) => CallablePlacement::Invalid {
            reason: InvalidPlacementReason::MissingConcreteBusBinding,
        },
        (1, Some((test, bindings))) => CallablePlacement::CapsuleLocal {
            test,
            reason: CapsulePlacementReason::ConcreteBusBinding {
                binding: bindings[0].id,
            },
        },
        (_, Some(_)) => CallablePlacement::Common,
    }
}

pub(crate) fn same_bus_binding_semantics(
    lhs: &crate::ir::BusBindingSchema,
    rhs: &crate::ir::BusBindingSchema,
) -> bool {
    lhs.field == rhs.field && lhs.bus == rhs.bus && lhs.methods == rhs.methods
}

pub(crate) fn same_bound_bus_binding_semantics(
    lhs: &crate::ir::BusBindingSchema,
    rhs: &crate::ir::BusBindingSchema,
) -> bool {
    lhs.bus == rhs.bus && lhs.methods == rhs.methods
}

fn regblock_state_reason(
    prog: &TbProgram,
    owner: &CallableOwner,
) -> Option<InvalidPlacementReason> {
    match owner {
        CallableOwner::Test { testbench, .. } => prog
            .testbenches
            .get(testbench.index())
            .and_then(|schema| regblock_reason_for_schema(*testbench, schema)),
        CallableOwner::TestbenchType(testbench_type) => prog
            .testbenches
            .iter()
            .enumerate()
            .filter(|(_, schema)| schema.type_id == *testbench_type)
            .find_map(|(index, schema)| {
                regblock_reason_for_schema(TestbenchId(index as u32), schema)
            }),
        _ => None,
    }
}

fn regblock_reason_for_schema(
    testbench: TestbenchId,
    schema: &crate::ir::TestbenchSchema,
) -> Option<InvalidPlacementReason> {
    (!schema.regblock_bindings.is_empty()).then(|| InvalidPlacementReason::RegblockState {
        testbench,
        callback_bearing: schema
            .regblock_bindings
            .iter()
            .any(|binding| !binding.callbacks.is_empty()),
    })
}

fn bound_bus_placement(prog: &TbProgram, owner: &CallableOwner) -> CallablePlacement {
    let bound_owner = match owner {
        CallableOwner::Component { component, .. } => {
            Some(crate::ir::BoundBusOwner::Component(*component))
        }
        CallableOwner::Transactor { transactor, .. } => {
            Some(crate::ir::BoundBusOwner::Transactor(*transactor))
        }
        _ => None,
    };
    let Some(bound_owner) = bound_owner else {
        return CallablePlacement::Invalid {
            reason: InvalidPlacementReason::MissingConcreteBusBinding,
        };
    };

    let mut bindings = Vec::new();
    for test in &prog.tests {
        let Some(testbench) = prog.testbenches.get(test.testbench.index()) else {
            continue;
        };
        for instance in &testbench.bound_bus_instances {
            if instance.owner == bound_owner {
                let Some(binding) = testbench.bus_binding(instance.binding) else {
                    return CallablePlacement::Invalid {
                        reason: InvalidPlacementReason::MissingConcreteBusBinding,
                    };
                };
                bindings.push((test.name.clone(), binding));
            }
        }
    }
    bindings.sort_by_key(|(test, binding)| (test.clone(), binding.id));
    let Some((first_test, first_binding)) = bindings.first() else {
        return CallablePlacement::Invalid {
            reason: InvalidPlacementReason::MissingConcreteBusBinding,
        };
    };
    if let Some((second_test, second_binding)) = bindings
        .iter()
        .skip(1)
        .find(|(_, binding)| !same_bound_bus_binding_semantics(first_binding, binding))
    {
        return CallablePlacement::Invalid {
            reason: InvalidPlacementReason::ConflictingBusBindings {
                first_test: first_test.clone(),
                first_binding: first_binding.id,
                second_test: second_test.clone(),
                second_binding: second_binding.id,
            },
        };
    }
    CallablePlacement::Common
}

pub(crate) fn function_uses_bound_bus(function: &TbFunction) -> bool {
    function.blocks.iter().any(|block| {
        block.stmts.iter().any(stmt_uses_bound_bus) || terminator_uses_bound_bus(&block.terminator)
    })
}

pub(crate) fn function_concrete_bus_fields(function: &TbFunction) -> BTreeSet<String> {
    let mut fields = BTreeSet::new();
    for block in &function.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::DutWrite(port, _) | Stmt::DutRead(_, port) | Stmt::ProbeRelease(port) => {
                    visit_concrete_bus_port(port, &mut fields);
                }
                Stmt::TlmFork(desc) => {
                    visit_concrete_bus_target(&desc.target, &mut fields);
                }
                Stmt::TlmJoinAll(pending) => {
                    for desc in pending {
                        visit_concrete_bus_target(&desc.target, &mut fields);
                    }
                }
                _ => {}
            }
            visit_stmt_exprs(stmt, &mut |expr| {
                visit_expr_ports(expr, &mut |port| visit_concrete_bus_port(port, &mut fields));
                visit_expr_concrete_bus_fields(expr, &mut fields);
                Ok(())
            })
            .expect("bus-binding expression scan cannot fail");
        }
        visit_terminator_exprs(&block.terminator, &mut |expr| {
            visit_expr_ports(expr, &mut |port| visit_concrete_bus_port(port, &mut fields));
            visit_expr_concrete_bus_fields(expr, &mut fields);
            Ok(())
        })
        .expect("bus-binding terminator scan cannot fail");
    }
    fields
}

fn visit_concrete_bus_port(port: &crate::ir::PortRef, fields: &mut BTreeSet<String>) {
    if let crate::ir::PortOrigin::BusBinding { field, .. } = &port.origin {
        fields.insert(field.clone());
    }
}

fn visit_concrete_bus_target(
    target: &crate::ir::TransactorMethodTarget,
    fields: &mut BTreeSet<String>,
) {
    if let crate::ir::TransactorMethodTarget::TestbenchBusField { field, .. } = target {
        fields.insert(field.clone());
    }
}

fn visit_expr_concrete_bus_fields(expr: &Expr, fields: &mut BTreeSet<String>) {
    if let Expr::Call(crate::ir::CallTarget::TransactorMethod { target, .. }, _) = expr {
        visit_concrete_bus_target(target, fields);
    }
    visit_expr_children(expr, &mut |child| {
        visit_expr_concrete_bus_fields(child, fields);
        Ok(())
    })
    .expect("bus-binding expression scan cannot fail");
}

fn visit_expr_ports(expr: &Expr, visit: &mut impl FnMut(&crate::ir::PortRef)) {
    match expr {
        Expr::Port(port) | Expr::PortSnapshotLane { port, .. } => visit(port),
        _ => {}
    }
    visit_expr_children(expr, &mut |child| {
        visit_expr_ports(child, visit);
        Ok(())
    })
    .expect("port expression scan cannot fail");
}

fn stmt_uses_bound_bus(stmt: &Stmt) -> bool {
    let direct = match stmt {
        Stmt::DutWrite(port, _) | Stmt::DutRead(_, port) | Stmt::ProbeRelease(port) => {
            port.origin == crate::ir::PortOrigin::BoundBus
        }
        Stmt::TlmFork(desc) => desc.target == crate::ir::TransactorMethodTarget::BoundBus,
        Stmt::TlmJoinAll(pending) => pending
            .iter()
            .any(|desc| desc.target == crate::ir::TransactorMethodTarget::BoundBus),
        _ => false,
    };
    if direct {
        return true;
    }
    let mut found = false;
    visit_stmt_exprs(stmt, &mut |expr| {
        found |= expr_uses_bound_bus(expr);
        Ok(())
    })
    .expect("bound-bus expression scan cannot fail");
    found
}

fn terminator_uses_bound_bus(terminator: &Terminator) -> bool {
    let mut found = false;
    visit_terminator_exprs(terminator, &mut |expr| {
        found |= expr_uses_bound_bus(expr);
        Ok(())
    })
    .expect("bound-bus terminator scan cannot fail");
    found
}

fn expr_uses_bound_bus(expr: &Expr) -> bool {
    let direct = match expr {
        Expr::Port(port) | Expr::PortSnapshotLane { port, .. } => {
            port.origin == crate::ir::PortOrigin::BoundBus
        }
        Expr::Call(
            crate::ir::CallTarget::TransactorMethod {
                target: crate::ir::TransactorMethodTarget::BoundBus,
                ..
            },
            _,
        ) => true,
        _ => false,
    };
    if direct {
        return true;
    }
    let mut found = false;
    visit_expr_children(expr, &mut |child| {
        found |= expr_uses_bound_bus(child);
        Ok(())
    })
    .expect("bound-bus child scan cannot fail");
    found
}

fn dependencies(
    prog: &TbProgram,
    function: &TbFunction,
) -> Result<Vec<FunctionId>, PlacementError> {
    let mut dependencies = BTreeSet::new();
    visit_function(function, &mut |target| {
        let resolved = match target {
            DependencyTarget::Helper(function) => prog
                .functions
                .get(function.index())
                .filter(|candidate| {
                    candidate.id == function && candidate.kind == FunctionKind::Helper
                })
                .map(|_| function)
                .ok_or_else(|| format!("helper fn{}", function.0)),
            DependencyTarget::Tseq(function) => prog
                .functions
                .get(function.index())
                .filter(|candidate| {
                    candidate.id == function && matches!(candidate.kind, FunctionKind::Tseq { .. })
                })
                .map(|_| function)
                .ok_or_else(|| format!("tseq fn{}", function.0)),
            DependencyTarget::Testbench(function) => prog
                .functions
                .get(function.index())
                .filter(|candidate| {
                    candidate.id == function
                        && matches!(candidate.kind, FunctionKind::TestbenchMethod { .. })
                })
                .map(|_| function)
                .ok_or_else(|| format!("testbench method fn{}", function.0)),
            DependencyTarget::Function(function) => prog
                .functions
                .get(function.index())
                .filter(|candidate| {
                    candidate.id == function
                        && matches!(candidate.kind, FunctionKind::TransactorBody { .. })
                })
                .map(|_| function)
                .ok_or_else(|| format!("transactor method fn{}", function.0)),
            DependencyTarget::Component(function) => prog
                .functions
                .get(function.index())
                .filter(|candidate| {
                    candidate.id == function
                        && matches!(candidate.kind, FunctionKind::ComponentMethod { .. })
                })
                .map(|_| function)
                .ok_or_else(|| format!("component method fn{}", function.0)),
            DependencyTarget::Hook(function) => prog
                .functions
                .get(function.index())
                .filter(|candidate| {
                    candidate.id == function
                        && matches!(candidate.kind, FunctionKind::TestHook { .. })
                })
                .map(|_| function)
                .ok_or_else(|| format!("test hook fn{}", function.0)),
            DependencyTarget::Lifecycle(function) => prog
                .functions
                .get(function.index())
                .filter(|candidate| {
                    candidate.id == function
                        && matches!(candidate.kind, FunctionKind::TestbenchLifecycle { .. })
                })
                .map(|_| function)
                .ok_or_else(|| format!("testbench lifecycle fn{}", function.0)),
        };
        match resolved {
            Ok(id) => {
                dependencies.insert(id);
                Ok(())
            }
            Err(target) => Err(PlacementError(format!(
                "callable fn{} `{}` references missing callable {target}",
                function.id.0, function.name
            ))),
        }
    })?;
    Ok(dependencies.into_iter().collect())
}

enum DependencyTarget {
    Helper(FunctionId),
    Tseq(FunctionId),
    Testbench(FunctionId),
    Function(FunctionId),
    Component(FunctionId),
    Hook(FunctionId),
    Lifecycle(FunctionId),
}

fn visit_function(
    function: &TbFunction,
    visit: &mut impl FnMut(DependencyTarget) -> Result<(), PlacementError>,
) -> Result<(), PlacementError> {
    for block in &function.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::ComponentCall { function, .. } => {
                    visit(DependencyTarget::Component(*function))?;
                }
                Stmt::TestbenchCall {
                    function: target, ..
                } => {
                    visit(DependencyTarget::Testbench(*target))?;
                }
                Stmt::RecordWriteCb {
                    callback: Some(function),
                    ..
                }
                | Stmt::EventSubscribe {
                    handler: function, ..
                }
                | Stmt::MethodHookSubscribe {
                    handler: function, ..
                } => {
                    visit(DependencyTarget::Hook(*function))?;
                }
                Stmt::TlmFork(desc) => visit_tlm_target(&desc.target, visit)?,
                Stmt::TlmJoinAll(pending) => {
                    for desc in pending {
                        visit_tlm_target(&desc.target, visit)?;
                    }
                }
                _ => {}
            }
            visit_stmt_exprs(stmt, &mut |expr| visit_expr(expr, visit))?;
        }
        if let Terminator::TbLifecycleCall { function, .. } = &block.terminator {
            visit(DependencyTarget::Lifecycle(*function))?;
        }
        visit_terminator_exprs(&block.terminator, &mut |expr| visit_expr(expr, visit))?;
    }
    Ok(())
}

fn visit_tlm_target(
    target: &crate::ir::TransactorMethodTarget,
    visit: &mut impl FnMut(DependencyTarget) -> Result<(), PlacementError>,
) -> Result<(), PlacementError> {
    if let crate::ir::TransactorMethodTarget::Callable { function, .. } = target {
        visit(DependencyTarget::Function(*function))?;
    }
    Ok(())
}

fn visit_expr(
    expr: &Expr,
    visit: &mut impl FnMut(DependencyTarget) -> Result<(), PlacementError>,
) -> Result<(), PlacementError> {
    if let Expr::Call(target, _) = expr {
        match target {
            crate::ir::CallTarget::Helper { function, .. } => {
                visit(DependencyTarget::Helper(*function))?
            }
            crate::ir::CallTarget::Tseq { function, .. } => {
                visit(DependencyTarget::Tseq(*function))?
            }
            crate::ir::CallTarget::TransactorSelfMethod { function, .. } => {
                visit(DependencyTarget::Function(*function))?
            }
            crate::ir::CallTarget::TransactorMethod {
                target: crate::ir::TransactorMethodTarget::Callable { function, .. },
                ..
            } => visit(DependencyTarget::Function(*function))?,
            crate::ir::CallTarget::Builtin(_)
            | crate::ir::CallTarget::ExternFn { .. }
            | crate::ir::CallTarget::TransactorMethod { .. } => {}
        }
    }
    visit_expr_children(expr, &mut |child| visit_expr(child, visit))
}

pub(super) fn visit_stmt_exprs(
    stmt: &Stmt,
    visit: &mut impl FnMut(&Expr) -> Result<(), PlacementError>,
) -> Result<(), PlacementError> {
    crate::ir::visit::try_visit_stmt_exprs(stmt, visit)
}

pub(super) fn visit_terminator_exprs(
    terminator: &Terminator,
    visit: &mut impl FnMut(&Expr) -> Result<(), PlacementError>,
) -> Result<(), PlacementError> {
    crate::ir::visit::try_visit_terminator_exprs(terminator, visit)
}

pub(super) fn visit_expr_children(
    expr: &Expr,
    visit: &mut impl FnMut(&Expr) -> Result<(), PlacementError>,
) -> Result<(), PlacementError> {
    crate::ir::visit::try_visit_expr_children(expr, visit)
}

fn strongly_connected_components(entries: &[CallablePlan]) -> Vec<Vec<usize>> {
    struct Tarjan<'a> {
        entries: &'a [CallablePlan],
        next: usize,
        indices: Vec<Option<usize>>,
        low: Vec<usize>,
        stack: Vec<usize>,
        on_stack: Vec<bool>,
        components: Vec<Vec<usize>>,
    }
    impl Tarjan<'_> {
        fn visit(&mut self, node: usize) {
            self.indices[node] = Some(self.next);
            self.low[node] = self.next;
            self.next += 1;
            self.stack.push(node);
            self.on_stack[node] = true;
            for dependency in &self.entries[node].dependencies {
                let next = dependency.index();
                if next >= self.entries.len() {
                    continue;
                }
                if self.indices[next].is_none() {
                    self.visit(next);
                    self.low[node] = self.low[node].min(self.low[next]);
                } else if self.on_stack[next] {
                    self.low[node] = self.low[node].min(self.indices[next].unwrap());
                }
            }
            if self.low[node] == self.indices[node].unwrap() {
                let mut component = Vec::new();
                loop {
                    let member = self.stack.pop().expect("active SCC has a stack member");
                    self.on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                component.sort_unstable();
                self.components.push(component);
            }
        }
    }
    let mut tarjan = Tarjan {
        entries,
        next: 0,
        indices: vec![None; entries.len()],
        low: vec![0; entries.len()],
        stack: Vec::new(),
        on_stack: vec![false; entries.len()],
        components: Vec::new(),
    };
    for node in 0..entries.len() {
        if tarjan.indices[node].is_none() {
            tarjan.visit(node);
        }
    }
    tarjan.components.sort_by_key(|component| component[0]);
    tarjan.components
}

fn propagate_placements(entries: &mut [CallablePlan], components: &[Vec<usize>]) {
    loop {
        let previous = entries
            .iter()
            .map(|entry| entry.placement.clone())
            .collect::<Vec<_>>();
        let mut changed = false;
        for component in components {
            let mut combined = previous[component[0]].clone();
            for &member in component.iter().skip(1) {
                combined = merge_placement(combined, &previous[member], entries[member].function);
            }
            for &member in component {
                for dependency in &entries[member].dependencies {
                    if let Some(target) = previous.get(dependency.index()) {
                        combined = merge_placement(combined, target, *dependency);
                    } else {
                        combined = CallablePlacement::Invalid {
                            reason: InvalidPlacementReason::MissingDependency {
                                target: format!("fn{}", dependency.0),
                            },
                        };
                    }
                }
            }
            for &member in component {
                if entries[member].placement != combined {
                    entries[member].placement = combined.clone();
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
}

fn merge_placement(
    current: CallablePlacement,
    dependency: &CallablePlacement,
    dependency_function: FunctionId,
) -> CallablePlacement {
    match (current, dependency) {
        (CallablePlacement::Invalid { reason }, _) => CallablePlacement::Invalid { reason },
        (_, CallablePlacement::Invalid { .. }) => CallablePlacement::Invalid {
            reason: InvalidPlacementReason::InvalidDependency {
                function: dependency_function,
            },
        },
        (CallablePlacement::Common, CallablePlacement::Common) => CallablePlacement::Common,
        (CallablePlacement::Common, CallablePlacement::CapsuleScoped { .. }) => {
            CallablePlacement::CapsuleScoped {
                reason: CapsulePlacementReason::Dependency {
                    function: dependency_function,
                },
            }
        }
        (CallablePlacement::Common, CallablePlacement::CapsuleLocal { test, .. }) => {
            CallablePlacement::CapsuleLocal {
                test: test.clone(),
                reason: CapsulePlacementReason::Dependency {
                    function: dependency_function,
                },
            }
        }
        (CallablePlacement::CapsuleLocal { test, reason }, CallablePlacement::Common) => {
            CallablePlacement::CapsuleLocal { test, reason }
        }
        (CallablePlacement::CapsuleScoped { reason }, CallablePlacement::Common)
        | (CallablePlacement::CapsuleScoped { reason }, CallablePlacement::CapsuleScoped { .. }) => {
            CallablePlacement::CapsuleScoped { reason }
        }
        (
            CallablePlacement::CapsuleLocal { test, reason },
            CallablePlacement::CapsuleScoped { .. },
        ) => CallablePlacement::CapsuleLocal { test, reason },
        (CallablePlacement::CapsuleScoped { .. }, CallablePlacement::CapsuleLocal { .. }) => {
            CallablePlacement::Invalid {
                reason: InvalidPlacementReason::InvalidDependency {
                    function: dependency_function,
                },
            }
        }
        (
            CallablePlacement::CapsuleLocal {
                test: first_test,
                reason,
            },
            CallablePlacement::CapsuleLocal {
                test: second_test, ..
            },
        ) if first_test == *second_test => CallablePlacement::CapsuleLocal {
            test: first_test,
            reason,
        },
        (
            CallablePlacement::CapsuleLocal {
                test: first_test, ..
            },
            CallablePlacement::CapsuleLocal {
                test: second_test, ..
            },
        ) => CallablePlacement::Invalid {
            reason: InvalidPlacementReason::ConflictingCapsules {
                first_test,
                second_test: second_test.clone(),
                dependency: dependency_function,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: u32, placement: CallablePlacement, dependencies: &[u32]) -> CallablePlan {
        CallablePlan {
            function: FunctionId(index),
            name: format!("f{index}"),
            kind: CallableKind::Helper,
            owner: CallableOwner::Suite,
            placement,
            dependencies: dependencies.iter().copied().map(FunctionId).collect(),
        }
    }

    fn local(test: &str) -> CallablePlacement {
        CallablePlacement::CapsuleLocal {
            test: test.to_string(),
            reason: CapsulePlacementReason::TestBody,
        }
    }

    #[test]
    fn same_capsule_dependency_diamond_converges_to_one_local_owner() {
        let mut entries = vec![
            entry(0, CallablePlacement::Common, &[1, 2]),
            entry(1, CallablePlacement::Common, &[3]),
            entry(2, CallablePlacement::Common, &[3]),
            entry(3, local("test_3"), &[]),
        ];
        let components = strongly_connected_components(&entries);
        propagate_placements(&mut entries, &components);

        assert!(entries.iter().all(|entry| {
            matches!(
                entry.placement,
                CallablePlacement::CapsuleLocal { ref test, .. } if test == "test_3"
            )
        }));
    }

    #[test]
    fn conflicting_capsule_dependencies_fail_closed() {
        let mut entries = vec![
            entry(0, CallablePlacement::Common, &[1, 2]),
            entry(1, local("test_2"), &[]),
            entry(2, local("test_7"), &[]),
        ];
        let components = strongly_connected_components(&entries);
        propagate_placements(&mut entries, &components);

        assert_eq!(
            entries[0].placement,
            CallablePlacement::Invalid {
                reason: InvalidPlacementReason::ConflictingCapsules {
                    first_test: "test_2".to_string(),
                    second_test: "test_7".to_string(),
                    dependency: FunctionId(2),
                },
            }
        );
    }

    #[test]
    fn recursive_component_inherits_the_strongest_seed() {
        let mut entries = vec![
            entry(0, CallablePlacement::Common, &[1]),
            entry(1, local("test_4"), &[0]),
        ];
        let components = strongly_connected_components(&entries);
        assert_eq!(components, vec![vec![0, 1]]);
        propagate_placements(&mut entries, &components);

        assert!(entries.iter().all(|entry| {
            matches!(
                entry.placement,
                CallablePlacement::CapsuleLocal { ref test, .. } if test == "test_4"
            )
        }));
    }
}
