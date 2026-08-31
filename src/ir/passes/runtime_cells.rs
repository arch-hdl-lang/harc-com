//! Deterministic ownership plan for mutable state that outlives one emitted
//! statement evaluation.
//!
//! The plan is derived from verified IR and is intentionally backend-neutral.
//! Code generators consume these semantic identities instead of inventing
//! storage from output position or generated symbol spelling.

use crate::ir::{
    ComponentCallableId, ComponentFieldKind, ComponentId, ConstraintRef, CoverCheckId, CovgroupId,
    CycleEdge, CycleHandlerId, CycleHandlerKind, FunctionId, FunctionKind, HandlerPhase, IrType,
    LocalId, MethodHookTarget, PropertyCheckId, PropertyShape, ScoreboardId, Stmt, TbProgram,
    TestId, TestbenchId, TransactorId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuntimeCellId {
    owner: RuntimeCellOwner,
    site: RuntimeCellSiteId,
}

impl RuntimeCellId {
    pub fn owner(&self) -> &RuntimeCellOwner {
        &self.owner
    }

    pub fn site(&self) -> &RuntimeCellSiteId {
        &self.site
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeCallableSite {
    TestBody(crate::ir::TestCallableMember),
    Named(String),
}

impl RuntimeCallableSite {
    fn symbol(&self) -> String {
        match self {
            Self::TestBody(member) => member.symbol().to_string(),
            Self::Named(name) => format!("n{}_{}", name.len(), name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RuntimeStatementSiteId {
    callable: RuntimeCallableSite,
    ordinal: u32,
}

impl RuntimeStatementSiteId {
    fn symbol(&self) -> String {
        format!("{}_s{}", self.callable.symbol(), self.ordinal)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeCellSiteId {
    Rng,
    Solver,
    CallbackRegistry(CallbackRegistryKind),
    PropertyTemporal {
        statement: RuntimeStatementSiteId,
        slot: u32,
    },
    PropertyImplication {
        statement: RuntimeStatementSiteId,
    },
    CoverTemporal {
        statement: RuntimeStatementSiteId,
        slot: u32,
    },
    CoverHits {
        statement: RuntimeStatementSiteId,
    },
    StatementPeriodic(crate::ir::TestHookSiteId),
    StatementEdge(crate::ir::TestHookSiteId),
    TestbenchPeriodic(crate::ir::TestHookMember),
    TestbenchEdge(crate::ir::TestHookMember),
    ComponentPeriodic(String),
    ComponentEdge(String),
    ComponentCooldown(String),
    ComponentWatchdog(String),
    Heartbeat(ComponentHeartbeat),
    CoverageHook {
        member: String,
        side: RuntimeHookSide,
    },
    HookSubscribers {
        member: String,
        side: RuntimeHookSide,
    },
    LocalEvent {
        member: crate::ir::TestCallableMember,
        local: String,
    },
    ComponentEvent(String),
    AutomaticCoverage {
        function: String,
        covgroup: String,
    },
    Constraint(RuntimeStatementSiteId),
    PersistentLocal {
        member: crate::ir::TestCallableMember,
        local: String,
    },
    TestHookClosure(crate::ir::TestHookMember),
}

impl RuntimeCellSiteId {
    pub fn symbol(&self) -> String {
        let side = |side: RuntimeHookSide| match side {
            RuntimeHookSide::Pre => "pre",
            RuntimeHookSide::Post => "post",
        };
        let heartbeat = |heartbeat: ComponentHeartbeat| match heartbeat {
            ComponentHeartbeat::Input => "input",
            ComponentHeartbeat::Output => "output",
        };
        match self {
            Self::Rng => "rng".to_string(),
            Self::Solver => "solver".to_string(),
            Self::CallbackRegistry(registry) => format!("callback_{registry:?}").to_lowercase(),
            Self::PropertyTemporal { statement, slot } => {
                format!("property_{}_temporal_{slot}", statement.symbol())
            }
            Self::PropertyImplication { statement } => {
                format!("property_{}_implication", statement.symbol())
            }
            Self::CoverTemporal { statement, slot } => {
                format!("cover_{}_temporal_{slot}", statement.symbol())
            }
            Self::CoverHits { statement } => format!("cover_{}_hits", statement.symbol()),
            Self::StatementPeriodic(site) => format!("cycle_{}_last", site.symbol()),
            Self::StatementEdge(site) => format!("cycle_{}_previous", site.symbol()),
            Self::TestbenchPeriodic(member) => format!("{}_last", member.symbol()),
            Self::TestbenchEdge(member) => format!("{}_previous", member.symbol()),
            Self::ComponentPeriodic(member) => format!("component_{member}_last"),
            Self::ComponentEdge(member) => format!("component_{member}_previous"),
            Self::ComponentCooldown(member) => format!("component_{member}_cooldown"),
            Self::ComponentWatchdog(member) => format!("component_{member}_last"),
            Self::Heartbeat(kind) => format!("heartbeat_{}", heartbeat(*kind)),
            Self::CoverageHook {
                member,
                side: hook_side,
            } => {
                format!("coverage_hook_{member}_{}", side(*hook_side))
            }
            Self::HookSubscribers {
                member,
                side: hook_side,
            } => {
                format!("hook_{member}_{}", side(*hook_side))
            }
            Self::LocalEvent { member, local } => {
                format!("event_{}_n{}_{}", member.symbol(), local.len(), local)
            }
            Self::ComponentEvent(field) => {
                format!("component_event_n{}_{}", field.len(), field)
            }
            Self::AutomaticCoverage { function, covgroup } => format!(
                "coverage_n{}_{}_n{}_{}",
                function.len(),
                function,
                covgroup.len(),
                covgroup
            ),
            Self::Constraint(statement) => format!("constraint_{}", statement.symbol()),
            Self::PersistentLocal { member, local } => {
                format!(
                    "callback_capture_{}_n{}_{}",
                    member.symbol(),
                    local.len(),
                    local
                )
            }
            Self::TestHookClosure(member) => format!("test_hook_{}", member.symbol()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeCellOwner {
    Runtime,
    Test {
        test: TestId,
        name: String,
    },
    Testbench {
        testbench: TestbenchId,
        name: String,
    },
    /// A receiver-relative member. Every concrete instance of `component`
    /// owns a distinct copy of the planned cell.
    ComponentInstance {
        component: ComponentId,
        name: String,
    },
    ScoreboardInstance {
        scoreboard: ScoreboardId,
        name: String,
    },
    TransactorInstance {
        transactor: TransactorId,
        name: String,
    },
    Callable {
        function: FunctionId,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CallbackRegistryKind {
    Checker,
    PostEval,
    AutomaticCoverageReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeHandlerPhase {
    Checker,
    PostEval,
}

impl From<HandlerPhase> for RuntimeHandlerPhase {
    fn from(phase: HandlerPhase) -> Self {
        match phase {
            HandlerPhase::Checker => Self::Checker,
            HandlerPhase::PostEval => Self::PostEval,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeHookSide {
    Pre,
    Post,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ComponentHeartbeat {
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TemporalCheck {
    Property(PropertyCheckId),
    Cover(CoverCheckId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HookOwner {
    Component {
        component: ComponentId,
        member: ComponentCallableId,
    },
    Transactor {
        function: FunctionId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeCellKind {
    Rng,
    Solver,
    CallbackRegistry(CallbackRegistryKind),
    TemporalPrevious {
        check: TemporalCheck,
        slot: u32,
    },
    PropertyImplicationPrevious {
        property: PropertyCheckId,
    },
    CoverHits {
        cover: CoverCheckId,
    },
    StatementPeriodicLast {
        handler: CycleHandlerId,
    },
    StatementEdgePrevious {
        handler: CycleHandlerId,
    },
    TestbenchPeriodicLast {
        function: FunctionId,
        phase: RuntimeHandlerPhase,
    },
    TestbenchEdgePrevious {
        function: FunctionId,
        phase: RuntimeHandlerPhase,
    },
    ComponentPeriodicLast {
        member: ComponentCallableId,
    },
    ComponentEdgePrevious {
        member: ComponentCallableId,
    },
    ComponentCooldown {
        member: ComponentCallableId,
    },
    ComponentWatchdogLast {
        member: ComponentCallableId,
    },
    ComponentHeartbeat(ComponentHeartbeat),
    ScoreboardHeartbeat(ComponentHeartbeat),
    TransactorHeartbeat(ComponentHeartbeat),
    TestbenchHeartbeat(ComponentHeartbeat),
    ComponentCoverageHookSubscribers {
        component: ComponentId,
        member: ComponentCallableId,
        side: RuntimeHookSide,
    },
    TransactorCoverageHookSubscribers {
        function: FunctionId,
        side: RuntimeHookSide,
    },
    HookSubscribers {
        hook: HookOwner,
        side: RuntimeHookSide,
    },
    LocalEventSubscribers {
        member: crate::ir::TestCallableMember,
        event: LocalId,
    },
    ComponentEventSubscribers {
        field: u32,
    },
    AutomaticCoverage {
        function: FunctionId,
        covgroup: CovgroupId,
    },
    ConstraintState {
        site: ConstraintRef,
    },
    PersistentLocal {
        member: crate::ir::TestCallableMember,
        local: LocalId,
    },
    TestHookClosure {
        function: FunctionId,
        member: crate::ir::TestHookMember,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeCellStorage {
    RandomGenerator,
    Solver,
    CallbackRegistry,
    TemporalValue,
    Counter,
    CycleStamp,
    Latch,
    HookRegistry,
    EventRegistry,
    Coverage,
    Constraint,
    PersistentValue,
    CallbackBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeCellInitializer {
    DefaultConstructed,
    SeedFromEnvironment,
    Zero,
    False,
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RuntimeCellRegistrationPhase {
    RuntimeSetup,
    ComponentSetup,
    TestbenchSetup,
    TestSetup,
    StatementExecution,
    CoverageSetup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCell {
    id: RuntimeCellId,
    owner: RuntimeCellOwner,
    kind: RuntimeCellKind,
    storage: RuntimeCellStorage,
    initializer: RuntimeCellInitializer,
    registration: RuntimeCellRegistrationPhase,
}

impl RuntimeCell {
    pub fn id(&self) -> &RuntimeCellId {
        &self.id
    }

    pub fn owner(&self) -> &RuntimeCellOwner {
        &self.owner
    }

    pub fn kind(&self) -> &RuntimeCellKind {
        &self.kind
    }

    pub fn site(&self) -> &RuntimeCellSiteId {
        self.id.site()
    }

    pub fn symbol(&self) -> String {
        self.site().symbol()
    }

    pub fn storage(&self) -> RuntimeCellStorage {
        self.storage
    }

    pub fn initializer(&self) -> RuntimeCellInitializer {
        self.initializer
    }

    pub fn registration(&self) -> RuntimeCellRegistrationPhase {
        self.registration
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCellPlan {
    cells: Vec<RuntimeCell>,
}

impl RuntimeCellPlan {
    pub fn cells(&self) -> &[RuntimeCell] {
        &self.cells
    }

    pub fn for_owner<'a>(
        &'a self,
        owner: &'a RuntimeCellOwner,
    ) -> impl Iterator<Item = &'a RuntimeCell> + 'a {
        self.cells.iter().filter(move |cell| cell.owner() == owner)
    }

    pub fn find(&self, owner: &RuntimeCellOwner, kind: &RuntimeCellKind) -> Option<&RuntimeCell> {
        self.cells
            .iter()
            .find(|cell| cell.owner() == owner && cell.kind() == kind)
    }

    pub fn find_for_test(&self, test: TestId, kind: &RuntimeCellKind) -> Option<&RuntimeCell> {
        self.cells.iter().find(|cell| {
            matches!(cell.owner(), RuntimeCellOwner::Test { test: owner, .. } if *owner == test)
                && cell.kind() == kind
        })
    }

    pub fn find_for_testbench(
        &self,
        testbench: TestbenchId,
        kind: &RuntimeCellKind,
    ) -> Option<&RuntimeCell> {
        self.cells.iter().find(|cell| {
            matches!(cell.owner(), RuntimeCellOwner::Testbench { testbench: owner, .. } if *owner == testbench)
                && cell.kind() == kind
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCellError(pub String);

impl fmt::Display for RuntimeCellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RuntimeCellError {}

type CellKey = (RuntimeCellOwner, RuntimeCellKind);

fn insert(cells: &mut BTreeSet<CellKey>, owner: RuntimeCellOwner, kind: RuntimeCellKind) {
    cells.insert((owner, kind));
}

fn insert_unique(
    cells: &mut BTreeSet<CellKey>,
    owner: RuntimeCellOwner,
    kind: RuntimeCellKind,
    description: &str,
) -> Result<(), RuntimeCellError> {
    if !cells.insert((owner, kind)) {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan found duplicate {description}"
        )));
    }
    Ok(())
}

fn cell_contract(
    kind: &RuntimeCellKind,
) -> (
    RuntimeCellStorage,
    RuntimeCellInitializer,
    RuntimeCellRegistrationPhase,
) {
    use RuntimeCellInitializer as Initializer;
    use RuntimeCellKind as Kind;
    use RuntimeCellRegistrationPhase as Registration;
    use RuntimeCellStorage as Storage;

    match kind {
        Kind::Rng => (
            Storage::RandomGenerator,
            Initializer::SeedFromEnvironment,
            Registration::RuntimeSetup,
        ),
        Kind::Solver => (
            Storage::Solver,
            Initializer::DefaultConstructed,
            Registration::RuntimeSetup,
        ),
        Kind::CallbackRegistry(_) => (
            Storage::CallbackRegistry,
            Initializer::Empty,
            Registration::RuntimeSetup,
        ),
        Kind::TemporalPrevious { .. } => (
            Storage::TemporalValue,
            Initializer::Zero,
            Registration::StatementExecution,
        ),
        Kind::PropertyImplicationPrevious { .. }
        | Kind::StatementEdgePrevious { .. }
        | Kind::TestbenchEdgePrevious { .. }
        | Kind::ComponentEdgePrevious { .. }
        | Kind::ComponentCooldown { .. } => (
            Storage::Latch,
            Initializer::False,
            match kind {
                Kind::TestbenchEdgePrevious { .. } => Registration::TestbenchSetup,
                Kind::ComponentEdgePrevious { .. } | Kind::ComponentCooldown { .. } => {
                    Registration::ComponentSetup
                }
                _ => Registration::StatementExecution,
            },
        ),
        Kind::CoverHits { .. } => (
            Storage::Counter,
            Initializer::Zero,
            Registration::CoverageSetup,
        ),
        Kind::StatementPeriodicLast { .. } => (
            Storage::CycleStamp,
            Initializer::Zero,
            Registration::StatementExecution,
        ),
        Kind::TestbenchPeriodicLast { .. } => (
            Storage::CycleStamp,
            Initializer::Zero,
            Registration::TestbenchSetup,
        ),
        Kind::ComponentPeriodicLast { .. } | Kind::ComponentWatchdogLast { .. } => (
            Storage::CycleStamp,
            Initializer::Zero,
            Registration::ComponentSetup,
        ),
        Kind::ComponentHeartbeat(_)
        | Kind::ScoreboardHeartbeat(_)
        | Kind::TransactorHeartbeat(_) => (
            Storage::CycleStamp,
            Initializer::Zero,
            Registration::ComponentSetup,
        ),
        Kind::TestbenchHeartbeat(_) => (
            Storage::CycleStamp,
            Initializer::Zero,
            Registration::TestbenchSetup,
        ),
        Kind::HookSubscribers { .. } => (
            Storage::HookRegistry,
            Initializer::Empty,
            Registration::StatementExecution,
        ),
        Kind::LocalEventSubscribers { .. } => (
            Storage::EventRegistry,
            Initializer::Empty,
            Registration::StatementExecution,
        ),
        Kind::ComponentEventSubscribers { .. } => (
            Storage::EventRegistry,
            Initializer::Empty,
            Registration::ComponentSetup,
        ),
        Kind::ComponentCoverageHookSubscribers { .. }
        | Kind::TransactorCoverageHookSubscribers { .. } => (
            Storage::HookRegistry,
            Initializer::Empty,
            Registration::CoverageSetup,
        ),
        Kind::AutomaticCoverage { .. } => (
            Storage::Coverage,
            Initializer::DefaultConstructed,
            Registration::CoverageSetup,
        ),
        Kind::ConstraintState { .. } => (
            Storage::Constraint,
            Initializer::DefaultConstructed,
            Registration::StatementExecution,
        ),
        Kind::PersistentLocal { .. } => (
            Storage::PersistentValue,
            Initializer::DefaultConstructed,
            Registration::StatementExecution,
        ),
        Kind::TestHookClosure { member, .. } => (
            Storage::CallbackBody,
            Initializer::Empty,
            match member {
                crate::ir::TestHookMember::EventSubscription(_)
                | crate::ir::TestHookMember::MethodSubscription(_)
                | crate::ir::TestHookMember::StatementCycle(_) => Registration::TestSetup,
                _ => Registration::TestbenchSetup,
            },
        ),
    }
}

fn test_owner(prog: &TbProgram, test: TestId) -> Result<RuntimeCellOwner, RuntimeCellError> {
    let mut matches = prog.tests.iter().filter(|schema| schema.id == test);
    let schema = matches.next().ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing test t{}",
            test.0
        ))
    })?;
    if matches.next().is_some() {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan found duplicate test identity t{}",
            test.0
        )));
    }
    Ok(RuntimeCellOwner::Test {
        test,
        name: schema.name.clone(),
    })
}

fn test_owner_by_name(prog: &TbProgram, name: &str) -> Result<RuntimeCellOwner, RuntimeCellError> {
    let mut matches = prog.tests.iter().filter(|schema| schema.name == name);
    let schema = matches.next().ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing test `{name}`"
        ))
    })?;
    if matches.next().is_some() {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan found duplicate test name `{name}`"
        )));
    }
    Ok(RuntimeCellOwner::Test {
        test: schema.id,
        name: schema.name.clone(),
    })
}

fn testbench_owner(
    prog: &TbProgram,
    testbench: TestbenchId,
) -> Result<RuntimeCellOwner, RuntimeCellError> {
    let schema = prog.testbenches.get(testbench.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing testbench tb{}",
            testbench.0
        ))
    })?;
    Ok(RuntimeCellOwner::Testbench {
        testbench,
        name: schema.name.clone(),
    })
}

fn component_owner(
    prog: &TbProgram,
    component: ComponentId,
) -> Result<RuntimeCellOwner, RuntimeCellError> {
    let schema = prog.components.get(component.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing component c{}",
            component.0
        ))
    })?;
    Ok(RuntimeCellOwner::ComponentInstance {
        component,
        name: schema.name.clone(),
    })
}

fn scoreboard_owner(
    prog: &TbProgram,
    scoreboard: ScoreboardId,
) -> Result<RuntimeCellOwner, RuntimeCellError> {
    let schema = prog.scoreboards.get(scoreboard.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing scoreboard s{}",
            scoreboard.0
        ))
    })?;
    Ok(RuntimeCellOwner::ScoreboardInstance {
        scoreboard,
        name: schema.name.clone(),
    })
}

fn transactor_owner(
    prog: &TbProgram,
    transactor: TransactorId,
) -> Result<RuntimeCellOwner, RuntimeCellError> {
    let schema = prog.transactors.get(transactor.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing transactor x{}",
            transactor.0
        ))
    })?;
    Ok(RuntimeCellOwner::TransactorInstance {
        transactor,
        name: schema.name.clone(),
    })
}

fn component_method_member(
    prog: &TbProgram,
    component: ComponentId,
    method: &str,
) -> Result<ComponentCallableId, RuntimeCellError> {
    let schema = prog.components.get(component.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan hook references missing component c{}",
            component.0
        ))
    })?;
    let member = schema
        .methods
        .iter()
        .position(|candidate| candidate.name == method)
        .map(|index| ComponentCallableId(index as u32))
        .ok_or_else(|| {
            RuntimeCellError(format!(
                "runtime-cell plan hook references missing method `{method}` on component `{}`",
                schema.name
            ))
        })?;
    let method_schema = &schema.methods[member.index()];
    validate_component_callable(
        prog,
        component,
        member,
        method_schema.function,
        &format!(
            "component `{}` method `{}`",
            schema.name, method_schema.name
        ),
    )?;
    Ok(member)
}

fn validate_test_hook(
    prog: &TbProgram,
    testbench: Option<TestbenchId>,
    function: FunctionId,
    expected: &crate::ir::TestHookMember,
    description: &str,
) -> Result<(), RuntimeCellError> {
    let body = prog.functions.get(function.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan {description} references missing fn{}",
            function.0
        ))
    })?;
    let expected_test = match expected {
        crate::ir::TestHookMember::EventSubscription(site)
        | crate::ir::TestHookMember::MethodSubscription(site)
        | crate::ir::TestHookMember::StatementCycle(site) => {
            prog.tests.iter().find(|test| test.name == site.owner.test)
        }
        _ => testbench.and_then(|owner| {
            let mut tests = prog.tests.iter().filter(|test| test.testbench == owner);
            let first = tests.next()?;
            tests.next().is_none().then_some(first)
        }),
    };
    let expected_name = expected_test.map(|test| expected.function_name(&test.name));
    if body.id != function
        || !matches!(&body.kind, FunctionKind::TestHook { member } if member == expected)
        || body.owner != testbench
        || expected_test.is_none_or(|test| Some(test.testbench) != testbench)
        || expected_name.as_deref() != Some(body.name.as_str())
    {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan {description} has invalid body fn{}",
            function.0
        )));
    }
    Ok(())
}

fn validate_component_callable(
    prog: &TbProgram,
    component: ComponentId,
    member: ComponentCallableId,
    function: FunctionId,
    description: &str,
) -> Result<(), RuntimeCellError> {
    let body = prog.functions.get(function.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan {description} references missing fn{}",
            function.0
        ))
    })?;
    if body.id != function
        || !matches!(
            body.kind,
            FunctionKind::ComponentMethod {
                component: owner,
                member: owner_member,
                ..
            } if owner == component && owner_member == member
        )
    {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan {description} has invalid callable fn{}",
            function.0
        )));
    }
    Ok(())
}

fn add_temporal_cells(
    cells: &mut BTreeSet<CellKey>,
    owner: &RuntimeCellOwner,
    check: TemporalCheck,
    count: usize,
) {
    for slot in 0..count {
        insert(
            cells,
            owner.clone(),
            RuntimeCellKind::TemporalPrevious {
                check,
                slot: slot as u32,
            },
        );
    }
}

#[derive(Clone, Copy)]
enum StatementCellTarget {
    Property(PropertyCheckId),
    Cover(CoverCheckId),
    Constraint(ConstraintRef),
}

fn runtime_callable_site(function: &crate::ir::TbFunction) -> RuntimeCallableSite {
    match &function.kind {
        FunctionKind::TestBody { member, .. } => RuntimeCallableSite::TestBody(*member),
        _ => RuntimeCallableSite::Named(function.name.clone()),
    }
}

fn statement_site(
    prog: &TbProgram,
    target: StatementCellTarget,
) -> Result<RuntimeStatementSiteId, RuntimeCellError> {
    for function in &prog.functions {
        let mut ordinal = 0u32;
        for block in &function.blocks {
            let matched = match target {
                StatementCellTarget::Property(target) => block
                    .stmts
                    .iter()
                    .filter_map(|stmt| match stmt {
                        Stmt::PropertyCheck(property) => Some(*property),
                        _ => None,
                    })
                    .find_map(|property| {
                        let current = ordinal;
                        ordinal += 1;
                        (property == target).then_some(current)
                    }),
                StatementCellTarget::Cover(target) => block
                    .stmts
                    .iter()
                    .filter_map(|stmt| match stmt {
                        Stmt::CoverCheck(cover) => Some(*cover),
                        _ => None,
                    })
                    .find_map(|cover| {
                        let current = ordinal;
                        ordinal += 1;
                        (cover == target).then_some(current)
                    }),
                StatementCellTarget::Constraint(target) => {
                    if let crate::ir::Terminator::Randomize { constraints, .. } = block.terminator {
                        let current = ordinal;
                        ordinal += 1;
                        (constraints == target).then_some(current)
                    } else {
                        None
                    }
                }
            };
            if let Some(ordinal) = matched {
                return Ok(RuntimeStatementSiteId {
                    callable: runtime_callable_site(function),
                    ordinal,
                });
            }
        }
    }
    let description = match target {
        StatementCellTarget::Property(id) => format!("property p{}", id.0),
        StatementCellTarget::Cover(id) => format!("cover c{}", id.0),
        StatementCellTarget::Constraint(id) => format!("constraint c{}", id.0),
    };
    Err(RuntimeCellError(format!(
        "runtime-cell plan cannot resolve stable source site for {description}"
    )))
}

fn test_hook_member(
    prog: &TbProgram,
    function: FunctionId,
) -> Result<crate::ir::TestHookMember, RuntimeCellError> {
    let body = prog.functions.get(function.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing callback fn{}",
            function.0
        ))
    })?;
    match &body.kind {
        FunctionKind::TestHook { member } => Ok(member.clone()),
        _ => Err(RuntimeCellError(format!(
            "runtime-cell plan callback fn{} `{}` is not a TestHook",
            function.0, body.name
        ))),
    }
}

fn component_member_name(
    prog: &TbProgram,
    component: ComponentId,
    member: ComponentCallableId,
) -> Result<String, RuntimeCellError> {
    let schema = prog.components.get(component.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing component c{}",
            component.0
        ))
    })?;
    let index = member.index();
    let name = if let Some(method) = schema.methods.get(index) {
        format!("method_n{}_{}", method.name.len(), method.name)
    } else {
        let index = index - schema.methods.len();
        if let Some(handler) = schema.on_handlers.get(index) {
            format!("event_h{index}_n{}_{}", handler.event.len(), handler.event)
        } else {
            let index = index - schema.on_handlers.len();
            if index < schema.periodic_handlers.len() {
                format!("periodic_{index}")
            } else {
                let index = index - schema.periodic_handlers.len();
                if index < schema.cycle_handlers.len() {
                    format!("cycle_{index}")
                } else if index == schema.cycle_handlers.len() && schema.watchdog.is_some() {
                    "watchdog".to_string()
                } else {
                    return Err(RuntimeCellError(format!(
                        "runtime-cell plan cannot resolve component `{}` member {}",
                        schema.name, member.0
                    )));
                }
            }
        }
    };
    Ok(name)
}

fn transactor_member_name(
    prog: &TbProgram,
    function: FunctionId,
) -> Result<String, RuntimeCellError> {
    let body = prog.functions.get(function.index()).ok_or_else(|| {
        RuntimeCellError(format!(
            "runtime-cell plan references missing transactor fn{}",
            function.0
        ))
    })?;
    if !matches!(body.kind, FunctionKind::TransactorBody { .. }) {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan fn{} `{}` is not a transactor callable",
            function.0, body.name
        )));
    }
    Ok(format!("n{}_{}", body.name.len(), body.name))
}

fn runtime_cell_site(
    prog: &TbProgram,
    owner: &RuntimeCellOwner,
    kind: &RuntimeCellKind,
) -> Result<RuntimeCellSiteId, RuntimeCellError> {
    use RuntimeCellKind as Kind;
    Ok(match kind {
        Kind::Rng => RuntimeCellSiteId::Rng,
        Kind::Solver => RuntimeCellSiteId::Solver,
        Kind::CallbackRegistry(registry) => RuntimeCellSiteId::CallbackRegistry(*registry),
        Kind::TemporalPrevious { check, slot } => match check {
            TemporalCheck::Property(property) => RuntimeCellSiteId::PropertyTemporal {
                statement: statement_site(prog, StatementCellTarget::Property(*property))?,
                slot: *slot,
            },
            TemporalCheck::Cover(cover) => RuntimeCellSiteId::CoverTemporal {
                statement: statement_site(prog, StatementCellTarget::Cover(*cover))?,
                slot: *slot,
            },
        },
        Kind::PropertyImplicationPrevious { property } => RuntimeCellSiteId::PropertyImplication {
            statement: statement_site(prog, StatementCellTarget::Property(*property))?,
        },
        Kind::CoverHits { cover } => RuntimeCellSiteId::CoverHits {
            statement: statement_site(prog, StatementCellTarget::Cover(*cover))?,
        },
        Kind::StatementPeriodicLast { handler } => RuntimeCellSiteId::StatementPeriodic(
            prog.cycle_handlers
                .get(handler.index())
                .ok_or_else(|| {
                    RuntimeCellError(format!(
                        "runtime-cell plan references missing cycle handler h{}",
                        handler.0
                    ))
                })?
                .site
                .clone(),
        ),
        Kind::StatementEdgePrevious { handler } => RuntimeCellSiteId::StatementEdge(
            prog.cycle_handlers
                .get(handler.index())
                .ok_or_else(|| {
                    RuntimeCellError(format!(
                        "runtime-cell plan references missing cycle handler h{}",
                        handler.0
                    ))
                })?
                .site
                .clone(),
        ),
        Kind::TestbenchPeriodicLast { function, .. } => {
            RuntimeCellSiteId::TestbenchPeriodic(test_hook_member(prog, *function)?)
        }
        Kind::TestbenchEdgePrevious { function, .. } => {
            RuntimeCellSiteId::TestbenchEdge(test_hook_member(prog, *function)?)
        }
        Kind::ComponentPeriodicLast { member } => {
            let RuntimeCellOwner::ComponentInstance { component, .. } = owner else {
                return Err(RuntimeCellError(
                    "component periodic cell has a non-component owner".to_string(),
                ));
            };
            RuntimeCellSiteId::ComponentPeriodic(component_member_name(prog, *component, *member)?)
        }
        Kind::ComponentEdgePrevious { member } => {
            let RuntimeCellOwner::ComponentInstance { component, .. } = owner else {
                return Err(RuntimeCellError(
                    "component edge cell has a non-component owner".to_string(),
                ));
            };
            RuntimeCellSiteId::ComponentEdge(component_member_name(prog, *component, *member)?)
        }
        Kind::ComponentCooldown { member } => {
            let RuntimeCellOwner::ComponentInstance { component, .. } = owner else {
                return Err(RuntimeCellError(
                    "component cooldown cell has a non-component owner".to_string(),
                ));
            };
            RuntimeCellSiteId::ComponentCooldown(component_member_name(prog, *component, *member)?)
        }
        Kind::ComponentWatchdogLast { member } => {
            let RuntimeCellOwner::ComponentInstance { component, .. } = owner else {
                return Err(RuntimeCellError(
                    "component watchdog cell has a non-component owner".to_string(),
                ));
            };
            RuntimeCellSiteId::ComponentWatchdog(component_member_name(prog, *component, *member)?)
        }
        Kind::ComponentHeartbeat(heartbeat)
        | Kind::ScoreboardHeartbeat(heartbeat)
        | Kind::TransactorHeartbeat(heartbeat)
        | Kind::TestbenchHeartbeat(heartbeat) => RuntimeCellSiteId::Heartbeat(*heartbeat),
        Kind::ComponentCoverageHookSubscribers {
            component,
            member,
            side,
        } => RuntimeCellSiteId::CoverageHook {
            member: component_member_name(prog, *component, *member)?,
            side: *side,
        },
        Kind::TransactorCoverageHookSubscribers { function, side } => {
            RuntimeCellSiteId::CoverageHook {
                member: transactor_member_name(prog, *function)?,
                side: *side,
            }
        }
        Kind::HookSubscribers { hook, side } => RuntimeCellSiteId::HookSubscribers {
            member: match hook {
                HookOwner::Component { component, member } => {
                    component_member_name(prog, *component, *member)?
                }
                HookOwner::Transactor { function } => transactor_member_name(prog, *function)?,
            },
            side: *side,
        },
        Kind::LocalEventSubscribers { member, event } => {
            let RuntimeCellOwner::Test { test, .. } = owner else {
                return Err(RuntimeCellError(
                    "local event cell has a non-test owner".to_string(),
                ));
            };
            let test = prog.tests.get(test.index()).ok_or_else(|| {
                RuntimeCellError("local event cell references missing test".to_string())
            })?;
            let function = match member {
                crate::ir::TestCallableMember::Run => test.run,
                crate::ir::TestCallableMember::Check => test.check.ok_or_else(|| {
                    RuntimeCellError("local event cell references missing check".to_string())
                })?,
            };
            let local = prog
                .function(function)
                .locals
                .get(event.index())
                .ok_or_else(|| {
                    RuntimeCellError("local event cell references missing local".to_string())
                })?;
            RuntimeCellSiteId::LocalEvent {
                member: *member,
                local: local.name.clone(),
            }
        }
        Kind::ComponentEventSubscribers { field } => {
            let RuntimeCellOwner::ComponentInstance { component, .. } = owner else {
                return Err(RuntimeCellError(
                    "component event cell has a non-component owner".to_string(),
                ));
            };
            let field = prog.components[component.index()]
                .fields
                .get(*field as usize)
                .ok_or_else(|| {
                    RuntimeCellError("component event cell references missing field".to_string())
                })?;
            RuntimeCellSiteId::ComponentEvent(field.name.clone())
        }
        Kind::AutomaticCoverage { function, covgroup } => {
            let function = prog.functions.get(function.index()).ok_or_else(|| {
                RuntimeCellError("coverage cell references missing function".to_string())
            })?;
            let covgroup = prog.covgroups.get(covgroup.index()).ok_or_else(|| {
                RuntimeCellError("coverage cell references missing covgroup".to_string())
            })?;
            RuntimeCellSiteId::AutomaticCoverage {
                function: function.name.clone(),
                covgroup: covgroup.name.clone(),
            }
        }
        Kind::ConstraintState { site } => RuntimeCellSiteId::Constraint(statement_site(
            prog,
            StatementCellTarget::Constraint(*site),
        )?),
        Kind::PersistentLocal { member, local } => {
            let RuntimeCellOwner::Test { test, .. } = owner else {
                return Err(RuntimeCellError(
                    "persistent local cell has a non-test owner".to_string(),
                ));
            };
            let test = prog.tests.get(test.index()).ok_or_else(|| {
                RuntimeCellError("persistent local cell references missing test".to_string())
            })?;
            let function = match member {
                crate::ir::TestCallableMember::Run => test.run,
                crate::ir::TestCallableMember::Check => test.check.ok_or_else(|| {
                    RuntimeCellError("persistent local cell references missing check".to_string())
                })?,
            };
            let local = prog
                .function(function)
                .locals
                .get(local.index())
                .ok_or_else(|| {
                    RuntimeCellError("persistent local cell references missing local".to_string())
                })?;
            RuntimeCellSiteId::PersistentLocal {
                member: *member,
                local: local.name.clone(),
            }
        }
        Kind::TestHookClosure { member, .. } => RuntimeCellSiteId::TestHookClosure(member.clone()),
    })
}

fn collect_expr_locals(expr: &crate::ir::Expr, locals: &mut BTreeSet<LocalId>) {
    use crate::ir::{ComponentBase, Expr};
    crate::ir::visit::walk_expr(expr, &mut |expr| match expr {
        Expr::RegRead { mirror, .. }
        | Expr::Local(mirror)
        | Expr::SeqLen(mirror)
        | Expr::PortSnapshotLane {
            snapshot: mirror, ..
        } => {
            locals.insert(*mirror);
        }
        Expr::RecordField { local, .. } | Expr::SeqIndex { seq: local, .. } => {
            locals.insert(*local);
        }
        Expr::ComponentValue {
            base: ComponentBase::Local(local),
        }
        | Expr::ComponentIdle {
            base: ComponentBase::Local(local),
            ..
        } => {
            locals.insert(*local);
        }
        _ => {}
    });
}

pub fn persistent_callback_captures(
    prog: &TbProgram,
    function: &crate::ir::TbFunction,
) -> Result<BTreeSet<LocalId>, RuntimeCellError> {
    let mut captures = BTreeSet::new();
    for block in &function.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::PropertyCheck(property) => {
                    let schema = prog.property_checks.get(property.index()).ok_or_else(|| {
                        RuntimeCellError(format!(
                            "persistent-capture analysis references missing property p{}",
                            property.0
                        ))
                    })?;
                    match &schema.shape {
                        PropertyShape::Implies { ante, cons }
                        | PropertyShape::ImpliesNext { ante, cons } => {
                            collect_expr_locals(ante, &mut captures);
                            collect_expr_locals(cons, &mut captures);
                        }
                        PropertyShape::Invariant(expr) => {
                            collect_expr_locals(expr, &mut captures);
                        }
                    }
                    for temporal in &schema.temporals {
                        collect_expr_locals(&temporal.inner, &mut captures);
                    }
                    if let Some(message) = &schema.message {
                        for arg in &message.args {
                            collect_expr_locals(&arg.expr, &mut captures);
                        }
                    }
                }
                Stmt::CoverCheck(cover) => {
                    let schema = prog.cover_checks.get(cover.index()).ok_or_else(|| {
                        RuntimeCellError(format!(
                            "persistent-capture analysis references missing cover c{}",
                            cover.0
                        ))
                    })?;
                    collect_expr_locals(&schema.cond, &mut captures);
                    for temporal in &schema.temporals {
                        collect_expr_locals(&temporal.inner, &mut captures);
                    }
                }
                Stmt::CycleHandler(handler) => {
                    let schema = prog.cycle_handlers.get(handler.index()).ok_or_else(|| {
                        RuntimeCellError(format!(
                            "persistent-capture analysis references missing cycle handler h{}",
                            handler.0
                        ))
                    })?;
                    if let CycleHandlerKind::Trigger { trigger, .. } = &schema.kind {
                        collect_expr_locals(trigger, &mut captures);
                    }
                }
                Stmt::MethodHookSubscribe {
                    captures: hook_captures,
                    ..
                } => captures.extend(hook_captures.iter().copied()),
                Stmt::EventSubscribe {
                    event: crate::ir::EventChannelRef::Local(local),
                    ..
                }
                | Stmt::EventEmit { event: local, .. } => {
                    captures.insert(*local);
                }
                _ => {}
            }
        }
    }

    let testbench_record_locals = function
        .testbench_record_locals
        .iter()
        .map(|binding| binding.local)
        .collect::<BTreeSet<_>>();
    captures.retain(|local| !testbench_record_locals.contains(local));
    for local in &captures {
        if function.locals.get(local.index()).is_none() {
            return Err(RuntimeCellError(format!(
                "persistent-capture analysis for fn{} references missing local %{}",
                function.id.0, local.0
            )));
        }
    }
    Ok(captures)
}

pub fn analyze(prog: &TbProgram) -> Result<RuntimeCellPlan, RuntimeCellError> {
    let mut cells = BTreeSet::new();
    insert(&mut cells, RuntimeCellOwner::Runtime, RuntimeCellKind::Rng);
    for registry in [
        CallbackRegistryKind::Checker,
        CallbackRegistryKind::PostEval,
        CallbackRegistryKind::AutomaticCoverageReport,
    ] {
        insert(
            &mut cells,
            RuntimeCellOwner::Runtime,
            RuntimeCellKind::CallbackRegistry(registry),
        );
    }
    if !prog.constraint_sites.is_empty() {
        insert(
            &mut cells,
            RuntimeCellOwner::Runtime,
            RuntimeCellKind::Solver,
        );
    }

    let mut property_owners = BTreeMap::new();
    let mut cover_owners = BTreeMap::new();
    let mut handler_owners = BTreeMap::new();
    let mut selected_tests = BTreeSet::new();
    for test in &prog.tests {
        if !selected_tests.insert(test.id) {
            return Err(RuntimeCellError(format!(
                "runtime-cell plan found duplicate test identity t{}",
                test.id.0
            )));
        }
        for (member, function) in [
            (crate::ir::TestCallableMember::Run, Some(test.run)),
            (crate::ir::TestCallableMember::Check, test.check),
        ] {
            let Some(function) = function else {
                continue;
            };
            let body = prog.functions.get(function.index()).ok_or_else(|| {
                RuntimeCellError(format!(
                    "runtime-cell plan test `{}` references missing fn{}",
                    test.name, function.0
                ))
            })?;
            if body.id != function
                || !matches!(
                    &body.kind,
                    FunctionKind::TestBody {
                        test: owner,
                        member: owner_member,
                        name,
                    } if *owner == test.id && *owner_member == member && name == &test.name
                )
            {
                return Err(RuntimeCellError(format!(
                    "runtime-cell plan found mismatched id or member for test `{}` fn{}",
                    test.name, function.0
                )));
            }
        }
    }
    let complete_test_table = prog
        .tests
        .iter()
        .enumerate()
        .all(|(index, test)| test.id == TestId(index as u32))
        && !prog.functions.iter().any(|function| {
            matches!(
                function.kind,
                FunctionKind::TestBody { test, .. } if !selected_tests.contains(&test)
            )
        });
    for function in &prog.functions {
        let function_owner = match &function.kind {
            FunctionKind::TestBody { test, .. } if selected_tests.contains(test) => {
                test_owner(prog, *test)?
            }
            FunctionKind::TestBody { .. } => continue,
            FunctionKind::TestHook { member } => match member {
                crate::ir::TestHookMember::EventSubscription(site)
                | crate::ir::TestHookMember::MethodSubscription(site)
                | crate::ir::TestHookMember::StatementCycle(site) => {
                    if !prog.tests.iter().any(|test| test.name == site.owner.test) {
                        if complete_test_table {
                            return Err(RuntimeCellError(format!(
                                "runtime-cell plan references missing test `{}`",
                                site.owner.test
                            )));
                        }
                        continue;
                    }
                    test_owner_by_name(prog, &site.owner.test)?
                }
                _ => testbench_owner(
                    prog,
                    function.owner.ok_or_else(|| {
                        RuntimeCellError(format!(
                            "runtime-cell plan test hook fn{} has no testbench owner",
                            function.id.0
                        ))
                    })?,
                )?,
            },
            _ => RuntimeCellOwner::Callable {
                function: function.id,
                name: function.name.clone(),
            },
        };
        if let FunctionKind::TestHook { member } = &function.kind {
            insert(
                &mut cells,
                function_owner.clone(),
                RuntimeCellKind::TestHookClosure {
                    function: function.id,
                    member: member.clone(),
                },
            );
        }
        if let FunctionKind::TestBody { member, .. } = function.kind {
            for local in persistent_callback_captures(prog, function)? {
                if matches!(function.locals[local.index()].ty, IrType::Event(_)) {
                    continue;
                }
                insert(
                    &mut cells,
                    function_owner.clone(),
                    RuntimeCellKind::PersistentLocal { member, local },
                );
            }
        }
        for (local, schema) in function.locals.iter().enumerate() {
            if let (IrType::Event(_), FunctionKind::TestBody { member, .. }) =
                (&schema.ty, &function.kind)
            {
                insert(
                    &mut cells,
                    function_owner.clone(),
                    RuntimeCellKind::LocalEventSubscribers {
                        member: *member,
                        event: LocalId(local as u32),
                    },
                );
            }
        }
        for block in &function.blocks {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::PropertyCheck(property) => {
                        let owner = function_owner.clone();
                        if property_owners.insert(*property, owner.clone()).is_some() {
                            return Err(RuntimeCellError(format!(
                                "property p{} has more than one runtime owner",
                                property.0
                            )));
                        }
                        let schema =
                            prog.property_checks.get(property.index()).ok_or_else(|| {
                                RuntimeCellError(format!(
                                    "runtime-cell plan references missing property p{}",
                                    property.0
                                ))
                            })?;
                        add_temporal_cells(
                            &mut cells,
                            &owner,
                            TemporalCheck::Property(*property),
                            schema.temporals.len(),
                        );
                        if matches!(schema.shape, PropertyShape::ImpliesNext { .. }) {
                            insert(
                                &mut cells,
                                owner,
                                RuntimeCellKind::PropertyImplicationPrevious {
                                    property: *property,
                                },
                            );
                        }
                    }
                    Stmt::CoverCheck(cover) => {
                        let owner = function_owner.clone();
                        if cover_owners.insert(*cover, owner.clone()).is_some() {
                            return Err(RuntimeCellError(format!(
                                "cover c{} has more than one runtime owner",
                                cover.0
                            )));
                        }
                        let schema = prog.cover_checks.get(cover.index()).ok_or_else(|| {
                            RuntimeCellError(format!(
                                "runtime-cell plan references missing cover c{}",
                                cover.0
                            ))
                        })?;
                        add_temporal_cells(
                            &mut cells,
                            &owner,
                            TemporalCheck::Cover(*cover),
                            schema.temporals.len(),
                        );
                        insert(
                            &mut cells,
                            owner,
                            RuntimeCellKind::CoverHits { cover: *cover },
                        );
                    }
                    Stmt::CycleHandler(handler) => {
                        let owner = function_owner.clone();
                        if handler_owners.insert(*handler, owner.clone()).is_some() {
                            return Err(RuntimeCellError(format!(
                                "cycle handler h{} has more than one runtime owner",
                                handler.0
                            )));
                        }
                        let schema = prog.cycle_handlers.get(handler.index()).ok_or_else(|| {
                            RuntimeCellError(format!(
                                "runtime-cell plan references missing cycle handler h{}",
                                handler.0
                            ))
                        })?;
                        validate_test_hook(
                            prog,
                            function.owner,
                            schema.function,
                            &crate::ir::TestHookMember::StatementCycle(schema.site.clone()),
                            &format!("statement cycle handler h{}", handler.0),
                        )?;
                        let kind = match schema.kind {
                            CycleHandlerKind::Periodic { .. } => {
                                RuntimeCellKind::StatementPeriodicLast { handler: *handler }
                            }
                            CycleHandlerKind::Trigger {
                                edge: CycleEdge::Rising | CycleEdge::Falling,
                                ..
                            } => RuntimeCellKind::StatementEdgePrevious { handler: *handler },
                            CycleHandlerKind::Trigger {
                                edge: CycleEdge::Level,
                                ..
                            } => continue,
                        };
                        insert(&mut cells, owner, kind);
                    }
                    Stmt::EventSubscribe { site, handler, .. } => {
                        validate_test_hook(
                            prog,
                            function.owner,
                            *handler,
                            &crate::ir::TestHookMember::EventSubscription(site.clone()),
                            &format!("event subscription {}", site.symbol()),
                        )?;
                    }
                    Stmt::MethodHookSubscribe {
                        site,
                        target,
                        handler,
                        ..
                    } => {
                        validate_test_hook(
                            prog,
                            function.owner,
                            *handler,
                            &crate::ir::TestHookMember::MethodSubscription(site.clone()),
                            &format!("method subscription {}", site.symbol()),
                        )?;
                        match target {
                            MethodHookTarget::Component {
                                component, method, ..
                            } => {
                                let owner = component_owner(prog, *component)?;
                                let member = component_method_member(prog, *component, method)?;
                                for side in [RuntimeHookSide::Pre, RuntimeHookSide::Post] {
                                    insert(
                                        &mut cells,
                                        owner.clone(),
                                        RuntimeCellKind::HookSubscribers {
                                            hook: HookOwner::Component {
                                                component: *component,
                                                member,
                                            },
                                            side,
                                        },
                                    );
                                }
                            }
                            MethodHookTarget::Transactor {
                                transactor, method, ..
                            } => {
                                let schema =
                                    prog.transactors.get(transactor.index()).ok_or_else(|| {
                                        RuntimeCellError(format!(
                                        "runtime-cell plan hook references missing transactor x{}",
                                        transactor.0
                                    ))
                                    })?;
                                let method = schema.method(method).ok_or_else(|| {
                                RuntimeCellError(format!(
                                    "runtime-cell plan hook references missing method on transactor `{}`",
                                    schema.name
                                ))
                            })?;
                                let owner = transactor_owner(prog, *transactor)?;
                                for side in [RuntimeHookSide::Pre, RuntimeHookSide::Post] {
                                    insert(
                                        &mut cells,
                                        owner.clone(),
                                        RuntimeCellKind::HookSubscribers {
                                            hook: HookOwner::Transactor {
                                                function: method.function,
                                            },
                                            side,
                                        },
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        for block in &function.blocks {
            if let crate::ir::Terminator::Randomize { constraints, .. } = block.terminator {
                if prog.constraint_sites.get(constraints.index()).is_none() {
                    return Err(RuntimeCellError(format!(
                        "runtime-cell plan references missing constraint site c{}",
                        constraints.0
                    )));
                }
                insert(
                    &mut cells,
                    function_owner.clone(),
                    RuntimeCellKind::ConstraintState { site: constraints },
                );
            }
        }
    }

    if complete_test_table && property_owners.len() != prog.property_checks.len() {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan found {} owned properties for {} property schemas",
            property_owners.len(),
            prog.property_checks.len()
        )));
    }
    if complete_test_table && cover_owners.len() != prog.cover_checks.len() {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan found {} owned covers for {} cover schemas",
            cover_owners.len(),
            prog.cover_checks.len()
        )));
    }
    if complete_test_table && handler_owners.len() != prog.cycle_handlers.len() {
        return Err(RuntimeCellError(format!(
            "runtime-cell plan found {} owned statement handlers for {} handler schemas",
            handler_owners.len(),
            prog.cycle_handlers.len()
        )));
    }

    for (index, testbench) in prog.testbenches.iter().enumerate() {
        let testbench_id = TestbenchId(index as u32);
        let owner = testbench_owner(prog, testbench_id)?;
        if !testbench.synthetic
            || !testbench.state_fields.is_empty()
            || !testbench.record_fields.is_empty()
        {
            for heartbeat in [ComponentHeartbeat::Input, ComponentHeartbeat::Output] {
                insert(
                    &mut cells,
                    owner.clone(),
                    RuntimeCellKind::TestbenchHeartbeat(heartbeat),
                );
            }
        }
        for binding in &testbench.regblock_bindings {
            for (register, function) in &binding.callbacks {
                validate_test_hook(
                    prog,
                    Some(testbench_id),
                    *function,
                    &crate::ir::TestHookMember::RegblockWrite {
                        binding: binding.field.clone(),
                        register: register.clone(),
                    },
                    &format!(
                        "testbench `{}` register callback {}.{register}",
                        testbench.name, binding.field
                    ),
                )?;
            }
        }
        for (service_index, service) in testbench.periodic_services.iter().enumerate() {
            validate_test_hook(
                prog,
                Some(testbench_id),
                service.function,
                &crate::ir::TestHookMember::TestbenchPeriodic {
                    service: service_index as u32,
                },
                &format!("testbench `{}` periodic service", testbench.name),
            )?;
            insert_unique(
                &mut cells,
                owner.clone(),
                RuntimeCellKind::TestbenchPeriodicLast {
                    function: service.function,
                    phase: service.phase.into(),
                },
                &format!(
                    "testbench `{}` periodic service fn{}",
                    testbench.name, service.function.0
                ),
            )?;
        }
        for (service_index, service) in testbench.cycle_services.iter().enumerate() {
            validate_test_hook(
                prog,
                Some(testbench_id),
                service.function,
                &crate::ir::TestHookMember::TestbenchCycle {
                    service: service_index as u32,
                },
                &format!("testbench `{}` cycle service", testbench.name),
            )?;
            if !matches!(service.edge, CycleEdge::Level) {
                insert_unique(
                    &mut cells,
                    owner.clone(),
                    RuntimeCellKind::TestbenchEdgePrevious {
                        function: service.function,
                        phase: service.phase.into(),
                    },
                    &format!(
                        "testbench `{}` edge service fn{}",
                        testbench.name, service.function.0
                    ),
                )?;
            }
        }
    }

    for (index, _) in prog.scoreboards.iter().enumerate() {
        let scoreboard_id = ScoreboardId(index as u32);
        let owner = scoreboard_owner(prog, scoreboard_id)?;
        for heartbeat in [ComponentHeartbeat::Input, ComponentHeartbeat::Output] {
            insert(
                &mut cells,
                owner.clone(),
                RuntimeCellKind::ScoreboardHeartbeat(heartbeat),
            );
        }
    }

    for (index, transactor) in prog.transactors.iter().enumerate() {
        let transactor_id = TransactorId(index as u32);
        let owner = transactor_owner(prog, transactor_id)?;
        for heartbeat in [ComponentHeartbeat::Input, ComponentHeartbeat::Output] {
            insert(
                &mut cells,
                owner.clone(),
                RuntimeCellKind::TransactorHeartbeat(heartbeat),
            );
        }
        for method in &transactor.methods {
            if method.hookable {
                for side in [RuntimeHookSide::Pre, RuntimeHookSide::Post] {
                    insert(
                        &mut cells,
                        owner.clone(),
                        RuntimeCellKind::HookSubscribers {
                            hook: HookOwner::Transactor {
                                function: method.function,
                            },
                            side,
                        },
                    );
                }
            }
            if !method.cov_hook_subs.is_empty() {
                for side in [RuntimeHookSide::Pre, RuntimeHookSide::Post] {
                    insert(
                        &mut cells,
                        owner.clone(),
                        RuntimeCellKind::TransactorCoverageHookSubscribers {
                            function: method.function,
                            side,
                        },
                    );
                }
            }
        }
    }

    for (index, component) in prog.components.iter().enumerate() {
        let component_id = ComponentId(index as u32);
        let owner = component_owner(prog, component_id)?;
        for heartbeat in [ComponentHeartbeat::Input, ComponentHeartbeat::Output] {
            insert(
                &mut cells,
                owner.clone(),
                RuntimeCellKind::ComponentHeartbeat(heartbeat),
            );
        }
        for (field, schema) in component.fields.iter().enumerate() {
            if matches!(schema.kind, ComponentFieldKind::Event { .. }) {
                insert(
                    &mut cells,
                    owner.clone(),
                    RuntimeCellKind::ComponentEventSubscribers {
                        field: field as u32,
                    },
                );
            }
        }
        let method_count = component.methods.len();
        let on_count = component.on_handlers.len();
        let periodic_base = method_count + on_count;
        for (member, handler) in component.periodic_handlers.iter().enumerate() {
            let callable = ComponentCallableId((periodic_base + member) as u32);
            validate_component_callable(
                prog,
                component_id,
                callable,
                handler.function,
                &format!("component `{}` periodic handler {member}", component.name),
            )?;
            insert(
                &mut cells,
                owner.clone(),
                RuntimeCellKind::ComponentPeriodicLast { member: callable },
            );
        }
        let cycle_base = periodic_base + component.periodic_handlers.len();
        for (member, handler) in component.cycle_handlers.iter().enumerate() {
            let member = ComponentCallableId((cycle_base + member) as u32);
            validate_component_callable(
                prog,
                component_id,
                member,
                handler.function,
                &format!("component `{}` cycle handler {}", component.name, member.0),
            )?;
            let kind = if handler.monitor_channel.is_some() {
                Some(RuntimeCellKind::ComponentCooldown { member })
            } else if matches!(handler.edge, CycleEdge::Rising | CycleEdge::Falling) {
                Some(RuntimeCellKind::ComponentEdgePrevious { member })
            } else {
                None
            };
            if let Some(kind) = kind {
                insert(&mut cells, owner.clone(), kind);
            }
        }
        if let Some(watchdog) = &component.watchdog {
            let member = ComponentCallableId((cycle_base + component.cycle_handlers.len()) as u32);
            validate_component_callable(
                prog,
                component_id,
                member,
                watchdog.function,
                &format!("component `{}` watchdog", component.name),
            )?;
            insert(
                &mut cells,
                owner.clone(),
                RuntimeCellKind::ComponentWatchdogLast { member },
            );
        }
        for (member, method) in component.methods.iter().enumerate() {
            if method.hookable {
                for side in [RuntimeHookSide::Pre, RuntimeHookSide::Post] {
                    insert(
                        &mut cells,
                        owner.clone(),
                        RuntimeCellKind::HookSubscribers {
                            hook: HookOwner::Component {
                                component: component_id,
                                member: ComponentCallableId(member as u32),
                            },
                            side,
                        },
                    );
                }
            }
            if !method.cov_hook_subs.is_empty() {
                for side in [RuntimeHookSide::Pre, RuntimeHookSide::Post] {
                    insert(
                        &mut cells,
                        owner.clone(),
                        RuntimeCellKind::ComponentCoverageHookSubscribers {
                            component: component_id,
                            member: ComponentCallableId(member as u32),
                            side,
                        },
                    );
                }
            }
        }
    }

    for function in &prog.functions {
        if let FunctionKind::SamplerAuto { covgroup } = function.kind {
            let owner = match function.owner {
                Some(testbench) => testbench_owner(prog, testbench)?,
                None => RuntimeCellOwner::Callable {
                    function: function.id,
                    name: function.name.clone(),
                },
            };
            insert(
                &mut cells,
                owner,
                RuntimeCellKind::AutomaticCoverage {
                    function: function.id,
                    covgroup,
                },
            );
        }
    }

    let mut planned = Vec::with_capacity(cells.len());
    let mut stable_ids = BTreeSet::new();
    for (owner, kind) in cells {
        let site = runtime_cell_site(prog, &owner, &kind)?;
        let id = RuntimeCellId {
            owner: owner.clone(),
            site,
        };
        if !stable_ids.insert(id.clone()) {
            return Err(RuntimeCellError(format!(
                "runtime-cell plan found duplicate stable cell `{}` for owner {owner:?}",
                id.site().symbol()
            )));
        }
        let (storage, initializer, registration) = cell_contract(&kind);
        planned.push(RuntimeCell {
            id,
            owner,
            kind,
            storage,
            initializer,
            registration,
        });
    }
    planned.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(RuntimeCellPlan { cells: planned })
}
