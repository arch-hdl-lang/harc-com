use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{
    Activation, ComponentId, Expr, FunctionId, IrType, LaneIndex, LocalId, PortAccess,
    PortDirection, PortOrigin, PortRef, ProbeId, ProbeScalarType, Stmt, TbProgram, TestId,
    TestbenchId,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DutAccessIdentity {
    Port { path: Vec<String>, aggregate: bool },
    Probe(ProbeId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DutAccessOperation {
    Read,
    Write,
    ForceWrite,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DutLaneShape {
    None,
    Constant,
    Dynamic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DutAccessSite {
    Clock(TestId),
    Function(FunctionId),
    ComponentLifecycle {
        component: ComponentId,
        function: FunctionId,
        activation: Activation,
    },
    TestbenchService {
        testbench: TestbenchId,
        function: FunctionId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DutInterfacePort {
    name: String,
    direction: PortDirection,
    width: Option<u32>,
    value_type: IrType,
    packed_lane_width: Option<u32>,
    packed_lane_type: Option<IrType>,
    unpacked_elements: Option<u32>,
}

impl DutInterfacePort {
    pub fn new(
        name: impl Into<String>,
        direction: PortDirection,
        width: u32,
        packed_lane_width: Option<u32>,
        unpacked_elements: Option<u32>,
    ) -> Self {
        Self::new_typed(
            name,
            direction,
            width,
            IrType::UInt(Some(width)),
            packed_lane_width,
            unpacked_elements,
        )
    }

    pub fn new_typed(
        name: impl Into<String>,
        direction: PortDirection,
        width: u32,
        value_type: IrType,
        packed_lane_width: Option<u32>,
        unpacked_elements: Option<u32>,
    ) -> Self {
        let packed_lane_type = packed_lane_width.map(|lane_width| match &value_type {
            IrType::Bool if lane_width == 1 => IrType::Bool,
            IrType::SInt(_) => IrType::SInt(Some(lane_width)),
            _ => IrType::UInt(Some(lane_width)),
        });
        Self {
            name: name.into(),
            direction,
            width: Some(width),
            value_type,
            packed_lane_width,
            packed_lane_type,
            unpacked_elements,
        }
    }

    pub(crate) fn new_unresolved_with_shape(
        name: impl Into<String>,
        direction: PortDirection,
        value_type: IrType,
        packed_lane_width: Option<u32>,
        unpacked_elements: Option<u32>,
    ) -> Self {
        let packed_lane_type = packed_lane_width.map(|lane_width| match &value_type {
            IrType::SInt(_) => IrType::SInt(Some(lane_width)),
            _ => IrType::UInt(Some(lane_width)),
        });
        Self {
            name: name.into(),
            direction,
            width: None,
            value_type,
            packed_lane_width,
            packed_lane_type,
            unpacked_elements,
        }
    }

    pub(crate) fn with_packed_lane_type(mut self, value_type: IrType) -> Self {
        self.packed_lane_type = Some(value_type);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn direction(&self) -> PortDirection {
        self.direction
    }

    pub fn resolved_width(&self) -> Option<u32> {
        self.width
    }

    pub fn value_type(&self) -> &IrType {
        &self.value_type
    }

    pub fn packed_lane_width(&self) -> Option<u32> {
        self.packed_lane_width
    }

    pub fn packed_lane_type(&self) -> Option<&IrType> {
        self.packed_lane_type.as_ref()
    }

    pub fn unpacked_elements(&self) -> Option<u32> {
        self.unpacked_elements
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DutInterfaceCatalog {
    dut_type: String,
    ports: Vec<DutInterfacePort>,
}

impl DutInterfaceCatalog {
    pub fn new(
        dut_type: impl Into<String>,
        mut ports: Vec<DutInterfacePort>,
    ) -> Result<Self, DutAccessPlanError> {
        let dut_type = dut_type.into();
        if dut_type.is_empty() {
            return Err(DutAccessPlanError(
                "DUT interface catalog has an empty DUT type".to_string(),
            ));
        }
        ports.sort_by(|lhs, rhs| lhs.name.cmp(&rhs.name));
        let mut physical_names = BTreeMap::<String, String>::new();
        for port in &ports {
            for physical in physical_port_names(&port.name) {
                if let Some(previous) = physical_names.insert(physical.clone(), port.name.clone()) {
                    if previous != port.name {
                        return Err(DutAccessPlanError(format!(
                            "DUT interface ports `{previous}` and `{}` alias the same physical binding `{physical}`",
                            port.name
                        )));
                    }
                }
            }
        }
        for pair in ports.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(DutAccessPlanError(format!(
                    "DUT interface catalog repeats port `{}`",
                    pair[0].name
                )));
            }
        }
        for port in &ports {
            if port.name.is_empty() || port.width == Some(0) {
                return Err(DutAccessPlanError(format!(
                    "DUT interface port `{}` has invalid width {:?}",
                    port.name, port.width
                )));
            }
            let type_width = match &port.value_type {
                IrType::UInt(Some(width)) | IrType::SInt(Some(width)) => Some(*width),
                IrType::Bool => Some(1),
                _ => None,
            };
            match port.width {
                Some(width) if type_width != Some(width) => {
                    return Err(DutAccessPlanError(format!(
                        "DUT interface port `{}` type {:?} does not match width {width}",
                        port.name, port.value_type
                    )));
                }
                None if !matches!(port.value_type, IrType::UInt(None) | IrType::SInt(None)) => {
                    return Err(DutAccessPlanError(format!(
                        "DUT interface port `{}` has unresolved width but non-widthless type {:?}",
                        port.name, port.value_type
                    )));
                }
                _ => {}
            }
            if port.packed_lane_width == Some(0) || port.unpacked_elements == Some(0) {
                return Err(DutAccessPlanError(format!(
                    "DUT interface port `{}` has an invalid zero-sized lane shape",
                    port.name
                )));
            }
            if let Some(lane_type) = &port.packed_lane_type {
                let lane_width = match lane_type {
                    IrType::Bool => Some(1),
                    IrType::UInt(width) | IrType::SInt(width) => *width,
                    _ => None,
                };
                if port.packed_lane_width.is_none() || lane_width != port.packed_lane_width {
                    return Err(DutAccessPlanError(format!(
                        "DUT interface port `{}` has inconsistent packed-lane type {:?} and width {:?}",
                        port.name, lane_type, port.packed_lane_width
                    )));
                }
            }
            if port.packed_lane_width.is_some() && port.unpacked_elements.is_some() {
                return Err(DutAccessPlanError(format!(
                    "DUT interface port `{}` cannot be both packed-lane and unpacked-array shaped",
                    port.name
                )));
            }
            if let Some(lane_width) = port.packed_lane_width {
                let Some(width) = port.width else {
                    return Err(DutAccessPlanError(format!(
                        "DUT interface port `{}` has unresolved width with a packed-lane shape",
                        port.name
                    )));
                };
                if lane_width > width || width % lane_width != 0 {
                    return Err(DutAccessPlanError(format!(
                        "DUT interface port `{}` width {} is not an integral number of {}-bit packed lanes",
                        port.name, width, lane_width
                    )));
                }
            }
        }
        Ok(Self { dut_type, ports })
    }

    pub fn dut_type(&self) -> &str {
        &self.dut_type
    }

    pub fn ports(&self) -> &[DutInterfacePort] {
        &self.ports
    }

    pub fn port(&self, name: &str) -> Option<&DutInterfacePort> {
        self.ports
            .binary_search_by(|port| port.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.ports[index])
    }

    pub fn port_by_physical_name(&self, name: &str) -> Option<&DutInterfacePort> {
        self.port(name).or_else(|| {
            self.ports.iter().find(|port| {
                physical_port_names(port.name())
                    .iter()
                    .any(|alias| alias == name)
            })
        })
    }
}

fn physical_port_names(name: &str) -> Vec<String> {
    let flattened = name.replace('.', "_");
    let mut verilator = String::with_capacity(flattened.len());
    let mut previous_underscore = false;
    for character in flattened.chars() {
        if character == '_' && previous_underscore {
            verilator.push_str("__05F");
        } else {
            verilator.push(character);
        }
        previous_underscore = character == '_';
    }
    if verilator == flattened {
        vec![flattened]
    } else {
        vec![flattened, verilator]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DutAccessUse {
    site: DutAccessSite,
    operation: DutAccessOperation,
    lane_shape: DutLaneShape,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DutAccessEntry {
    identity: DutAccessIdentity,
    source_path: Vec<String>,
    source_aggregate: bool,
    path: Vec<String>,
    aggregate: bool,
    direction: Option<PortDirection>,
    width: u32,
    value_type: Option<IrType>,
    packed_lane_width: Option<u32>,
    packed_lane_type: Option<IrType>,
    access: PortAccess,
    lane_shapes: BTreeSet<DutLaneShape>,
    operations: BTreeSet<DutAccessOperation>,
    uses: Vec<DutAccessUse>,
}

impl DutAccessEntry {
    pub fn identity(&self) -> &DutAccessIdentity {
        &self.identity
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub fn aggregate(&self) -> bool {
        self.aggregate
    }

    pub fn direction(&self) -> Option<PortDirection> {
        self.direction
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn value_type(&self) -> Option<&IrType> {
        self.value_type.as_ref()
    }

    pub fn packed_lane_width(&self) -> Option<u32> {
        self.packed_lane_width
    }

    pub fn value_type_for(&self, port: &PortRef) -> Option<IrType> {
        if port.lane.is_some() {
            return self.lane_value_type();
        }
        let width = self.width_for(port);
        self.value_type.as_ref().map(|value_type| match value_type {
            IrType::Bool if width == 1 => IrType::Bool,
            IrType::SInt(_) => IrType::SInt(Some(width)),
            _ => IrType::UInt(Some(width)),
        })
    }

    pub fn lane_value_type(&self) -> Option<IrType> {
        let width = self.packed_lane_width.unwrap_or(self.width);
        self.packed_lane_type.clone().or_else(|| {
            self.value_type.as_ref().map(|value_type| match value_type {
                IrType::Bool if width == 1 => IrType::Bool,
                IrType::SInt(_) => IrType::SInt(Some(width)),
                _ => IrType::UInt(Some(width)),
            })
        })
    }

    pub fn width_for(&self, port: &PortRef) -> u32 {
        if port.lane.is_some() {
            self.packed_lane_width.unwrap_or(self.width)
        } else {
            self.width
        }
    }

    pub fn access(&self) -> PortAccess {
        self.access
    }

    pub fn lane_shapes(&self) -> &BTreeSet<DutLaneShape> {
        &self.lane_shapes
    }

    pub fn operations(&self) -> &BTreeSet<DutAccessOperation> {
        &self.operations
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DutProbePlan {
    id: ProbeId,
    name: String,
    sv_path: String,
    ty: ProbeScalarType,
    force: bool,
    shared: bool,
    tests: Vec<TestId>,
    test_names: Vec<String>,
}

impl DutProbePlan {
    pub fn id(&self) -> ProbeId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn sv_path(&self) -> &str {
        &self.sv_path
    }

    pub fn ty(&self) -> ProbeScalarType {
        self.ty
    }

    pub fn force_capable(&self) -> bool {
        self.force
    }

    pub fn shared(&self) -> bool {
        self.shared
    }

    pub fn tests(&self) -> &[TestId] {
        &self.tests
    }

    pub fn test_names(&self) -> &[String] {
        &self.test_names
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DutAccessPlan {
    dut_type: String,
    interface: DutInterfaceCatalog,
    probes: Vec<DutProbePlan>,
    accesses: Vec<DutAccessEntry>,
    inferred_local_types: BTreeMap<(FunctionId, LocalId), IrType>,
}

impl DutAccessPlan {
    pub fn dut_type(&self) -> &str {
        &self.dut_type
    }

    pub fn probes(&self) -> &[DutProbePlan] {
        &self.probes
    }

    pub fn interface(&self) -> &DutInterfaceCatalog {
        &self.interface
    }

    pub fn accesses(&self) -> &[DutAccessEntry] {
        &self.accesses
    }

    pub fn inferred_local_type(&self, function: FunctionId, local: LocalId) -> Option<&IrType> {
        self.inferred_local_types.get(&(function, local))
    }

    pub fn probe(&self, id: ProbeId) -> Option<&DutProbePlan> {
        self.probes.iter().find(|probe| probe.id == id)
    }

    pub fn resolve<'a>(&'a self, port: &PortRef) -> Result<&'a DutAccessEntry, DutAccessPlanError> {
        let identity = resolved_access_identity(&self.interface, port)?;
        let entry = self
            .accesses
            .iter()
            .find(|entry| entry.identity == identity)
            .ok_or_else(|| {
                DutAccessPlanError(format!(
                    "DUT access `{}` is absent from the immutable access plan",
                    display_port(port)
                ))
            })?;
        let projected_type = entry.value_type_for(port);
        if (port.direction.is_some() && entry.direction != port.direction)
            || port
                .width
                .is_some_and(|width| width != entry.width_for(port))
            || port
                .value_type
                .as_ref()
                .is_some_and(|ty| Some(ty) != projected_type.as_ref())
            || entry.access != port.access
        {
            return Err(DutAccessPlanError(format!(
                "DUT access `{}` does not match its immutable access-plan entry",
                display_port(port)
            )));
        }
        Ok(entry)
    }

    pub fn probe_for_port(&self, port: &PortRef) -> Result<&DutProbePlan, DutAccessPlanError> {
        self.resolve(port)?;
        let probe = port.probe.ok_or_else(|| {
            DutAccessPlanError(format!(
                "DUT access `{}` is not a probe",
                display_port(port)
            ))
        })?;
        self.probe(probe).ok_or_else(|| {
            DutAccessPlanError(format!(
                "DUT access `{}` references missing probe p{}",
                display_port(port),
                probe.0
            ))
        })
    }

    pub fn abi_lines(&self) -> Vec<String> {
        let mut lines = vec![format!("dut={}", self.dut_type)];
        for port in self.interface.ports() {
            lines.push(format!(
                "port:name={}:direction={:?}:width={:?}:type={:?}:packed_lane={:?}:packed_lane_type={:?}:unpacked_elements={:?}",
                port.name(),
                port.direction(),
                port.resolved_width(),
                port.value_type(),
                port.packed_lane_width(),
                port.packed_lane_type(),
                port.unpacked_elements(),
            ));
        }
        for probe in &self.probes {
            let cohort = if probe.shared {
                "all".to_string()
            } else {
                probe.test_names.join(",")
            };
            lines.push(format!(
                "probe:name={}:path={}:type={:?}:force={}:tests={}",
                probe.name, probe.sv_path, probe.ty, probe.force, cohort
            ));
        }
        lines
    }

    pub fn access_lines_for_sites(&self, sites: &BTreeSet<DutAccessSite>) -> Vec<String> {
        let mut lines = self
            .accesses
            .iter()
            .filter_map(|access| {
                let operations = access
                    .uses
                    .iter()
                    .filter(|access_use| sites.contains(&access_use.site))
                    .map(|access_use| access_use.operation)
                    .collect::<BTreeSet<_>>();
                let lane_shapes = access
                    .uses
                    .iter()
                    .filter(|access_use| sites.contains(&access_use.site))
                    .map(|access_use| access_use.lane_shape)
                    .collect::<BTreeSet<_>>();
                (!operations.is_empty()).then(|| {
                    let identity = match &access.identity {
                        DutAccessIdentity::Port { .. } => {
                            format!("port:{}", access.path.join("."))
                        }
                        DutAccessIdentity::Probe(probe) => self
                            .probe(*probe)
                            .map(|probe| {
                                format!("probe:{}@{}", probe.name, probe.sv_path)
                            })
                            .unwrap_or_else(|| format!("missing-probe:p{}", probe.0)),
                    };
                    format!(
                        "access:{identity}:path={}:aggregate={}:direction={:?}:width={}:type={:?}:packed_lane={:?}:class={:?}:lanes={lane_shapes:?}:ops={operations:?}",
                        access.path.join("."),
                        access.aggregate,
                        access.direction,
                        access.width,
                        access.value_type,
                        access.packed_lane_width,
                        access.access,
                    )
                })
            })
            .collect::<Vec<_>>();
        lines.sort();
        lines
    }

    pub fn sites_use_probe(&self, sites: &BTreeSet<DutAccessSite>) -> bool {
        self.accesses.iter().any(|access| {
            matches!(access.identity, DutAccessIdentity::Probe(_))
                && access
                    .uses
                    .iter()
                    .any(|access_use| sites.contains(&access_use.site))
        })
    }

    pub fn uses_probe(&self) -> bool {
        self.accesses.iter().any(|access| {
            matches!(access.identity, DutAccessIdentity::Probe(_)) && !access.uses.is_empty()
        })
    }

    pub fn clock_access_sites(&self) -> BTreeSet<DutAccessSite> {
        self.accesses
            .iter()
            .flat_map(|access| &access.uses)
            .filter_map(|access_use| match access_use.site {
                DutAccessSite::Clock(_) => Some(access_use.site),
                _ => None,
            })
            .collect()
    }

    pub fn validate_clock(
        &self,
        test: TestId,
        name: &str,
    ) -> Result<&DutAccessEntry, DutAccessPlanError> {
        self.accesses
            .iter()
            .find(|access| {
                access.path == [name]
                    && access.uses.iter().any(|access_use| {
                        access_use.site == DutAccessSite::Clock(test)
                            && access_use.operation == DutAccessOperation::Write
                    })
            })
            .ok_or_else(|| {
                DutAccessPlanError(format!(
                    "test t{} clock `{name}` is absent from the immutable DUT access plan",
                    test.0
                ))
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DutAccessPlanError(pub String);

impl std::fmt::Display for DutAccessPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DutAccessPlanError {}

pub fn analyze(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
) -> Result<DutAccessPlan, DutAccessPlanError> {
    let first_test = prog.tests.first().ok_or_else(|| {
        DutAccessPlanError("DUT access planning requires at least one test".to_string())
    })?;
    let first_tb = prog
        .testbenches
        .get(first_test.testbench.index())
        .ok_or_else(|| {
            DutAccessPlanError(format!(
                "test `{}` references missing testbench tb{}",
                first_test.name, first_test.testbench.0
            ))
        })?;
    let dut_type = first_tb.dut_type.clone();
    if interface.dut_type != dut_type {
        return Err(DutAccessPlanError(format!(
            "DUT interface catalog targets `{}` but the verified suite uses `{dut_type}`",
            interface.dut_type
        )));
    }

    for (test_index, test) in prog.tests.iter().enumerate() {
        if test.id != TestId(test_index as u32) {
            return Err(DutAccessPlanError(format!(
                "test table slot {test_index} carries mismatched id t{}",
                test.id.0
            )));
        }
        let tb = prog
            .testbenches
            .get(test.testbench.index())
            .ok_or_else(|| {
                DutAccessPlanError(format!(
                    "test `{}` references missing testbench tb{}",
                    test.name, test.testbench.0
                ))
            })?;
        if tb.dut_type != dut_type {
            return Err(DutAccessPlanError(format!(
                "test `{}` uses DUT `{}` but the suite cohort uses `{dut_type}`",
                test.name, tb.dut_type
            )));
        }
    }

    for component in &prog.components {
        for field in &component.fields {
            if let crate::ir::ComponentFieldKind::Dut {
                dut_type: field_type,
            } = &field.kind
            {
                if field_type != &dut_type {
                    return Err(DutAccessPlanError(format!(
                        "component `{}` DUT field `{}` has module type `{field_type}`, but this suite owns `{dut_type}`",
                        component.name, field.name
                    )));
                }
            }
        }
    }
    for schema in &prog.testbench_types {
        for method in &schema.methods {
            for (parameter, module) in method.module_param_types.iter().enumerate() {
                if let Some(module) = module {
                    if module != &dut_type {
                        return Err(DutAccessPlanError(format!(
                            "testbench method `{}.{}` parameter {} has module type `{module}`, but this suite owns `{dut_type}`",
                            schema.name,
                            method.name,
                            parameter + 1
                        )));
                    }
                }
            }
        }
    }

    for (testbench_index, testbench) in prog.testbenches.iter().enumerate() {
        let mut seen = BTreeSet::new();
        for probe in &testbench.probes {
            if !seen.insert(*probe) {
                return Err(DutAccessPlanError(format!(
                    "testbench tb{testbench_index} `{}` repeats probe capability p{}",
                    testbench.name, probe.0
                )));
            }
            let schema = prog
                .probes
                .get(probe.index())
                .filter(|schema| schema.id == *probe)
                .ok_or_else(|| {
                    DutAccessPlanError(format!(
                        "testbench tb{testbench_index} `{}` references missing probe p{}",
                        testbench.name, probe.0
                    ))
                })?;
            if schema.dut_type != testbench.dut_type {
                return Err(DutAccessPlanError(format!(
                    "testbench tb{testbench_index} `{}` uses DUT `{}` but probe p{} `{}` targets `{}`",
                    testbench.name,
                    testbench.dut_type,
                    probe.0,
                    schema.name,
                    schema.dut_type
                )));
            }
        }
    }

    let mut probes = Vec::with_capacity(prog.probes.len());
    let mut probe_names = BTreeMap::<String, ProbeId>::new();
    let mut generated_probe_symbols = BTreeMap::<String, ProbeId>::new();
    let mut force_probe_paths = Vec::<(String, ProbeId)>::new();
    let mut entries = BTreeMap::<DutAccessIdentity, DutAccessEntry>::new();
    for (index, probe) in prog.probes.iter().enumerate() {
        if probe.id != ProbeId(index as u32) {
            return Err(DutAccessPlanError(format!(
                "probe table slot {index} carries mismatched id p{}",
                probe.id.0
            )));
        }
        if probe.dut_type != dut_type {
            return Err(DutAccessPlanError(format!(
                "probe p{} `{}` targets DUT `{}` but the suite cohort uses `{dut_type}`",
                probe.id.0, probe.name, probe.dut_type
            )));
        }
        if probe.name.is_empty() || probe.sv_path.trim().is_empty() || probe.ty.width() == 0 {
            return Err(DutAccessPlanError(format!(
                "probe p{} `{}` has invalid path/type metadata ({:?} at `{}`)",
                probe.id.0, probe.name, probe.ty, probe.sv_path
            )));
        }
        if let Some(previous) = probe_names.insert(probe.name.clone(), probe.id) {
            return Err(DutAccessPlanError(format!(
                "probe p{} `{}` conflicts with probe p{} in the shared DUT accessor namespace",
                probe.id.0, probe.name, previous.0
            )));
        }
        let mut symbols = vec![probe.name.clone()];
        if probe.force {
            symbols.push(format!("{}_drv", probe.name));
            symbols.push(format!("{}_en", probe.name));
        }
        for symbol in symbols {
            if let Some(previous) = generated_probe_symbols.insert(symbol.clone(), probe.id) {
                return Err(DutAccessPlanError(format!(
                    "probe p{} `{}` collides with generated signal `{symbol}` owned by probe p{}",
                    probe.id.0, probe.name, previous.0
                )));
            }
        }
        if probe.force {
            for (path, previous) in &force_probe_paths {
                if probe_paths_overlap(path, &probe.sv_path) {
                    return Err(DutAccessPlanError(format!(
                        "force probe p{} `{}` path `{}` overlaps force probe p{} path `{path}`",
                        probe.id.0, probe.name, probe.sv_path, previous.0
                    )));
                }
            }
            force_probe_paths.push((probe.sv_path.clone(), probe.id));
        }
        let mut cohort = Vec::new();
        for test in &prog.tests {
            let tb = &prog.testbenches[test.testbench.index()];
            if tb.probes.contains(&probe.id) {
                cohort.push((test.name.clone(), test.id));
            }
        }
        cohort.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));
        let test_names = cohort
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let tests = cohort.iter().map(|(_, id)| *id).collect::<Vec<_>>();
        if probe.shared != (cohort.len() == prog.tests.len()) {
            return Err(DutAccessPlanError(format!(
                "probe p{} `{}` shared={} but its exact test cohort is [{}]",
                probe.id.0,
                probe.name,
                probe.shared,
                test_names.join(", ")
            )));
        }
        probes.push(DutProbePlan {
            id: probe.id,
            name: probe.name.clone(),
            sv_path: probe.sv_path.clone(),
            ty: probe.ty,
            force: probe.force,
            shared: probe.shared,
            tests,
            test_names,
        });
        entries.insert(
            DutAccessIdentity::Probe(probe.id),
            DutAccessEntry {
                identity: DutAccessIdentity::Probe(probe.id),
                source_path: vec![probe.name.clone()],
                source_aggregate: true,
                path: vec![probe.name.clone()],
                aggregate: true,
                direction: None,
                width: probe.ty.width(),
                value_type: Some(probe.ty.ir_type()),
                packed_lane_width: None,
                packed_lane_type: None,
                access: if probe.force {
                    PortAccess::Force
                } else {
                    PortAccess::Probe
                },
                lane_shapes: BTreeSet::from([DutLaneShape::None]),
                operations: BTreeSet::new(),
                uses: Vec::new(),
            },
        );
    }

    for test in &prog.tests {
        let testbench = prog.testbench(test.testbench);
        let run = checked_function(prog, test.run, "test run")?;
        for clock in &test.clocks {
            let port = PortRef {
                testbench_field: testbench.dut_field.clone(),
                origin: PortOrigin::Dut,
                port_path: vec![clock.name.clone()],
                aggregate_path: true,
                direction: Some(PortDirection::In),
                width: Some(1),
                value_type: None,
                access: PortAccess::Port,
                probe: None,
                lane: None,
            };
            record_use_at(
                prog,
                interface,
                &mut entries,
                DutAccessSite::Clock(test.id),
                run.id,
                &run.name,
                0,
                &port,
                DutAccessOperation::Write,
                None,
            )?;
        }
    }

    for function in &prog.functions {
        let crate::ir::FunctionKind::SamplerAuto { covgroup } = function.kind else {
            continue;
        };
        let schema = prog.covgroups.get(covgroup.index()).ok_or_else(|| {
            DutAccessPlanError(format!(
                "sampler fn{} `{}` references missing covergroup cg{}",
                function.id.0, function.name, covgroup.0
            ))
        })?;
        for point in &schema.points {
            record_expr_reads_at(
                prog,
                interface,
                &mut entries,
                DutAccessSite::Function(function.id),
                function.id,
                &function.name,
                0,
                &point.target,
            )?;
            for value in &point.bins {
                for value in &value.values {
                    match value {
                        crate::ir::CovBinValue::Eq(crate::ir::CovBinBound::Runtime(expr)) => {
                            record_expr_reads_at(
                                prog,
                                interface,
                                &mut entries,
                                DutAccessSite::Function(function.id),
                                function.id,
                                &function.name,
                                0,
                                expr,
                            )?;
                        }
                        crate::ir::CovBinValue::Range { lo, hi } => {
                            for bound in [lo, hi].into_iter().flatten() {
                                if let crate::ir::CovBinBound::Runtime(expr) = bound {
                                    record_expr_reads_at(
                                        prog,
                                        interface,
                                        &mut entries,
                                        DutAccessSite::Function(function.id),
                                        function.id,
                                        &function.name,
                                        0,
                                        expr,
                                    )?;
                                }
                            }
                        }
                        crate::ir::CovBinValue::Eq(crate::ir::CovBinBound::Const(_)) => {}
                    }
                }
            }
        }
    }

    let mut inferred_local_types = BTreeMap::<(FunctionId, LocalId), IrType>::new();
    for function in &prog.functions {
        for (block_index, block) in function.blocks.iter().enumerate() {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::DutWrite(port, value) => record_use(
                        prog,
                        interface,
                        &mut entries,
                        function.id,
                        &function.name,
                        block_index,
                        port,
                        if port.access == PortAccess::Force {
                            DutAccessOperation::ForceWrite
                        } else {
                            DutAccessOperation::Write
                        },
                        super::super::verify::assignment_expr_type(prog, function, value),
                    )?,
                    Stmt::DutRead(destination, port) => {
                        let local_type = function
                            .locals
                            .get(destination.index())
                            .map(|local| local.ty.clone());
                        record_use(
                            prog,
                            interface,
                            &mut entries,
                            function.id,
                            &function.name,
                            block_index,
                            port,
                            DutAccessOperation::Read,
                            local_type.clone(),
                        )?;
                        if matches!(local_type, Some(IrType::Unknown)) {
                            let resolved = if port.origin == PortOrigin::Dut {
                                let identity = resolved_access_identity(interface, port)?;
                                entries
                                    .get(&identity)
                                    .and_then(|entry| entry.value_type_for(port))
                            } else {
                                port.value_type.clone().filter(|ty| *ty != IrType::Unknown)
                            }
                            .ok_or_else(|| {
                                use_error(
                                    function.id,
                                    &function.name,
                                    block_index,
                                    port,
                                    "has no resolved scalar type for its inferred destination",
                                )
                            })?;
                            let key = (function.id, *destination);
                            if let Some(previous) = inferred_local_types.get(&key) {
                                let Some(joined) = super::super::verify::common_scalar_expr_type(
                                    Some(previous.clone()),
                                    Some(resolved.clone()),
                                ) else {
                                    return Err(use_error(
                                        function.id,
                                        &function.name,
                                        block_index,
                                        port,
                                        &format!(
                                            "cannot join inferred destination %{} types {previous:?} and {resolved:?}",
                                            destination.0
                                        ),
                                    ));
                                };
                                inferred_local_types.insert(key, joined);
                            } else {
                                inferred_local_types.insert(key, resolved);
                            }
                        }
                    }
                    Stmt::ProbeRelease(port) => record_use(
                        prog,
                        interface,
                        &mut entries,
                        function.id,
                        &function.name,
                        block_index,
                        port,
                        DutAccessOperation::Release,
                        None,
                    )?,
                    Stmt::PropertyCheck(id) => {
                        let property = prog.property_checks.get(id.index()).ok_or_else(|| {
                            DutAccessPlanError(format!(
                                "function fn{} `{}` block b{block_index} references missing property check pc{}",
                                function.id.0, function.name, id.0
                            ))
                        })?;
                        record_property_reads(
                            prog,
                            interface,
                            &mut entries,
                            function.id,
                            &function.name,
                            block_index,
                            property,
                        )?;
                    }
                    Stmt::CoverCheck(id) => {
                        let cover = prog.cover_checks.get(id.index()).ok_or_else(|| {
                            DutAccessPlanError(format!(
                                "function fn{} `{}` block b{block_index} references missing cover check cc{}",
                                function.id.0, function.name, id.0
                            ))
                        })?;
                        record_expr_reads(
                            prog,
                            interface,
                            &mut entries,
                            function.id,
                            &function.name,
                            block_index,
                            &cover.cond,
                        )?;
                        for temporal in &cover.temporals {
                            record_expr_reads(
                                prog,
                                interface,
                                &mut entries,
                                function.id,
                                &function.name,
                                block_index,
                                &temporal.inner,
                            )?;
                        }
                    }
                    Stmt::CycleHandler(id) => {
                        let handler = prog.cycle_handlers.get(id.index()).ok_or_else(|| {
                            DutAccessPlanError(format!(
                                "function fn{} `{}` block b{block_index} references missing cycle handler ch{}",
                                function.id.0, function.name, id.0
                            ))
                        })?;
                        if let crate::ir::CycleHandlerKind::Trigger { trigger, .. } = &handler.kind
                        {
                            record_expr_reads(
                                prog,
                                interface,
                                &mut entries,
                                function.id,
                                &function.name,
                                block_index,
                                trigger,
                            )?;
                        }
                    }
                    _ => {}
                }
                super::callable_placement::visit_stmt_exprs(stmt, &mut |expr| {
                    record_expr_reads(
                        prog,
                        interface,
                        &mut entries,
                        function.id,
                        &function.name,
                        block_index,
                        expr,
                    )
                    .map_err(|error| super::callable_placement::PlacementError(error.0))
                })
                .map_err(|error| DutAccessPlanError(error.0))?;
            }
            super::callable_placement::visit_terminator_exprs(&block.terminator, &mut |expr| {
                record_expr_reads(
                    prog,
                    interface,
                    &mut entries,
                    function.id,
                    &function.name,
                    block_index,
                    expr,
                )
                .map_err(|error| super::callable_placement::PlacementError(error.0))
            })
            .map_err(|error| DutAccessPlanError(error.0))?;
        }
    }

    for (component_index, component) in prog.components.iter().enumerate() {
        let component_id = ComponentId(component_index as u32);
        for periodic in &component.periodic_handlers {
            let function = checked_function(prog, periodic.function, "component periodic")?;
            record_expr_reads_at(
                prog,
                interface,
                &mut entries,
                DutAccessSite::ComponentLifecycle {
                    component: component_id,
                    function: function.id,
                    activation: periodic.activation,
                },
                function.id,
                &function.name,
                0,
                &periodic.period,
            )?;
        }
        for cycle in &component.cycle_handlers {
            let function = checked_function(prog, cycle.function, "component cycle handler")?;
            record_expr_reads_at(
                prog,
                interface,
                &mut entries,
                DutAccessSite::ComponentLifecycle {
                    component: component_id,
                    function: function.id,
                    activation: cycle.activation,
                },
                function.id,
                &function.name,
                0,
                &cycle.trigger,
            )?;
        }
        if let Some(watchdog) = &component.watchdog {
            let function = checked_function(prog, watchdog.function, "component watchdog")?;
            for expr in [watchdog.period.as_ref(), watchdog.max_idle.as_ref()]
                .into_iter()
                .flatten()
            {
                record_expr_reads_at(
                    prog,
                    interface,
                    &mut entries,
                    DutAccessSite::ComponentLifecycle {
                        component: component_id,
                        function: function.id,
                        activation: watchdog.activation,
                    },
                    function.id,
                    &function.name,
                    0,
                    expr,
                )?;
            }
        }
    }
    for (testbench_index, testbench) in prog.testbenches.iter().enumerate() {
        let testbench_id = TestbenchId(testbench_index as u32);
        for service in &testbench.cycle_services {
            let function = checked_function(prog, service.function, "testbench cycle service")?;
            record_expr_reads_at(
                prog,
                interface,
                &mut entries,
                DutAccessSite::TestbenchService {
                    testbench: testbench_id,
                    function: function.id,
                },
                function.id,
                &function.name,
                0,
                &service.trigger,
            )?;
        }
    }

    propagate_inferred_local_types(prog, interface, &entries, &mut inferred_local_types)?;
    validate_resolved_scalar_uses(prog, interface, &entries, &inferred_local_types)?;

    probes.sort_by(|lhs, rhs| lhs.name.cmp(&rhs.name));
    let mut accesses = entries.into_values().collect::<Vec<_>>();
    accesses.sort_by(|lhs, rhs| {
        access_semantic_key(lhs, &probes).cmp(&access_semantic_key(rhs, &probes))
    });
    for access in &mut accesses {
        access.uses.sort_by(|lhs, rhs| {
            lhs.site
                .cmp(&rhs.site)
                .then(lhs.operation.cmp(&rhs.operation))
                .then(lhs.lane_shape.cmp(&rhs.lane_shape))
        });
    }
    Ok(DutAccessPlan {
        dut_type,
        interface: interface.clone(),
        probes,
        accesses,
        inferred_local_types,
    })
}

fn propagate_inferred_local_types(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &mut BTreeMap<(FunctionId, LocalId), IrType>,
) -> Result<(), DutAccessPlanError> {
    loop {
        let mut changed = false;
        for function in &prog.functions {
            for (block, stmt) in function
                .blocks
                .iter()
                .enumerate()
                .flat_map(|(block, basic)| basic.stmts.iter().map(move |stmt| (block, stmt)))
            {
                let Stmt::Assign(destination, value) = stmt else {
                    continue;
                };
                if !matches!(
                    function
                        .locals
                        .get(destination.index())
                        .map(|local| &local.ty),
                    Some(IrType::Unknown)
                ) || !expr_uses_resolved_dut_type(function.id, value, inferred)
                {
                    continue;
                }
                let Some(resolved) = resolved_contextual_expr_type(
                    prog, function, value, interface, entries, inferred,
                ) else {
                    continue;
                };
                if matches!(resolved, IrType::Unknown | IrType::PortSnapshot) {
                    continue;
                }
                match inferred.get(&(function.id, *destination)) {
                    Some(previous) if previous != &resolved => {
                        let Some(joined) = super::super::verify::common_scalar_expr_type(
                            Some(previous.clone()),
                            Some(resolved.clone()),
                        ) else {
                            return Err(DutAccessPlanError(format!(
                                "function fn{} `{}` block b{block} cannot join inferred destination %{} types {previous:?} and {resolved:?}",
                                function.id.0, function.name, destination.0
                            )));
                        };
                        if &joined != previous {
                            inferred.insert((function.id, *destination), joined);
                            changed = true;
                        }
                    }
                    Some(_) => {}
                    None => {
                        inferred.insert((function.id, *destination), resolved);
                        changed = true;
                    }
                }
            }
            for stmt in function.blocks.iter().flat_map(|block| &block.stmts) {
                let Stmt::MethodHookSubscribe {
                    handler, captures, ..
                } = stmt
                else {
                    continue;
                };
                let handler = checked_function(prog, *handler, "method-hook capture")?;
                let method_count = handler
                    .params
                    .len()
                    .checked_sub(captures.len())
                    .ok_or_else(|| {
                        DutAccessPlanError(format!(
                            "method-hook handler fn{} `{}` has fewer parameters than captures",
                            handler.id.0, handler.name
                        ))
                    })?;
                for (capture_index, capture) in captures.iter().enumerate() {
                    let Some(resolved) = resolved_local_type(function, *capture, inferred) else {
                        continue;
                    };
                    if matches!(resolved, IrType::Unknown | IrType::PortSnapshot) {
                        continue;
                    }
                    let handler_local = LocalId((method_count + capture_index) as u32);
                    if !matches!(
                        handler
                            .locals
                            .get(handler_local.index())
                            .map(|local| &local.ty),
                        Some(IrType::Unknown)
                    ) {
                        continue;
                    }
                    match inferred.get(&(handler.id, handler_local)) {
                        Some(previous) if previous != &resolved => {
                            let Some(joined) = super::super::verify::common_scalar_expr_type(
                                Some(previous.clone()),
                                Some(resolved.clone()),
                            ) else {
                                return Err(DutAccessPlanError(format!(
                                    "method-hook handler fn{} `{}` cannot join capture parameter %{} types {previous:?} and {resolved:?}",
                                    handler.id.0, handler.name, handler_local.0
                                )));
                            };
                            if &joined != previous {
                                inferred.insert((handler.id, handler_local), joined);
                                changed = true;
                            }
                        }
                        Some(_) => {}
                        None => {
                            inferred.insert((handler.id, handler_local), resolved);
                            changed = true;
                        }
                    }
                }
            }
        }
        if !changed {
            return Ok(());
        }
    }
}

fn validate_resolved_scalar_uses(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Result<(), DutAccessPlanError> {
    let mut resolved_program = prog.clone();
    for ((function, local), ty) in inferred {
        if let Some(function) = resolved_program.functions.get_mut(function.index()) {
            if let Some(local_schema) = function.locals.get_mut(local.index()) {
                local_schema.ty = ty.clone();
            }
            if let Some(param) = function.params.get_mut(local.index()) {
                param.ty = ty.clone();
            }
        }
    }
    let baseline_errors = super::super::verify::verify_program(prog)
        .err()
        .unwrap_or_default()
        .into_iter()
        .map(|error| error.to_string())
        .collect::<BTreeSet<_>>();
    if let Err(errors) = super::super::verify::verify_program(&resolved_program) {
        let new_errors = errors
            .iter()
            .map(|error| error.to_string())
            .filter(|error| !baseline_errors.contains(error))
            .collect::<Vec<_>>();
        if new_errors.is_empty() {
            // Preserve the planner's established deterministic diagnostics
            // for malformed IR that was already invalid before DUT typing.
        } else {
            return Err(DutAccessPlanError(format!(
                "resolved DUT read types invalidate downstream IR transfers:\n{}",
                new_errors
                    .iter()
                    .map(|error| format!("  - {error}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )));
        }
    }
    for function in &prog.functions {
        for (block_index, block) in function.blocks.iter().enumerate() {
            for stmt in &block.stmts {
                validate_resolved_statement_sinks(
                    prog,
                    function,
                    block_index,
                    stmt,
                    interface,
                    entries,
                    inferred,
                )?;
                match stmt {
                    Stmt::Assign(destination, value)
                        if expr_uses_inferred_local(function.id, value, inferred) =>
                    {
                        if let Some(expected) =
                            resolved_local_type(function, *destination, inferred)
                        {
                            validate_contextual_scalar_value(
                                prog,
                                function,
                                value,
                                &expected,
                                interface,
                                entries,
                                inferred,
                            )
                            .map_err(|detail| {
                                DutAccessPlanError(format!(
                                    "function fn{} `{}` block b{block_index} assignment to %{} {detail}",
                                    function.id.0, function.name, destination.0
                                ))
                            })?;
                        }
                    }
                    Stmt::DutWrite(port, value) => {
                        let destination =
                            resolved_port_type(interface, entries, port).ok_or_else(|| {
                                use_error(
                                    function.id,
                                    &function.name,
                                    block_index,
                                    port,
                                    "has no resolved scalar destination type",
                                )
                            })?;
                        validate_contextual_scalar_value(
                            prog,
                            function,
                            value,
                            &destination,
                            interface,
                            entries,
                            inferred,
                        )
                        .map_err(|detail| {
                            use_error(
                                function.id,
                                &function.name,
                                block_index,
                                port,
                                &format!("has incompatible written value: {detail}"),
                            )
                        })?;
                    }
                    Stmt::ComponentCall {
                        function: target,
                        args,
                        ..
                    } => {
                        validate_call_arguments(
                            prog,
                            function,
                            block_index,
                            *target,
                            args,
                            &BTreeSet::new(),
                            interface,
                            entries,
                            inferred,
                        )?;
                    }
                    Stmt::TestbenchCall {
                        function: target,
                        args,
                        dut_args,
                        ..
                    } => {
                        validate_call_arguments(
                            prog,
                            function,
                            block_index,
                            *target,
                            args,
                            &dut_args.iter().copied().collect(),
                            interface,
                            entries,
                            inferred,
                        )?;
                    }
                    _ => {}
                }
                crate::ir::visit::try_visit_stmt_exprs(stmt, &mut |expr| {
                    validate_expr_call_arguments(
                        prog,
                        function,
                        block_index,
                        expr,
                        interface,
                        entries,
                        inferred,
                    )
                })?;
            }
            crate::ir::visit::try_visit_terminator_exprs(&block.terminator, &mut |expr| {
                validate_expr_call_arguments(
                    prog,
                    function,
                    block_index,
                    expr,
                    interface,
                    entries,
                    inferred,
                )
            })?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_resolved_statement_sinks(
    prog: &TbProgram,
    function: &crate::ir::TbFunction,
    block: usize,
    stmt: &Stmt,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Result<(), DutAccessPlanError> {
    let validate = |value: &Expr,
                    expected: Option<IrType>,
                    destination: String|
     -> Result<(), DutAccessPlanError> {
        if !expr_uses_inferred_local(function.id, value, inferred) {
            return Ok(());
        }
        let expected = expected.ok_or_else(|| {
            DutAccessPlanError(format!(
                "function fn{} `{}` block b{block} cannot resolve {destination} type after DUT read inference",
                function.id.0, function.name
            ))
        })?;
        validate_contextual_scalar_value(
            prog, function, value, &expected, interface, entries, inferred,
        )
        .map_err(|detail| {
            DutAccessPlanError(format!(
                "function fn{} `{}` block b{block} {destination} {detail}",
                function.id.0, function.name
            ))
        })
    };

    match stmt {
        Stmt::RecordFieldWrite {
            local,
            field,
            path,
            mid_indices,
            index,
            value,
        } => validate(
            value,
            resolved_host_state_type(
                prog,
                function,
                &Expr::RecordField {
                    local: *local,
                    field: field.clone(),
                    path: path.clone(),
                    mid_indices: mid_indices.clone(),
                    index: index.clone().map(Box::new),
                },
            ),
            format!("record field `{}`", record_path_label(field, path)),
        )?,
        Stmt::RecordWriteCb {
            local,
            field,
            value,
            ..
        } => validate(
            value,
            resolved_host_state_type(
                prog,
                function,
                &Expr::RecordField {
                    local: *local,
                    field: field.clone(),
                    path: Vec::new(),
                    mid_indices: Vec::new(),
                    index: None,
                },
            ),
            format!("record callback field `{field}`"),
        )?,
        Stmt::TbFieldWrite { field, value } => validate(
            value,
            resolved_host_state_type(prog, function, &Expr::TbField(field.clone())),
            format!("testbench field `{field}`"),
        )?,
        Stmt::TbFieldVecElementWrite {
            field,
            index,
            inner_index,
            value,
        } => validate(
            value,
            resolved_host_state_type(
                prog,
                function,
                &Expr::TbFieldVecElement {
                    field: field.clone(),
                    index: Box::new(index.clone()),
                    inner_index: inner_index.clone().map(Box::new),
                },
            ),
            format!("testbench vector field `{field}`"),
        )?,
        Stmt::TbQueuePush { field, value } => validate(
            value,
            function_testbench(prog, function)
                .and_then(|testbench| {
                    testbench
                        .queue_fields
                        .iter()
                        .find(|queue| queue.name == *field)
                })
                .map(|queue| queue.elem.ir_type()),
            format!("testbench queue `{field}`"),
        )?,
        Stmt::TransactorStateWrite {
            instance,
            field,
            value,
        } => validate(
            value,
            resolved_host_state_type(
                prog,
                function,
                &Expr::TransactorState {
                    instance: instance.clone(),
                    field: field.clone(),
                },
            ),
            format!("transactor state `{instance}.{field}`"),
        )?,
        Stmt::TransactorStateRecordFieldWrite {
            instance,
            field,
            path,
            mid_indices,
            index,
            value,
        } => validate(
            value,
            resolved_host_state_type(
                prog,
                function,
                &Expr::TransactorStateRecordField {
                    instance: instance.clone(),
                    field: field.clone(),
                    path: path.clone(),
                    mid_indices: mid_indices.clone(),
                    index: index.clone().map(Box::new),
                },
            ),
            format!(
                "transactor state field `{instance}.{field}.{}`",
                path.join(".")
            ),
        )?,
        Stmt::TransactorStateQueuePush {
            instance,
            field,
            value,
        } => validate(
            value,
            transactor_state_field(prog, function, instance, field).and_then(|kind| match kind {
                crate::ir::StateFieldKind::Queue { elem } => Some(elem.ir_type()),
                _ => None,
            }),
            format!("transactor state queue `{instance}.{field}`"),
        )?,
        Stmt::ScoreboardOp { sb, op, .. } => match op {
            crate::ir::ScoreboardOp::QueuePush { queue, value } => validate(
                value,
                prog.scoreboards
                    .get(sb.index())
                    .and_then(|schema| schema.field(queue))
                    .and_then(|field| match &field.kind {
                        crate::ir::ScoreboardFieldKind::Queue { elem } => Some(elem.ir_type()),
                        _ => None,
                    }),
                format!("scoreboard queue `{queue}`"),
            )?,
            crate::ir::ScoreboardOp::ScalarWrite { scalar, value } => validate(
                value,
                prog.scoreboards
                    .get(sb.index())
                    .and_then(|schema| schema.field(scalar))
                    .and_then(|field| match &field.kind {
                        crate::ir::ScoreboardFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                        crate::ir::ScoreboardFieldKind::Record { record } => {
                            Some(IrType::Record(*record))
                        }
                        _ => None,
                    }),
                format!("scoreboard field `{scalar}`"),
            )?,
            crate::ir::ScoreboardOp::QueuePop { .. } => {}
        },
        Stmt::ComponentFieldWrite { base, field, value } => validate(
            value,
            resolved_host_state_type(
                prog,
                function,
                &Expr::ComponentField {
                    base: base.clone(),
                    field: field.clone(),
                },
            ),
            format!("component field `{field}`"),
        )?,
        Stmt::ComponentVecElementWrite {
            base,
            field,
            index_pos,
            index,
            inner_index,
            value,
        } => validate(
            value,
            resolved_host_state_type(
                prog,
                function,
                &Expr::ComponentVecElement {
                    base: base.clone(),
                    field: field.clone(),
                    index_pos: *index_pos,
                    index: Box::new(index.clone()),
                    inner_index: inner_index.clone().map(Box::new),
                },
            ),
            format!("component vector field `{field}`"),
        )?,
        Stmt::ComponentQueuePush { base, queue, value } => validate(
            value,
            component_base_type(prog, function, base)
                .and_then(|component| prog.components.get(component.index()))
                .and_then(|schema| schema.field(queue))
                .and_then(|field| match &field.kind {
                    crate::ir::ComponentFieldKind::Queue { elem } => Some(elem.ir_type()),
                    _ => None,
                }),
            format!("component queue `{queue}`"),
        )?,
        Stmt::EventEmit { event, args } => {
            let expected =
                resolved_local_type(function, *event, inferred).and_then(|ty| match ty {
                    IrType::Event(payload) => Some(event_payload_type(payload)),
                    _ => None,
                });
            for (index, value) in args.iter().enumerate() {
                validate(
                    value,
                    expected.clone(),
                    format!("event argument {}", index + 1),
                )?;
            }
        }
        Stmt::ComponentEmit {
            base,
            subpath,
            event,
            args,
        } => {
            let expected = component_base_type(prog, function, base)
                .and_then(|component| {
                    crate::ir::resolve_component_path_mode(
                        &prog.components,
                        component,
                        None,
                        subpath,
                    )
                    .ok()
                    .map(|resolved| resolved.component)
                })
                .and_then(|component| prog.components.get(component.index()))
                .and_then(|schema| schema.field(event))
                .and_then(|field| match &field.kind {
                    crate::ir::ComponentFieldKind::Event { payload } => {
                        Some(event_payload_type(payload.clone()))
                    }
                    _ => None,
                });
            for (index, value) in args.iter().enumerate() {
                validate(
                    value,
                    expected.clone(),
                    format!("component event `{event}` argument {}", index + 1),
                )?;
            }
        }
        Stmt::SeqPush { seq, value } => validate(
            value,
            resolved_local_type(function, *seq, inferred).and_then(|ty| match ty {
                IrType::RecordSeq(record) => Some(IrType::Record(record)),
                IrType::Seq(element) => Some(*element),
                _ => None,
            }),
            format!("sequence %{} element", seq.0),
        )?,
        _ => {}
    }
    Ok(())
}

fn record_path_label(field: &str, path: &[String]) -> String {
    std::iter::once(field)
        .chain(path.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(".")
}

fn event_payload_type(payload: crate::ir::EventPayload) -> IrType {
    payload.value_ir_type()
}

#[allow(clippy::too_many_arguments)]
fn validate_call_arguments(
    prog: &TbProgram,
    caller: &crate::ir::TbFunction,
    block: usize,
    target: FunctionId,
    args: &[Expr],
    skipped: &BTreeSet<usize>,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Result<(), DutAccessPlanError> {
    let Some(target) = prog
        .functions
        .get(target.index())
        .filter(|candidate| candidate.id == target)
    else {
        return Ok(());
    };
    for (index, (arg, parameter)) in args.iter().zip(&target.params).enumerate() {
        if skipped.contains(&index) || !expr_uses_inferred_local(caller.id, arg, inferred) {
            continue;
        }
        validate_contextual_scalar_value(
            prog,
            caller,
            arg,
            &parameter.ty,
            interface,
            entries,
            inferred,
        )
        .map_err(|detail| {
            DutAccessPlanError(format!(
                "function fn{} `{}` block b{block} call to fn{} `{}` argument {} {detail}",
                caller.id.0,
                caller.name,
                target.id.0,
                target.name,
                index + 1
            ))
        })?;
    }
    Ok(())
}

fn validate_expr_call_arguments(
    prog: &TbProgram,
    caller: &crate::ir::TbFunction,
    block: usize,
    expr: &Expr,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Result<(), DutAccessPlanError> {
    crate::ir::visit::try_walk_expr(expr, &mut |expr| {
        validate_one_expr_call_arguments(prog, caller, block, expr, interface, entries, inferred)
    })
}

fn validate_one_expr_call_arguments(
    prog: &TbProgram,
    caller: &crate::ir::TbFunction,
    block: usize,
    expr: &Expr,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Result<(), DutAccessPlanError> {
    let Expr::Call(target, args) = expr else {
        return Ok(());
    };
    let parameter_types: Option<Vec<IrType>> = match target {
        crate::ir::CallTarget::Helper { function, .. }
        | crate::ir::CallTarget::Tseq { function, .. } => prog
            .functions
            .get(function.index())
            .filter(|candidate| candidate.id == *function)
            .map(|function| {
                function
                    .params
                    .iter()
                    .map(|param| param.ty.clone())
                    .collect()
            }),
        crate::ir::CallTarget::ExternFn { params, .. } => Some(params.clone()),
        crate::ir::CallTarget::TransactorSelfMethod { function, .. } => prog
            .functions
            .get(function.index())
            .filter(|candidate| candidate.id == *function)
            .map(|function| {
                function
                    .params
                    .iter()
                    .map(|param| param.ty.clone())
                    .collect()
            }),
        crate::ir::CallTarget::TransactorMethod {
            target: crate::ir::TransactorMethodTarget::Callable { function, .. },
            ..
        } => prog
            .functions
            .get(function.index())
            .filter(|candidate| candidate.id == *function)
            .map(|function| {
                function
                    .params
                    .iter()
                    .map(|param| param.ty.clone())
                    .collect()
            }),
        _ => None,
    };
    let Some(parameter_types) = parameter_types else {
        return Ok(());
    };
    for (index, (arg, expected)) in args.iter().zip(parameter_types.iter()).enumerate() {
        if !expr_uses_inferred_local(caller.id, arg, inferred) {
            continue;
        }
        validate_contextual_scalar_value(prog, caller, arg, expected, interface, entries, inferred)
            .map_err(|detail| {
                DutAccessPlanError(format!(
                    "function fn{} `{}` block b{block} call argument {} {detail}",
                    caller.id.0,
                    caller.name,
                    index + 1
                ))
            })?;
    }
    Ok(())
}

fn expr_uses_inferred_local(
    function: FunctionId,
    expr: &Expr,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> bool {
    let mut found = false;
    crate::ir::visit::walk_expr(expr, &mut |expr| {
        if let Expr::Local(local) = expr {
            found |= inferred.contains_key(&(function, *local));
        }
    });
    found
}

fn expr_uses_resolved_dut_type(
    function: FunctionId,
    expr: &Expr,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> bool {
    let mut found = false;
    crate::ir::visit::walk_expr(expr, &mut |expr| match expr {
        Expr::Local(local) => found |= inferred.contains_key(&(function, *local)),
        Expr::Port(port) | Expr::PortSnapshotLane { port, .. } => {
            found |= port.origin == PortOrigin::Dut;
        }
        _ => {}
    });
    found
}

fn resolved_local_type(
    function: &crate::ir::TbFunction,
    local: LocalId,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Option<IrType> {
    let declared = function.locals.get(local.index())?.ty.clone();
    if matches!(declared, IrType::Unknown) {
        inferred
            .get(&(function.id, local))
            .cloned()
            .or(Some(declared))
    } else {
        Some(declared)
    }
}

fn resolved_port_type(
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    port: &PortRef,
) -> Option<IrType> {
    if port.origin != PortOrigin::Dut {
        return port
            .value_type
            .clone()
            .or_else(|| Some(IrType::UInt(port.width)));
    }
    let identity = resolved_access_identity(interface, port).ok()?;
    entries.get(&identity)?.value_type_for(port)
}

fn resolved_assignment_expr_type(
    prog: &TbProgram,
    function: &crate::ir::TbFunction,
    value: &Expr,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Option<IrType> {
    if let Expr::PortSnapshotLane { port, .. } = value {
        let identity = resolved_access_identity(interface, port).ok()?;
        return entries.get(&identity)?.lane_value_type();
    }
    super::super::verify::assignment_expr_type_with(
        prog,
        function,
        value,
        &|local| resolved_local_type(function, local, inferred),
        &|port| resolved_port_type(interface, entries, port),
        &|leaf| resolved_host_state_type(prog, function, leaf),
    )
}

fn resolved_contextual_expr_type(
    prog: &TbProgram,
    function: &crate::ir::TbFunction,
    value: &Expr,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Option<IrType> {
    use crate::ir::scalar::{scalar_value_evidence, ScalarValueEvidence};

    scalar_value_evidence(value, &|leaf| {
        resolved_assignment_expr_type(prog, function, leaf, interface, entries, inferred)
    })
    .map(|evidence| match evidence {
        ScalarValueEvidence::Bool => IrType::Bool,
        ScalarValueEvidence::Integer {
            width,
            signed: Some(true),
            ..
        } => IrType::SInt(width),
        ScalarValueEvidence::Integer {
            width,
            signed: Some(false),
            ..
        } => IrType::UInt(width),
        ScalarValueEvidence::Integer {
            width,
            signed: None,
            exact: Some(value),
            ..
        } if value < 0 => IrType::SInt(width),
        ScalarValueEvidence::Integer { width, .. } => IrType::UInt(width),
    })
    .or_else(|| resolved_assignment_expr_type(prog, function, value, interface, entries, inferred))
}

fn resolved_host_state_type(
    prog: &TbProgram,
    function: &crate::ir::TbFunction,
    expr: &Expr,
) -> Option<IrType> {
    match expr {
        Expr::TbField(name) => function_testbench(prog, function)?
            .scalar_fields
            .iter()
            .find(|field| field.name == *name)
            .map(|field| field.ty.clone()),
        Expr::TbFieldVecElement {
            field, inner_index, ..
        } => {
            let ty = &function_testbench(prog, function)?
                .scalar_fields
                .iter()
                .find(|candidate| candidate.name == *field)?
                .ty;
            fixed_vec_element_type(ty, inner_index.is_some())
        }
        Expr::RecordField {
            local,
            field,
            path,
            mid_indices,
            index,
        } => {
            let IrType::Record(record) = function.locals.get(local.index())?.ty else {
                return None;
            };
            let segments = std::iter::once(field.as_str())
                .chain(path.iter().map(String::as_str))
                .collect::<Vec<_>>();
            let mid_positions = mid_indices
                .iter()
                .map(|(position, _)| *position)
                .collect::<BTreeSet<_>>();
            record_leaf_type(prog, record, &segments, &mid_positions, index.is_some())
        }
        Expr::ScoreboardQuery {
            sb,
            query: crate::ir::ScoreboardQuery::Scalar { scalar },
            ..
        } => prog
            .scoreboards
            .get(sb.index())?
            .field(scalar)
            .and_then(|field| match &field.kind {
                crate::ir::ScoreboardFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                crate::ir::ScoreboardFieldKind::Record { record } => Some(IrType::Record(*record)),
                _ => None,
            }),
        Expr::TransactorState { instance, field } => {
            match transactor_state_field(prog, function, instance, field)? {
                crate::ir::StateFieldKind::Scalar { ty, .. } => Some(ty.clone()),
                crate::ir::StateFieldKind::Record { record } => Some(IrType::Record(*record)),
                crate::ir::StateFieldKind::FixedVec { ty } => Some(ty.clone()),
                crate::ir::StateFieldKind::Queue { .. } => None,
            }
        }
        Expr::TransactorStateRecordField {
            instance,
            field,
            path,
            mid_indices,
            index,
        } => {
            let crate::ir::StateFieldKind::Record { record } =
                transactor_state_field(prog, function, instance, field)?
            else {
                return None;
            };
            let segments = path.iter().map(String::as_str).collect::<Vec<_>>();
            let mid_positions = mid_indices
                .iter()
                .map(|(position, _)| *position)
                .collect::<BTreeSet<_>>();
            record_leaf_type(prog, *record, &segments, &mid_positions, index.is_some())
        }
        Expr::ComponentField { base, field } => {
            let component = component_base_type(prog, function, base)?;
            let mut segments = field.split('.');
            let root = prog
                .components
                .get(component.index())?
                .field(segments.next()?)?;
            let tail = segments.collect::<Vec<_>>();
            match (&root.kind, tail.is_empty()) {
                (crate::ir::ComponentFieldKind::Scalar { ty, .. }, true) => Some(ty.clone()),
                (crate::ir::ComponentFieldKind::Record { record }, false) => {
                    record_leaf_type(prog, *record, &tail, &BTreeSet::new(), false)
                }
                _ => None,
            }
        }
        Expr::ComponentVecElement {
            base,
            field,
            inner_index,
            ..
        } => {
            let component = component_base_type(prog, function, base)?;
            let schema = prog.components.get(component.index())?;
            let field = schema.field(field.split('.').next()?)?;
            match &field.kind {
                crate::ir::ComponentFieldKind::FixedVec(vector) => {
                    fixed_vec_element_type(&vector.elem, inner_index.is_some())
                        .or_else(|| (!inner_index.is_some()).then(|| vector.elem.clone()))
                }
                _ => None,
            }
        }
        Expr::SeqIndex { seq, .. } => match &function.locals.get(seq.index())?.ty {
            IrType::RecordSeq(record) => Some(IrType::Record(*record)),
            IrType::Seq(element) => Some((**element).clone()),
            _ => None,
        },
        Expr::RegRead { mirror, field, .. } => {
            let IrType::Record(record) = function.locals.get(mirror.index())?.ty else {
                return None;
            };
            record_leaf_type(prog, record, &[field.as_str()], &BTreeSet::new(), false)
        }
        _ => None,
    }
}

fn function_testbench<'a>(
    prog: &'a TbProgram,
    function: &crate::ir::TbFunction,
) -> Option<&'a crate::ir::TestbenchSchema> {
    if let Some(owner) = function.owner {
        return prog.testbenches.get(owner.index());
    }
    let crate::ir::FunctionKind::TestbenchMethod { testbench, .. } = function.kind else {
        return None;
    };
    prog.testbenches
        .iter()
        .find(|instance| instance.type_id == testbench)
}

fn fixed_vec_element_type(ty: &IrType, inner: bool) -> Option<IrType> {
    let IrType::FixedVec { elem, .. } = ty else {
        return None;
    };
    if !inner {
        return Some((**elem).clone());
    }
    let IrType::FixedVec { elem, .. } = elem.as_ref() else {
        return None;
    };
    Some((**elem).clone())
}

fn record_leaf_type(
    prog: &TbProgram,
    mut record: crate::ir::RecordId,
    segments: &[&str],
    mid_positions: &BTreeSet<usize>,
    leaf_indexed: bool,
) -> Option<IrType> {
    for (position, segment) in segments.iter().enumerate() {
        let field = prog.records.get(record.index())?.field(segment)?;
        let last = position + 1 == segments.len();
        if last {
            return match (field.vec_len, leaf_indexed) {
                (Some(_), false) => None,
                _ => Some(field.ty.clone()),
            };
        }
        let indexed = mid_positions.contains(&position);
        match (&field.ty, field.vec_len, indexed) {
            (IrType::Record(next), None, false) | (IrType::Record(next), Some(_), true) => {
                record = *next
            }
            _ => return None,
        }
    }
    None
}

fn transactor_state_field<'a>(
    prog: &'a TbProgram,
    function: &crate::ir::TbFunction,
    instance: &str,
    field: &str,
) -> Option<&'a crate::ir::StateFieldKind> {
    let transactor = match function.kind {
        crate::ir::FunctionKind::TransactorBody { transactor, .. } if instance.is_empty() => {
            transactor
        }
        _ => {
            let testbench = function_testbench(prog, function)?;
            testbench
                .target_tlm_actors
                .iter()
                .find(|actor| actor.instance == instance)
                .map(|actor| actor.transactor)
                .or_else(|| {
                    testbench
                        .unbound_state_actors
                        .iter()
                        .find(|actor| actor.field == instance)
                        .map(|actor| actor.transactor)
                })?
        }
    };
    prog.transactors
        .get(transactor.index())?
        .state_fields
        .iter()
        .find(|state| state.name == field)
        .map(|state| &state.kind)
}

fn component_base_type(
    prog: &TbProgram,
    function: &crate::ir::TbFunction,
    base: &crate::ir::ComponentBase,
) -> Option<ComponentId> {
    match base {
        crate::ir::ComponentBase::Local(local) => match function.locals.get(local.index())?.ty {
            IrType::Component(component) => Some(component),
            _ => None,
        },
        crate::ir::ComponentBase::SelfField => match function.kind {
            crate::ir::FunctionKind::ComponentMethod { component, .. } => Some(component),
            _ => None,
        },
        crate::ir::ComponentBase::Path(path) => {
            let (root, tail) = path.split_first()?;
            if root == "self" {
                let crate::ir::FunctionKind::ComponentMethod { component, .. } = function.kind
                else {
                    return None;
                };
                return crate::ir::resolve_component_path_mode(
                    &prog.components,
                    component,
                    None,
                    tail,
                )
                .ok()
                .map(|resolved| resolved.component);
            }
            let binding = function_testbench(prog, function)?
                .component_fields
                .iter()
                .find(|binding| binding.field == *root)?;
            crate::ir::resolve_component_path_mode(
                &prog.components,
                binding.component,
                binding.mode,
                tail,
            )
            .ok()
            .map(|resolved| resolved.component)
        }
    }
}

fn validate_contextual_scalar_value(
    prog: &TbProgram,
    function: &crate::ir::TbFunction,
    value: &Expr,
    expected: &IrType,
    interface: &DutInterfaceCatalog,
    entries: &BTreeMap<DutAccessIdentity, DutAccessEntry>,
    inferred: &BTreeMap<(FunctionId, LocalId), IrType>,
) -> Result<(), String> {
    use crate::ir::scalar::{contextual_value_bits, scalar_value_evidence, ScalarValueEvidence};

    if matches!(
        expected,
        IrType::Unknown | IrType::UInt(None) | IrType::SInt(None)
    ) {
        return Ok(());
    }
    let evidence = scalar_value_evidence(value, &|leaf| {
        resolved_assignment_expr_type(prog, function, leaf, interface, entries, inferred)
    });
    if matches!(expected, IrType::Bool) {
        return match evidence {
            Some(ScalarValueEvidence::Bool)
            | Some(ScalarValueEvidence::Integer {
                signed: None,
                exact: Some(0 | 1),
                ..
            }) => Ok(()),
            Some(_) => Err("does not fit Bool; use a boolean or an untyped literal 0/1".into()),
            None => Ok(()),
        };
    }
    let (destination_width, destination_signed) = match expected {
        IrType::UInt(Some(width)) => (*width, false),
        IrType::SInt(Some(width)) => (*width, true),
        _ => {
            let Some(actual) =
                resolved_assignment_expr_type(prog, function, value, interface, entries, inferred)
            else {
                return Ok(());
            };
            return super::super::verify::assign_compatible(expected, &actual)
                .then_some(())
                .ok_or_else(|| format!("has type {actual:?}, expected {expected:?}"));
        }
    };
    let Some(evidence) = evidence else {
        let Some(actual) =
            resolved_assignment_expr_type(prog, function, value, interface, entries, inferred)
        else {
            return Ok(());
        };
        return super::super::verify::assign_compatible(expected, &actual)
            .then_some(())
            .ok_or_else(|| format!("has type {actual:?}, expected {expected:?}"));
    };
    let ScalarValueEvidence::Integer {
        width,
        signed,
        exact,
        unsigned_bound,
    } = evidence
    else {
        return (destination_width >= 1)
            .then_some(())
            .ok_or_else(|| format!("does not fit {expected:?}"));
    };
    if let Some(source_signed) = signed {
        if source_signed != destination_signed {
            return Err(format!(
                "has {} signedness, expected {} {destination_width}-bit scalar",
                if source_signed { "signed" } else { "unsigned" },
                if destination_signed {
                    "signed"
                } else {
                    "unsigned"
                }
            ));
        }
    } else if exact.is_some_and(|value| value < 0) && !destination_signed {
        return Err(format!(
            "is negative, but the destination is unsigned {destination_width}-bit"
        ));
    }
    let source_width = match (signed, exact) {
        (None, Some(value)) => contextual_value_bits(value, destination_signed),
        (None, None) if destination_signed && unsigned_bound => {
            width.and_then(|width| width.checked_add(1))
        }
        _ => width,
    };
    if source_width.is_some_and(|width| width > destination_width) {
        return Err(format!(
            "needs {} bits, but the destination is {destination_width} bits; use an explicit truncation/cast",
            source_width.expect("checked")
        ));
    }
    Ok(())
}

fn access_semantic_key(access: &DutAccessEntry, probes: &[DutProbePlan]) -> String {
    match &access.identity {
        DutAccessIdentity::Port { .. } => format!(
            "port:{}:{:?}:{}",
            access.path.join("."),
            access.lane_shapes,
            access.aggregate
        ),
        DutAccessIdentity::Probe(id) => probes
            .iter()
            .find(|probe| probe.id == *id)
            .map(|probe| format!("probe:{}:{}", probe.name, probe.sv_path))
            .unwrap_or_else(|| format!("missing-probe:{}", id.0)),
    }
}

fn checked_function<'a>(
    prog: &'a TbProgram,
    function: FunctionId,
    owner: &str,
) -> Result<&'a crate::ir::TbFunction, DutAccessPlanError> {
    prog.functions
        .get(function.index())
        .filter(|candidate| candidate.id == function)
        .ok_or_else(|| {
            DutAccessPlanError(format!(
                "{owner} references missing function fn{}",
                function.0
            ))
        })
}

#[allow(clippy::too_many_arguments)]
fn record_property_reads(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
    entries: &mut BTreeMap<DutAccessIdentity, DutAccessEntry>,
    function: FunctionId,
    function_name: &str,
    block: usize,
    property: &crate::ir::PropertyCheckSchema,
) -> Result<(), DutAccessPlanError> {
    let mut record = |expr: &Expr| {
        record_expr_reads(
            prog,
            interface,
            entries,
            function,
            function_name,
            block,
            expr,
        )
    };
    match &property.shape {
        crate::ir::PropertyShape::Implies { ante, cons }
        | crate::ir::PropertyShape::ImpliesNext { ante, cons } => {
            record(ante)?;
            record(cons)?;
        }
        crate::ir::PropertyShape::Invariant(expr) => record(expr)?,
    }
    for temporal in &property.temporals {
        record(&temporal.inner)?;
    }
    if let Some(message) = &property.message {
        for arg in &message.args {
            record(&arg.expr)?;
        }
    }
    Ok(())
}

fn record_expr_reads(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
    entries: &mut BTreeMap<DutAccessIdentity, DutAccessEntry>,
    function: FunctionId,
    function_name: &str,
    block: usize,
    expr: &Expr,
) -> Result<(), DutAccessPlanError> {
    record_expr_reads_at(
        prog,
        interface,
        entries,
        DutAccessSite::Function(function),
        function,
        function_name,
        block,
        expr,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_expr_reads_at(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
    entries: &mut BTreeMap<DutAccessIdentity, DutAccessEntry>,
    site: DutAccessSite,
    function: FunctionId,
    function_name: &str,
    block: usize,
    expr: &Expr,
) -> Result<(), DutAccessPlanError> {
    crate::ir::visit::try_walk_expr(expr, &mut |expr| match expr {
        Expr::Port(port) => record_use_at(
            prog,
            interface,
            entries,
            site,
            function,
            function_name,
            block,
            port,
            DutAccessOperation::Read,
            port.value_type.clone(),
        ),
        Expr::PortSnapshotLane { port, index, .. } => {
            let mut indexed = port.clone();
            indexed.lane = Some(LaneIndex::Var(index.clone()));
            record_use_at(
                prog,
                interface,
                entries,
                site,
                function,
                function_name,
                block,
                &indexed,
                DutAccessOperation::Read,
                None,
            )
        }
        _ => Ok(()),
    })
}

#[allow(clippy::too_many_arguments)]
fn record_use(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
    entries: &mut BTreeMap<DutAccessIdentity, DutAccessEntry>,
    function: FunctionId,
    function_name: &str,
    block: usize,
    port: &PortRef,
    operation: DutAccessOperation,
    transfer_type: Option<IrType>,
) -> Result<(), DutAccessPlanError> {
    record_use_at(
        prog,
        interface,
        entries,
        DutAccessSite::Function(function),
        function,
        function_name,
        block,
        port,
        operation,
        transfer_type,
    )
}

#[allow(clippy::too_many_arguments)]
fn record_use_at(
    prog: &TbProgram,
    interface: &DutInterfaceCatalog,
    entries: &mut BTreeMap<DutAccessIdentity, DutAccessEntry>,
    site: DutAccessSite,
    function: FunctionId,
    function_name: &str,
    block: usize,
    port: &PortRef,
    operation: DutAccessOperation,
    transfer_type: Option<IrType>,
) -> Result<(), DutAccessPlanError> {
    if port.origin != PortOrigin::Dut {
        return Ok(());
    }
    let expected_fields = expected_dut_fields(prog, function, interface.dut_type())?;
    if !expected_fields.contains(port.testbench_field.as_str()) {
        return Err(use_error(
            function,
            function_name,
            block,
            port,
            &format!(
                "uses receiver `{}` but its verified DUT receiver set is [{}]",
                port.testbench_field,
                expected_fields
                    .iter()
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let lane_shape = match port.lane {
        None => DutLaneShape::None,
        Some(LaneIndex::Const(_)) => DutLaneShape::Constant,
        Some(LaneIndex::Var(_)) => DutLaneShape::Dynamic,
    };
    let (
        identity,
        width,
        value_type,
        resolved_direction,
        resolved_path,
        resolved_aggregate,
        resolved_packed_lane_width,
        resolved_packed_lane_type,
    ) = match port.access {
        PortAccess::Port => {
            if port.probe.is_some() {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    "ordinary port carries a probe identity",
                ));
            }
            port.port_path.first().ok_or_else(|| {
                use_error(function, function_name, block, port, "DUT path is empty")
            })?;
            if port.port_path.len() > 1 && !port.aggregate_path {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    "uses a multi-segment non-aggregate DUT path",
                ));
            }
            let exact_name = port.port_path.join(".");
            let flattened_name = port.port_path.join("_");
            let (declared, resolved_path, resolved_aggregate) =
                resolve_interface_port(interface, port).ok_or_else(|| {
                    use_error(
                        function,
                        function_name,
                        block,
                        port,
                        &format!(
                            "is absent from the DUT interface catalog (looked for `{exact_name}` and `{flattened_name}`)"
                        ),
                    )
                })?;
            let declared_width = declared.width.ok_or_else(|| {
                use_error(
                    function,
                    function_name,
                    block,
                    port,
                    "has a parameter-dependent DUT width that is unresolved in the effective interface catalog",
                )
            })?;
            let declared_type = declared.value_type.clone();
            let packed_lane_width = declared.packed_lane_width;
            let packed_lane_type = declared.packed_lane_type.clone();
            let unpacked_elements = declared.unpacked_elements;
            if port.lane.is_some() && packed_lane_width.is_none() && unpacked_elements.is_none() {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    "indexes a DUT port whose resolved interface is scalar",
                ));
            }
            if port.lane.is_some() && packed_lane_width.is_some_and(|width| width > 64) {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    "indexes a packed DUT lane wider than the supported 64-bit lane helper",
                ));
            }
            if port.lane.is_none() && unpacked_elements.is_some() {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    "uses an unpacked-array DUT port without an element index",
                ));
            }
            let discovered = match &port.lane {
                None => declared_width,
                Some(_) => packed_lane_width.unwrap_or(declared_width),
            };
            if discovered == 0 || discovered > crate::MAX_WIDTH_METHOD_BITS {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    &format!(
                        "has resolved width {discovered}, outside the supported 1..={} scalar range",
                        crate::MAX_WIDTH_METHOD_BITS
                    ),
                ));
            }
            let discovered_type = if port.lane.is_some() {
                packed_lane_type
                    .clone()
                    .unwrap_or_else(|| match &declared_type {
                        IrType::Bool if discovered == 1 => IrType::Bool,
                        IrType::SInt(_) => IrType::SInt(Some(discovered)),
                        _ => IrType::UInt(Some(discovered)),
                    })
            } else {
                declared_type.clone()
            };
            if let Some(LaneIndex::Const(index)) = &port.lane {
                let elements = unpacked_elements.or_else(|| {
                    packed_lane_width.and_then(|lane_width| declared_width.checked_div(lane_width))
                });
                if elements.is_some_and(|elements| *index >= u64::from(elements)) {
                    return Err(use_error(
                        function,
                        function_name,
                        block,
                        port,
                        &format!(
                            "lane index {index} is outside the resolved {}-element DUT shape",
                            elements.expect("checked above")
                        ),
                    ));
                }
            }
            if let Some(recorded) = port.width {
                if recorded != discovered {
                    return Err(use_error(
                        function,
                        function_name,
                        block,
                        port,
                        &format!(
                            "IR width {recorded} conflicts with resolved DUT width {discovered}"
                        ),
                    ));
                }
            }
            if port
                .direction
                .is_some_and(|direction| direction != declared.direction)
            {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    &format!(
                        "IR direction {:?} conflicts with DUT direction {:?}",
                        port.direction, declared.direction
                    ),
                ));
            }
            if matches!(operation, DutAccessOperation::Write)
                && declared.direction == PortDirection::Out
            {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    "writes an output-only DUT port",
                ));
            }
            if port
                .value_type
                .as_ref()
                .is_some_and(|value_type| value_type != &discovered_type)
            {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    &format!(
                        "IR type {:?} conflicts with resolved DUT type {:?}",
                        port.value_type, discovered_type
                    ),
                ));
            }
            if matches!(operation, DutAccessOperation::Read) {
                validate_transfer_type(
                    function,
                    function_name,
                    block,
                    port,
                    operation,
                    &discovered_type,
                    transfer_type.as_ref(),
                )?;
            }
            (
                DutAccessIdentity::Port {
                    path: resolved_path.clone(),
                    aggregate: resolved_aggregate,
                },
                declared_width,
                Some(declared_type),
                Some(declared.direction),
                resolved_path,
                resolved_aggregate,
                packed_lane_width,
                packed_lane_type,
            )
        }
        PortAccess::Probe | PortAccess::Force => {
            let id = port.probe.ok_or_else(|| {
                use_error(
                    function,
                    function_name,
                    block,
                    port,
                    "probe access has no ProbeId",
                )
            })?;
            let probe = prog
                .probes
                .get(id.index())
                .filter(|probe| probe.id == id)
                .ok_or_else(|| {
                    use_error(
                        function,
                        function_name,
                        block,
                        port,
                        &format!("references missing probe p{}", id.0),
                    )
                })?;
            let expected_access = if probe.force {
                PortAccess::Force
            } else {
                PortAccess::Probe
            };
            let expected_type = probe.ty.ir_type();
            if !expected_fields.contains(port.testbench_field.as_str())
                || port.port_path != [probe.name.clone()]
                || !port.aggregate_path
                || port.direction.is_some()
                || port.width != Some(probe.ty.width())
                || port.value_type.as_ref() != Some(&expected_type)
                || port.access != expected_access
                || port.lane.is_some()
            {
                return Err(use_error(
                    function,
                    function_name,
                    block,
                    port,
                    &format!(
                        "does not match probe p{} `{}` ({:?} at `{}`, force={})",
                        id.0, probe.name, probe.ty, probe.sv_path, probe.force
                    ),
                ));
            }
            match operation {
                DutAccessOperation::Write if !probe.force => {
                    return Err(use_error(
                        function,
                        function_name,
                        block,
                        port,
                        "writes a read-only probe",
                    ));
                }
                DutAccessOperation::ForceWrite | DutAccessOperation::Release if !probe.force => {
                    return Err(use_error(
                        function,
                        function_name,
                        block,
                        port,
                        "requires force capability",
                    ));
                }
                _ => {}
            }
            if matches!(operation, DutAccessOperation::Read) {
                validate_transfer_type(
                    function,
                    function_name,
                    block,
                    port,
                    operation,
                    &expected_type,
                    transfer_type.as_ref(),
                )?;
            }
            (
                DutAccessIdentity::Probe(id),
                probe.ty.width(),
                Some(expected_type),
                None,
                port.port_path.clone(),
                port.aggregate_path,
                None,
                None,
            )
        }
    };
    let candidate = DutAccessEntry {
        identity: identity.clone(),
        source_path: port.port_path.clone(),
        source_aggregate: port.aggregate_path,
        path: resolved_path.clone(),
        aggregate: resolved_aggregate,
        direction: resolved_direction,
        width,
        value_type: value_type.clone(),
        packed_lane_width: resolved_packed_lane_width,
        packed_lane_type: resolved_packed_lane_type.clone(),
        access: port.access,
        lane_shapes: BTreeSet::new(),
        operations: BTreeSet::new(),
        uses: Vec::new(),
    };
    let entry = entries.entry(identity).or_insert(candidate);
    if entry.path != resolved_path
        || entry.aggregate != resolved_aggregate
        || entry.source_path != port.port_path
        || entry.source_aggregate != port.aggregate_path
        || entry.direction != resolved_direction
        || entry.width != width
        || entry.value_type != value_type
        || entry.packed_lane_width != resolved_packed_lane_width
        || entry.packed_lane_type != resolved_packed_lane_type
        || entry.access != port.access
    {
        return Err(use_error(
            function,
            function_name,
            block,
            port,
            "aliases or conflicts with an earlier access to the same resolved DUT signal",
        ));
    }
    entry.lane_shapes.insert(lane_shape);
    entry.operations.insert(operation);
    entry.uses.push(DutAccessUse {
        site,
        operation,
        lane_shape,
    });
    Ok(())
}

fn expected_dut_fields<'a>(
    prog: &'a TbProgram,
    function: FunctionId,
    dut_type: &str,
) -> Result<BTreeSet<&'a str>, DutAccessPlanError> {
    let function = prog
        .functions
        .get(function.index())
        .filter(|candidate| candidate.id == function)
        .ok_or_else(|| {
            DutAccessPlanError(format!(
                "DUT access references missing function fn{}",
                function.0
            ))
        })?;
    if let Some(owner) = function.owner {
        let testbench = prog.testbenches.get(owner.index()).ok_or_else(|| {
            DutAccessPlanError(format!(
                "function fn{} `{}` references missing testbench tb{}",
                function.id.0, function.name, owner.0
            ))
        })?;
        return Ok(BTreeSet::from([testbench.dut_field.as_str()]));
    }
    let mut fields = BTreeSet::new();
    match function.kind {
        crate::ir::FunctionKind::TestbenchMethod {
            testbench, method, ..
        } => {
            let schema = prog.testbench_types.get(testbench.index()).ok_or_else(|| {
                DutAccessPlanError(format!(
                    "testbench method fn{} `{}` references missing testbench type tbt{}",
                    function.id.0, function.name, testbench.0
                ))
            })?;
            let method = schema.methods.get(method.index()).ok_or_else(|| {
                DutAccessPlanError(format!(
                    "testbench method fn{} `{}` references missing member tbm{}",
                    function.id.0, function.name, method.0
                ))
            })?;
            for (name, module) in method.param_names.iter().zip(&method.module_param_types) {
                if module.as_deref() == Some(dut_type) {
                    fields.insert(name.as_str());
                }
            }
            for instance in prog
                .testbenches
                .iter()
                .filter(|instance| instance.type_id == testbench)
            {
                if instance.dut_type == dut_type {
                    fields.insert(instance.dut_field.as_str());
                }
            }
        }
        crate::ir::FunctionKind::ComponentMethod { component, .. } => {
            fields.insert("dut");
            let schema = prog.components.get(component.index()).ok_or_else(|| {
                DutAccessPlanError(format!(
                    "component function fn{} `{}` references missing component c{}",
                    function.id.0, function.name, component.0
                ))
            })?;
            for field in &schema.fields {
                if matches!(
                    &field.kind,
                    crate::ir::ComponentFieldKind::Dut { dut_type: field_type }
                        if field_type == dut_type
                ) {
                    fields.insert(field.name.as_str());
                }
            }
        }
        _ => {
            fields.insert("dut");
        }
    }
    Ok(fields)
}

#[allow(clippy::too_many_arguments)]
fn validate_transfer_type(
    function: FunctionId,
    function_name: &str,
    block: usize,
    port: &PortRef,
    operation: DutAccessOperation,
    signal_type: &IrType,
    transfer_type: Option<&IrType>,
) -> Result<(), DutAccessPlanError> {
    let Some(transfer_type) = transfer_type else {
        return Ok(());
    };
    if matches!(transfer_type, IrType::PortSnapshot) {
        return Ok(());
    }
    let (expected, actual, phrase) = match operation {
        DutAccessOperation::Read => (transfer_type, signal_type, "read destination"),
        DutAccessOperation::Write | DutAccessOperation::ForceWrite => {
            (signal_type, transfer_type, "written value")
        }
        DutAccessOperation::Release => return Ok(()),
    };
    if !super::super::verify::assign_compatible(expected, actual) {
        return Err(use_error(
            function,
            function_name,
            block,
            port,
            &format!(
                "has incompatible {phrase} type {transfer_type:?} for resolved signal type {signal_type:?}"
            ),
        ));
    }
    Ok(())
}

fn resolved_access_identity(
    interface: &DutInterfaceCatalog,
    port: &PortRef,
) -> Result<DutAccessIdentity, DutAccessPlanError> {
    match port.access {
        PortAccess::Port => {
            let (_, path, aggregate) =
                resolve_interface_port(interface, port).ok_or_else(|| {
                    DutAccessPlanError(format!(
                        "DUT access `{}` is absent from the interface catalog",
                        display_port(port)
                    ))
                })?;
            Ok(DutAccessIdentity::Port { path, aggregate })
        }
        PortAccess::Probe | PortAccess::Force => {
            port.probe.map(DutAccessIdentity::Probe).ok_or_else(|| {
                DutAccessPlanError(format!(
                    "probe access `{}` has no ProbeId",
                    display_port(port)
                ))
            })
        }
    }
}

fn resolve_interface_port<'a>(
    interface: &'a DutInterfaceCatalog,
    port: &PortRef,
) -> Option<(&'a DutInterfacePort, Vec<String>, bool)> {
    let exact_name = port.port_path.join(".");
    let flattened_name = port.port_path.join("_");
    interface
        .port(&exact_name)
        .map(|declared| {
            (
                declared,
                declared.name.split('.').map(str::to_string).collect(),
                declared.name.contains('.'),
            )
        })
        .or_else(|| {
            interface
                .port_by_physical_name(&exact_name)
                .map(|declared| (declared, port.port_path.clone(), port.aggregate_path))
        })
        .or_else(|| {
            (flattened_name != exact_name)
                .then(|| interface.port_by_physical_name(&flattened_name))
                .flatten()
                .map(|declared| (declared, vec![flattened_name], false))
        })
}

fn display_port(port: &PortRef) -> String {
    format!("{}.{}", port.testbench_field, port.port_path.join("."))
}

fn probe_paths_overlap(lhs: &str, rhs: &str) -> bool {
    fn is_prefix(prefix: &str, path: &str) -> bool {
        path.strip_prefix(prefix).is_some_and(|suffix| {
            suffix.is_empty() || suffix.starts_with('.') || suffix.starts_with('[')
        })
    }
    is_prefix(lhs, rhs) || is_prefix(rhs, lhs)
}

fn use_error(
    function: FunctionId,
    function_name: &str,
    block: usize,
    port: &PortRef,
    detail: &str,
) -> DutAccessPlanError {
    DutAccessPlanError(format!(
        "function fn{} `{function_name}` block b{block} access `{}` {detail}",
        function.0,
        display_port(port)
    ))
}
