//! Minimal verified-IR common-object layout for TB-IR.
//!
//! The planner closes shared types, callables, DUT/bus adapters, per-test clock
//! topology, and runtime ownership before rendering. Unsupported semantic
//! families remain fail-closed so later common-object tickets can widen the
//! surface without weakening the plan/artifact boundary.

use super::func;
use crate::ast::SourceFile;
use crate::codegen::common_artifacts::{self, AbiAnchor, CommonArtifactPlan};
use crate::codegen::cpp_tb::{EmitError, EmitOpts, TbirRandomizeEmissionPlan};
pub use crate::ir::passes::callable_placement::{
    CallableOwner, CallablePlacement, CapsulePlacementReason, ComponentCallableMember,
    InvalidPlacementReason,
};
use crate::ir::passes::dut_access::{DutAccessPlan, DutAccessSite};
use crate::ir::passes::runtime_cells::RuntimeCellPlan;
use crate::ir::{
    self, ComponentId, CovgroupId, Expr, FmtArgs, FunctionId, FunctionKind, IrType, PortAccess,
    PortRef, RecordId, ScoreboardId, Stmt, TbFunction, TbProgram, Terminator, TestSchema,
    TestbenchId, TransactorId,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const INDENT: &str = "    ";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonClockPlan {
    name: String,
    period_ps: i64,
    domain: Option<String>,
}

impl CommonClockPlan {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn period_ps(&self) -> i64 {
        self.period_ps
    }

    pub fn domain(&self) -> Option<&str> {
        self.domain.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonTestClockPlan {
    test_index: usize,
    clocks: Vec<CommonClockPlan>,
}

impl CommonTestClockPlan {
    pub fn test_index(&self) -> usize {
        self.test_index
    }

    pub fn clocks(&self) -> &[CommonClockPlan] {
        &self.clocks
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonTestBodyPlan {
    test_index: usize,
    run: FunctionId,
    check: Option<FunctionId>,
    test_hooks: Vec<FunctionId>,
    run_callback_captures: HashSet<ir::LocalId>,
    check_callback_captures: HashSet<ir::LocalId>,
    placement_reason: CapsulePlacementReason,
}

impl CommonTestBodyPlan {
    pub fn test_index(&self) -> usize {
        self.test_index
    }

    pub fn run(&self) -> FunctionId {
        self.run
    }

    pub fn check(&self) -> Option<FunctionId> {
        self.check
    }

    pub fn test_hooks(&self) -> &[FunctionId] {
        &self.test_hooks
    }

    pub fn placement_reason(&self) -> CapsulePlacementReason {
        self.placement_reason
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonCapsulePlan {
    index: usize,
    test_bodies: Vec<CommonTestBodyPlan>,
    artifact_index: usize,
    dut_access: CommonDutAccessProfile,
    build_profile: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonDutAccessProfile {
    sites: BTreeSet<DutAccessSite>,
    functions: BTreeSet<FunctionId>,
    digest: String,
    uses_probe: bool,
}

impl CommonDutAccessProfile {
    fn new(plan: &DutAccessPlan, sites: BTreeSet<DutAccessSite>) -> Self {
        let digest = common_artifacts::stable_hash_hex(
            plan.access_lines_for_sites(&sites).join("\n").as_bytes(),
        );
        let uses_probe = plan.sites_use_probe(&sites);
        let functions = sites
            .iter()
            .filter_map(|site| match site {
                DutAccessSite::Function(function)
                | DutAccessSite::ComponentLifecycle { function, .. }
                | DutAccessSite::TestbenchService { function, .. } => Some(*function),
                DutAccessSite::Clock(_) => None,
            })
            .collect();
        Self {
            sites,
            functions,
            digest,
            uses_probe,
        }
    }

    pub fn sites(&self) -> &BTreeSet<DutAccessSite> {
        &self.sites
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn functions(&self) -> &BTreeSet<FunctionId> {
        &self.functions
    }

    pub fn uses_probe(&self) -> bool {
        self.uses_probe
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommonSharedTypeKind {
    Record(RecordId),
    Scoreboard(ScoreboardId),
    Component(ComponentId),
    TransactorState(TransactorId),
    Covergroup(CovgroupId),
    Testbench(TestbenchId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonSharedTypePlan {
    kind: CommonSharedTypeKind,
    name: String,
}

impl CommonSharedTypePlan {
    pub fn kind(&self) -> CommonSharedTypeKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommonCallableKind {
    Run,
    Check,
    SamplerAuto,
    Helper,
    TestbenchMethod,
    ComponentMethod,
    TransactorMethod,
    Tseq { needs_context: bool },
    TestHook,
    TestbenchLifecycle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommonCallablePlan {
    function: FunctionId,
    name: String,
    kind: CommonCallableKind,
    owner: CallableOwner,
    placement: CallablePlacement,
    bus_bindings: Vec<ir::BusBindingSchema>,
    bus_adapter: Option<super::expr::TestbenchBusAdapterPlan>,
}

impl CommonCallablePlan {
    pub fn function(&self) -> FunctionId {
        self.function
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> CommonCallableKind {
        self.kind
    }

    pub fn owner(&self) -> CallableOwner {
        self.owner.clone()
    }

    pub fn placement(&self) -> &CallablePlacement {
        &self.placement
    }

    pub fn bus_bindings(&self) -> &[ir::BusBindingSchema] {
        &self.bus_bindings
    }
}

impl CommonCapsulePlan {
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn test_bodies(&self) -> &[CommonTestBodyPlan] {
        &self.test_bodies
    }

    pub fn artifact_index(&self) -> usize {
        self.artifact_index
    }

    pub fn dut_access(&self) -> &CommonDutAccessProfile {
        &self.dut_access
    }
}

/// A common-layout plan owns a frozen snapshot of the verified program.
/// Render entry points consume only this plan, so caller-side IR mutation
/// cannot invalidate its IDs, ownership decisions, or artifact profiles.
#[derive(Debug)]
pub struct CommonCppPlan {
    program: TbProgram,
    build_profile: String,
    interface_build_profile: String,
    interface_abi_inputs: Vec<String>,
    runtime_build_profile: String,
    runtime_cells: RuntimeCellPlan,
    randomize: Option<TbirRandomizeEmissionPlan>,
    extern_declarations: String,
    dut_access: DutAccessPlan,
    bus_access: ir::passes::bus_access::BusAccessPlan,
    artifact_plan: CommonArtifactPlan,
    dut_type: String,
    mt: bool,
    clock_topologies: Vec<CommonTestClockPlan>,
    vec_lane_widths: HashMap<String, u32>,
    shared_types: Vec<CommonSharedTypePlan>,
    callables: Vec<CommonCallablePlan>,
    shared_callables: Vec<CommonCallablePlan>,
    bus_adapters: Vec<super::expr::TestbenchBusAdapterPlan>,
    contextual_tseqs: BTreeSet<FunctionId>,
    runtime_dut_access: CommonDutAccessProfile,
    capsules: Vec<CommonCapsulePlan>,
    placement: common_artifacts::PlacementMetrics,
}

/// A closed rendering and publication identity derived from one common plan.
///
/// The ABI anchor, build profile, registry, rendered units, and manifest are
/// all derived through this snapshot, so callers cannot accidentally publish
/// artifacts assembled from independent identities.
pub struct CommonCppPublication<'a> {
    plan: &'a CommonCppPlan,
    anchor: AbiAnchor,
    interface: String,
}

impl CommonCppPlan {
    fn program(&self) -> &TbProgram {
        &self.program
    }

    pub fn build_profile(&self) -> &str {
        &self.build_profile
    }

    pub fn artifact_plan(&self) -> &CommonArtifactPlan {
        &self.artifact_plan
    }

    pub fn runtime_cells(&self) -> &RuntimeCellPlan {
        &self.runtime_cells
    }

    pub fn dut_access(&self) -> &DutAccessPlan {
        &self.dut_access
    }

    pub fn bus_access(&self) -> &ir::passes::bus_access::BusAccessPlan {
        &self.bus_access
    }

    pub fn dut_type(&self) -> &str {
        &self.dut_type
    }

    pub fn mt(&self) -> bool {
        self.mt
    }

    pub fn clock_topologies(&self) -> &[CommonTestClockPlan] {
        &self.clock_topologies
    }

    pub fn shared_types(&self) -> &[CommonSharedTypePlan] {
        &self.shared_types
    }

    pub fn shared_callables(&self) -> &[CommonCallablePlan] {
        &self.shared_callables
    }

    pub fn callables(&self) -> &[CommonCallablePlan] {
        &self.callables
    }

    pub fn capsules(&self) -> &[CommonCapsulePlan] {
        &self.capsules
    }

    pub fn placement(&self) -> &common_artifacts::PlacementMetrics {
        &self.placement
    }

    pub fn runtime_dut_access(&self) -> &CommonDutAccessProfile {
        &self.runtime_dut_access
    }

    fn randomize_snippets(&self) -> &[String] {
        self.randomize
            .as_ref()
            .map_or(&[], |randomize| randomize.snippets.as_slice())
    }

    pub fn publication(&self) -> Result<CommonCppPublication<'_>, EmitError> {
        let interface_template = render_common_interface_template(self)?;
        let anchor = AbiAnchor::from_marked_interface_with_identity(
            &interface_template,
            common_artifacts::CodegenBackend::Tbir,
            common_artifacts::CppLayout::Common,
            &self.interface_abi_inputs,
        )
        .map_err(|error| EmitError(error.to_string()))?;
        let interface = anchor
            .bind_declarations(&interface_template)
            .map_err(|error| EmitError(error.to_string()))?;
        Ok(CommonCppPublication {
            plan: self,
            anchor,
            interface,
        })
    }

    fn clock_topology(&self, test_index: usize) -> Result<&CommonTestClockPlan, EmitError> {
        self.clock_topologies
            .iter()
            .find(|topology| topology.test_index == test_index)
            .ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout has no clock topology for test index {test_index}"
                ))
            })
    }
}

impl CommonCppPublication<'_> {
    pub fn plan(&self) -> &CommonCppPlan {
        self.plan
    }

    pub fn interface(&self) -> &str {
        &self.interface
    }

    pub fn interface_abi(&self) -> &str {
        self.anchor.digest()
    }

    pub fn abi_symbol(&self) -> &str {
        self.anchor.symbol()
    }

    pub fn runtime(&self) -> Result<String, EmitError> {
        self.anchor
            .bind_definition(&render_common_runtime_template(self.plan)?)
            .map_err(|error| EmitError(error.to_string()))
    }

    pub fn capsule(&self, capsule: &CommonCapsulePlan) -> Result<String, EmitError> {
        let owned = self
            .plan
            .capsules
            .get(capsule.index)
            .is_some_and(|owned| std::ptr::eq(owned, capsule));
        if !owned {
            return Err(EmitError(
                "tbir: common capsule handle does not belong to this publication plan".to_string(),
            ));
        }
        self.capsule_at(capsule.index)
    }

    fn capsule_at(&self, capsule_index: usize) -> Result<String, EmitError> {
        self.anchor
            .bind_declarations(&render_common_capsule_template(self.plan, capsule_index)?)
            .map_err(|error| EmitError(error.to_string()))
    }

    pub fn registry(&self) -> String {
        common_artifacts::render_registry_with_required_abi(
            &self.plan.artifact_plan,
            &self.plan.build_profile,
            &self.anchor,
        )
    }

    pub fn begin_publication<'a>(
        &'a self,
        outdir: &'a Path,
    ) -> Result<common_artifacts::Publication<'a>, common_artifacts::ArtifactError> {
        let identity = common_artifacts::ManifestIdentity::new(
            common_artifacts::CodegenBackend::Tbir,
            common_artifacts::CppLayout::Common,
            self.anchor.digest(),
            &self.plan.build_profile,
            self.plan.placement.clone(),
        );
        self.plan
            .artifact_plan
            .begin_publication_v2(outdir, &identity)
    }
}

pub fn plan_common_tests(
    prog: &TbProgram,
    opts: &EmitOpts,
    file_prefix: &str,
) -> Result<CommonCppPlan, EmitError> {
    plan_common_tests_impl(prog, opts, file_prefix, None, String::new())
}

pub fn plan_common_tests_with_source(
    prog: &TbProgram,
    file: &SourceFile,
    opts: &EmitOpts,
    file_prefix: &str,
) -> Result<CommonCppPlan, EmitError> {
    let randomize = if prog.constraint_sites.is_empty() {
        None
    } else {
        Some(crate::codegen::cpp_tb::plan_tbir_randomize_emission(
            file,
            opts,
            &prog.constraint_sites,
            4,
        )?)
    };
    let mut extern_declarations = String::new();
    crate::codegen::cpp_tb::emit_extern_fn_decls(&mut extern_declarations, file);
    plan_common_tests_impl(prog, opts, file_prefix, randomize, extern_declarations)
}

fn plan_common_tests_impl(
    prog: &TbProgram,
    opts: &EmitOpts,
    file_prefix: &str,
    randomize: Option<TbirRandomizeEmissionPlan>,
    extern_declarations: String,
) -> Result<CommonCppPlan, EmitError> {
    if opts.cosim.is_some() {
        return Err(unsupported("DPI co-simulation", "ticket 09"));
    }
    if prog.tests.is_empty() {
        return Err(EmitError("no `test` declaration found".into()));
    }

    preflight_regblock_placement(prog)?;
    if extern_declarations.is_empty() && program_has_extern_calls(prog) {
        return Err(EmitError(
            "TB-IR common layout requires source-backed external-function declarations".to_string(),
        ));
    }
    validate_program_tables(prog, randomize.is_some())?;

    let dut_interface = opts.dut_interface.as_ref().ok_or_else(|| {
        EmitError(
            "TB-IR common layout requires a resolved DUT interface catalog before planning"
                .to_string(),
        )
    })?;
    let dut_access = crate::ir::passes::dut_access::analyze(prog, dut_interface)
        .map_err(|error| EmitError(format!("TB-IR common layout {error}")))?;
    validate_dut_access_features(&dut_access)?;
    let vec_lane_widths = dut_access
        .interface()
        .ports()
        .iter()
        .filter_map(|port| {
            port.packed_lane_width()
                .map(|width| (port.name().to_string(), width))
        })
        .collect();

    let runtime_cells = crate::ir::passes::runtime_cells::analyze(prog)
        .map_err(|error| EmitError(format!("TB-IR common layout {error}")))?;
    let shared_types = plan_shared_types(prog)?;
    let (callables, shared_callables, contextual_tseqs) = plan_callables(prog)?;
    let bus_access = crate::ir::passes::bus_access::analyze(prog, dut_interface)
        .map_err(|error| EmitError(format!("TB-IR common layout {error}")))?;
    let bus_adapters = shared_callables
        .iter()
        .filter_map(|callable| callable.bus_adapter.clone())
        .collect::<Vec<_>>();
    let first_test = &prog.tests[0];
    let first_tb = prog.testbench(first_test.testbench);
    let dut_type = first_tb.dut_type.clone();
    let clock_topologies = prog
        .tests
        .iter()
        .enumerate()
        .map(|(test_index, test)| plan_test_clocks(test_index, test, &dut_access))
        .collect::<Result<Vec<_>, _>>()?;

    let mut artifact_profile_inputs = vec![
        "backend=tbir".to_string(),
        "layout=common".to_string(),
        format!("top={}", dut_access.dut_type()),
        format!(
            "dut_access={}",
            common_artifacts::stable_hash_hex(dut_access.abi_lines().join("\n").as_bytes())
        ),
    ];
    let randomize_profile = randomize.as_ref().map(|randomize| {
        common_artifacts::stable_hash_hex(full_randomize_profile(randomize).as_bytes())
    });
    if !extern_declarations.is_empty() {
        artifact_profile_inputs.push(format!(
            "extern_declarations={}",
            common_artifacts::stable_hash_hex(extern_declarations.as_bytes())
        ));
    }
    artifact_profile_inputs.extend(opts.build_profile_inputs.iter().cloned());
    let interface_build_profile =
        common_artifacts::build_profile_fingerprint(opts.mt, &artifact_profile_inputs);
    let mut runtime_profile_inputs = artifact_profile_inputs.clone();
    if let Some(randomize_profile) = &randomize_profile {
        runtime_profile_inputs.push(format!("randomize={randomize_profile}"));
    }
    runtime_profile_inputs.extend(callable_bus_profile_lines(&shared_callables));
    let runtime_build_profile =
        common_artifacts::build_profile_fingerprint(opts.mt, &runtime_profile_inputs);
    let build_profile = interface_build_profile.clone();

    let mut seen_functions = HashSet::new();
    for test in &prog.tests {
        let ir::TestSchema {
            id: _,
            name: _,
            testbench: _,
            run: _,
            check: _,
            clock_domain: _,
            clocks: _,
            cover_checks: _,
        } = test;
        let tb = prog.testbench(test.testbench);
        if tb.dut_type != dut_type {
            return Err(EmitError(format!(
                "TB-IR common layout requires one DUT type; test `{}` uses `{}` but the suite uses `{dut_type}`",
                test.name, tb.dut_type
            )));
        }
        validate_testbench(prog, test.name.as_str(), tb)?;
        validate_testbench_services(prog, opts, test, tb)?;
        for cover in &test.cover_checks {
            if prog.cover_checks.get(cover.index()).is_none() {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` references missing concurrent cover c{}",
                    test.name, cover.0
                )));
            }
        }
        if !seen_functions.insert(test.run) {
            return Err(EmitError(format!(
                "TB-IR common layout planner found run function fn{} owned by more than one test",
                test.run.0
            )));
        }
        validate_test_function(
            prog,
            opts,
            test,
            prog.function(test.run),
            ir::TestCallableMember::Run,
        )?;
        if let Some(check) = test.check {
            if !seen_functions.insert(check) {
                return Err(EmitError(format!(
                    "TB-IR common layout planner found check function fn{} owned by more than one test",
                    check.0
                )));
            }
            validate_test_function(
                prog,
                opts,
                test,
                prog.function(check),
                ir::TestCallableMember::Check,
            )?;
        }
    }

    let test_names: Vec<String> = prog.tests.iter().map(|test| test.name.clone()).collect();
    let mut artifact_plan = CommonArtifactPlan::new(file_prefix, &test_names)
        .map_err(|error| EmitError(error.to_string()))?;
    artifact_plan = artifact_plan
        .with_runtime_headers()
        .map_err(|error| EmitError(error.to_string()))?;
    if !dut_access.probes().is_empty() {
        artifact_plan = artifact_plan
            .with_probe_stub()
            .map_err(|error| EmitError(error.to_string()))?;
    }
    let runtime_dut_access = CommonDutAccessProfile::new(
        &dut_access,
        shared_callables
            .iter()
            .map(|callable| DutAccessSite::Function(callable.function))
            .chain(dut_access.clock_access_sites())
            .collect(),
    );
    let capsules = artifact_plan
        .capsules()
        .iter()
        .enumerate()
        .map(|(index, capsule)| {
            let test_bodies = capsule
                .test_indices()
                .iter()
                .map(|&test_index| {
                    let test = &prog.tests[test_index];
                    let mut test_hooks = prog
                        .functions
                        .iter()
                        .filter(|function| {
                            super::runtime::test_hook_belongs_to_test(function, test)
                        })
                        .map(|function| function.id)
                        .collect::<Vec<_>>();
                    test_hooks.sort();
                    let run_callback_captures =
                        ir::passes::runtime_cells::persistent_callback_captures(
                            prog,
                            prog.function(test.run),
                        )
                        .map_err(|error| EmitError(format!("tbir: {error}")))?
                        .into_iter()
                        .collect();
                    let check_callback_captures = match test.check {
                        Some(check) => ir::passes::runtime_cells::persistent_callback_captures(
                            prog,
                            prog.function(check),
                        )
                        .map_err(|error| EmitError(format!("tbir: {error}")))?
                        .into_iter()
                        .collect(),
                        None => HashSet::new(),
                    };
                    Ok(CommonTestBodyPlan {
                        test_index,
                        run: test.run,
                        check: test.check,
                        test_hooks,
                        run_callback_captures,
                        check_callback_captures,
                        placement_reason: CapsulePlacementReason::TestBody,
                    })
                })
                .collect::<Result<Vec<_>, EmitError>>()?;
            let mut capsule = CommonCapsulePlan {
                index,
                test_bodies,
                artifact_index: capsule.artifact_index(),
                dut_access: CommonDutAccessProfile::new(&dut_access, BTreeSet::new()),
                build_profile: String::new(),
            };
            let sites = capsule_dut_access_sites(prog, &capsule)?;
            capsule.dut_access = CommonDutAccessProfile::new(&dut_access, sites);
            let mut capsule_profile_inputs = artifact_profile_inputs.clone();
            capsule_profile_inputs.push(format!("dut-access={}", capsule.dut_access.digest()));
            for body in &capsule.test_bodies {
                let test = &prog.tests[body.test_index];
                capsule_profile_inputs.extend(test_bus_profile_lines_for_test(prog, test));
                let topology = clock_topologies
                    .iter()
                    .find(|topology| topology.test_index == body.test_index)
                    .ok_or_else(|| {
                        EmitError(format!(
                            "TB-IR common layout has no clock topology for test index {}",
                            body.test_index
                        ))
                    })?;
                capsule_profile_inputs.extend(clock_profile_lines_for_test(test, topology));
            }
            if let Some(randomize) = &randomize {
                let mut sites = BTreeSet::new();
                for body in &capsule.test_bodies {
                    let functions = std::iter::once(body.run)
                        .chain(body.check)
                        .chain(body.test_hooks.iter().copied());
                    for function in functions {
                        for block in &prog.function(function).blocks {
                            if let Terminator::Randomize { constraints, .. } = block.terminator {
                                sites.insert(constraints);
                            }
                        }
                    }
                }
                let mut profile = String::new();
                for site in sites {
                    profile.push_str(&randomize_site_profile(randomize, site));
                }
                if !profile.is_empty() {
                    capsule_profile_inputs.push(format!(
                        "randomize={}",
                        common_artifacts::stable_hash_hex(profile.as_bytes())
                    ));
                }
            }
            capsule.build_profile =
                common_artifacts::build_profile_fingerprint(opts.mt, &capsule_profile_inputs);
            Ok(capsule)
        })
        .collect::<Result<Vec<_>, EmitError>>()?;
    let placement = placement_metrics(&callables)?;

    Ok(CommonCppPlan {
        program: prog.clone(),
        build_profile,
        interface_build_profile,
        interface_abi_inputs: crate::codegen::cpp_tb::complete_common_abi_inputs(
            &opts.common_abi_inputs,
        ),
        runtime_build_profile,
        runtime_cells,
        randomize,
        extern_declarations,
        dut_access,
        bus_access,
        artifact_plan,
        dut_type,
        mt: opts.mt,
        clock_topologies,
        vec_lane_widths,
        shared_types,
        callables,
        shared_callables,
        bus_adapters,
        contextual_tseqs,
        runtime_dut_access,
        capsules,
        placement,
    })
}

fn placement_metrics(
    callables: &[CommonCallablePlan],
) -> Result<common_artifacts::PlacementMetrics, EmitError> {
    let mut common_callables = 0usize;
    let mut capsule_reasons = BTreeMap::<String, usize>::new();
    for callable in callables {
        match callable.placement() {
            CallablePlacement::Common => common_callables += 1,
            CallablePlacement::CapsuleLocal { reason, .. }
            | CallablePlacement::CapsuleScoped { reason } => {
                *capsule_reasons
                    .entry(capsule_placement_reason_name(*reason).to_string())
                    .or_default() += 1;
            }
            CallablePlacement::Invalid { reason } => {
                return Err(EmitError(format!(
                    "TB-IR common layout retained invalid callable placement: {reason:?}"
                )));
            }
        }
    }
    let capsule_callables = capsule_reasons.values().sum();
    common_artifacts::PlacementMetrics::new(common_callables, capsule_callables, capsule_reasons)
        .map_err(|error| EmitError(error.to_string()))
}

fn capsule_placement_reason_name(reason: CapsulePlacementReason) -> &'static str {
    match reason {
        CapsulePlacementReason::TestBody => "test_body",
        CapsulePlacementReason::TestHook => "test_hook",
        CapsulePlacementReason::TargetResponder => "target_responder",
        CapsulePlacementReason::ConcreteBusBinding { .. } => "concrete_bus_binding",
        CapsulePlacementReason::LifecycleService => "lifecycle_service",
        CapsulePlacementReason::Dependency { .. } => "dependency",
    }
}

fn validate_dut_access_features(plan: &DutAccessPlan) -> Result<(), EmitError> {
    for access in plan.accesses() {
        let ty = access.value_type().ok_or_else(|| {
            EmitError(format!(
                "TB-IR DUT access `{}` has no resolved scalar type",
                access.path().join(".")
            ))
        })?;
        if access
            .lane_shapes()
            .contains(&crate::ir::passes::dut_access::DutLaneShape::None)
        {
            validate_common_scalar_type(ty).map_err(|feature| {
                EmitError(format!(
                    "TB-IR DUT access `{}` has {feature}",
                    access.path().join(".")
                ))
            })?;
        }
        if access
            .lane_shapes()
            .iter()
            .any(|shape| !matches!(shape, crate::ir::passes::dut_access::DutLaneShape::None))
        {
            let lane_type = access.lane_value_type().ok_or_else(|| {
                EmitError(format!(
                    "TB-IR DUT access `{}` has no resolved lane scalar type",
                    access.path().join(".")
                ))
            })?;
            validate_common_scalar_type(&lane_type).map_err(|feature| {
                EmitError(format!(
                    "TB-IR DUT access `{}` lane has {feature}",
                    access.path().join(".")
                ))
            })?;
        }
    }
    Ok(())
}

fn program_has_extern_calls(prog: &TbProgram) -> bool {
    prog.functions.iter().any(|function| {
        let mut found = false;
        func::for_each_function_expr(function, |expr| {
            found |= matches!(expr, Expr::Call(ir::CallTarget::ExternFn { .. }, _));
        });
        found
    })
}

fn preflight_regblock_placement(prog: &TbProgram) -> Result<(), EmitError> {
    if !prog
        .testbenches
        .iter()
        .any(|schema| !schema.regblock_bindings.is_empty())
    {
        return Ok(());
    }
    let catalog = crate::ir::passes::callable_placement::analyze(prog)
        .map_err(|error| EmitError(format!("TB-IR common layout {error}")))?;
    if let Some(entry) = catalog.callables().iter().find(|entry| {
        matches!(
            entry.placement,
            CallablePlacement::Invalid {
                reason: InvalidPlacementReason::RegblockState { .. }
            }
        )
    }) {
        let CallablePlacement::Invalid { reason } = &entry.placement else {
            unreachable!("filtered to invalid register-block placement")
        };
        return Err(invalid_callable_placement(
            entry.function,
            &entry.name,
            reason,
        ));
    }
    Ok(())
}

fn validate_program_tables(prog: &TbProgram, has_randomize_plan: bool) -> Result<(), EmitError> {
    // Keep this pattern exhaustive so a new program-level IR surface cannot
    // enter common layout until its ownership is classified deliberately.
    let TbProgram {
        functions: _,
        testbench_types: _,
        testbenches: _,
        tests: _,
        probes: _,
        covgroups: _,
        records: _,
        transactors: _,
        scoreboards: _,
        regblocks: _,
        components: _,
        constraint_sites,
        property_checks: _,
        cover_checks: _,
        cycle_handlers: _,
    } = prog;
    let unsupported_tables = [(
        !constraint_sites.is_empty() && !has_randomize_plan,
        "randomization constraints without source-backed common planning",
        "source-backed randomization plan required",
    )];
    for (present, feature, ticket) in unsupported_tables {
        if present {
            return Err(unsupported(feature, ticket));
        }
    }
    Ok(())
}

fn plan_shared_types(prog: &TbProgram) -> Result<Vec<CommonSharedTypePlan>, EmitError> {
    let mut nodes = Vec::new();
    let mut testbench_aliases = Vec::new();
    for (index, schema) in prog.records.iter().enumerate() {
        nodes.push(CommonSharedTypePlan {
            kind: CommonSharedTypeKind::Record(RecordId(index as u32)),
            name: schema.name.clone(),
        });
    }
    for (index, schema) in prog.scoreboards.iter().enumerate() {
        nodes.push(CommonSharedTypePlan {
            kind: CommonSharedTypeKind::Scoreboard(ScoreboardId(index as u32)),
            name: schema.name.clone(),
        });
    }
    for (index, schema) in prog.components.iter().enumerate() {
        nodes.push(CommonSharedTypePlan {
            kind: CommonSharedTypeKind::Component(ComponentId(index as u32)),
            name: schema.name.clone(),
        });
    }
    for index in 0..prog.transactors.len() {
        let transactor = TransactorId(index as u32);
        nodes.push(CommonSharedTypePlan {
            kind: CommonSharedTypeKind::TransactorState(transactor),
            name: super::runtime::common_unbound_state_struct_ty(prog, transactor),
        });
    }
    for (index, schema) in prog.covgroups.iter().enumerate() {
        nodes.push(CommonSharedTypePlan {
            kind: CommonSharedTypeKind::Covergroup(CovgroupId(index as u32)),
            name: schema.name.clone(),
        });
    }
    let mut testbench_by_name = HashMap::<String, (TestbenchId, usize)>::new();
    for (index, schema) in prog.testbenches.iter().enumerate() {
        if super::needs_tb_struct(schema) {
            let id = TestbenchId(index as u32);
            if let Some(&(canonical, node_index)) = testbench_by_name.get(&schema.name) {
                let other = &prog.testbenches[canonical.index()];
                if schema.dut_type != other.dut_type
                    || schema.cov_fields != other.cov_fields
                    || schema.state_fields != other.state_fields
                    || schema.record_fields != other.record_fields
                    || schema.scoreboard_fields != other.scoreboard_fields
                {
                    return Err(EmitError(format!(
                        "TB-IR common layout testbench type `{}` has inconsistent state schemas across implementations",
                        schema.name
                    )));
                }
                testbench_aliases.push((id, node_index));
            } else {
                let node_index = nodes.len();
                nodes.push(CommonSharedTypePlan {
                    kind: CommonSharedTypeKind::Testbench(id),
                    name: schema.name.clone(),
                });
                testbench_by_name.insert(schema.name.clone(), (id, node_index));
            }
        }
    }

    let mut by_kind = HashMap::new();
    let mut by_name = HashMap::new();
    for (index, node) in nodes.iter().enumerate() {
        if by_kind.insert(node.kind, index).is_some() {
            return Err(EmitError(format!(
                "TB-IR common layout planner found duplicate shared type plan entry for `{}`",
                node.name
            )));
        }
        if let Some(previous) = by_name.insert(node.name.clone(), node.kind) {
            return Err(EmitError(format!(
                "TB-IR common layout planner found C++ type-name collision `{}` between {previous:?} and {:?}",
                node.name, node.kind
            )));
        }
    }
    for (alias, node_index) in testbench_aliases {
        by_kind.insert(CommonSharedTypeKind::Testbench(alias), node_index);
    }

    let mut deps = vec![Vec::new(); nodes.len()];

    for (index, node) in nodes.iter().enumerate() {
        match node.kind {
            CommonSharedTypeKind::Record(record) => {
                for field in &prog.records[record.index()].fields {
                    push_ir_type_dependency(
                        index,
                        &field.ty,
                        &format!("field `{}`", field.name),
                        &nodes,
                        &by_kind,
                        &mut deps,
                    )?;
                }
            }
            CommonSharedTypeKind::Scoreboard(scoreboard) => {
                for field in &prog.scoreboards[scoreboard.index()].fields {
                    match &field.kind {
                        ir::ScoreboardFieldKind::Scalar { ty, .. }
                        | ir::ScoreboardFieldKind::List { elem: ty, .. } => {
                            push_ir_type_dependency(
                                index,
                                ty,
                                &format!("field `{}`", field.name),
                                &nodes,
                                &by_kind,
                                &mut deps,
                            )?;
                        }
                        ir::ScoreboardFieldKind::Record { record } => push_type_dependency(
                            index,
                            CommonSharedTypeKind::Record(*record),
                            &format!("record r{} for field `{}`", record.0, field.name),
                            &nodes,
                            &by_kind,
                            &mut deps,
                        )?,
                        ir::ScoreboardFieldKind::Queue { elem } => {
                            if let ir::QueueElem::Record(record) = elem {
                                push_type_dependency(
                                    index,
                                    CommonSharedTypeKind::Record(*record),
                                    &format!("record r{} for field `{}`", record.0, field.name),
                                    &nodes,
                                    &by_kind,
                                    &mut deps,
                                )?;
                            }
                        }
                    }
                }
            }
            CommonSharedTypeKind::Component(component) => {
                for field in &prog.components[component.index()].fields {
                    match &field.kind {
                        ir::ComponentFieldKind::Scalar { ty, .. } => {
                            push_ir_type_dependency(
                                index,
                                ty,
                                &format!("field `{}`", field.name),
                                &nodes,
                                &by_kind,
                                &mut deps,
                            )?;
                        }
                        ir::ComponentFieldKind::FixedVec(vector) => {
                            push_ir_type_dependency(
                                index,
                                &vector.elem,
                                &format!("field `{}`", field.name),
                                &nodes,
                                &by_kind,
                                &mut deps,
                            )?;
                        }
                        ir::ComponentFieldKind::Record { record } => push_type_dependency(
                            index,
                            CommonSharedTypeKind::Record(*record),
                            &format!("record r{} for field `{}`", record.0, field.name),
                            &nodes,
                            &by_kind,
                            &mut deps,
                        )?,
                        ir::ComponentFieldKind::Queue { elem } => {
                            if let ir::QueueElem::Record(record) = elem {
                                push_type_dependency(
                                    index,
                                    CommonSharedTypeKind::Record(*record),
                                    &format!("record r{} for field `{}`", record.0, field.name),
                                    &nodes,
                                    &by_kind,
                                    &mut deps,
                                )?;
                            }
                        }
                        ir::ComponentFieldKind::Event {
                            payload: ir::EventPayload::Record(record),
                        } => push_type_dependency(
                            index,
                            CommonSharedTypeKind::Record(*record),
                            &format!("record r{} for field `{}`", record.0, field.name),
                            &nodes,
                            &by_kind,
                            &mut deps,
                        )?,
                        ir::ComponentFieldKind::Sub { component, .. } => push_type_dependency(
                            index,
                            CommonSharedTypeKind::Component(*component),
                            &format!("component c{} for field `{}`", component.0, field.name),
                            &nodes,
                            &by_kind,
                            &mut deps,
                        )?,
                        ir::ComponentFieldKind::ScoreboardSub { scoreboard } => {
                            push_type_dependency(
                                index,
                                CommonSharedTypeKind::Scoreboard(*scoreboard),
                                &format!(
                                    "scoreboard sb{} for field `{}`",
                                    scoreboard.0, field.name
                                ),
                                &nodes,
                                &by_kind,
                                &mut deps,
                            )?
                        }
                        ir::ComponentFieldKind::Event { .. }
                        | ir::ComponentFieldKind::Dut { .. } => {}
                    }
                }
            }
            CommonSharedTypeKind::TransactorState(transactor) => {
                for field in &prog.transactors[transactor.index()].state_fields {
                    match &field.kind {
                        ir::StateFieldKind::Scalar { ty, .. } => {
                            push_ir_type_dependency(
                                index,
                                ty,
                                &format!("field `{}`", field.name),
                                &nodes,
                                &by_kind,
                                &mut deps,
                            )?;
                        }
                        ir::StateFieldKind::Queue {
                            elem: ir::QueueElem::Record(record),
                        }
                        | ir::StateFieldKind::Record { record } => push_type_dependency(
                            index,
                            CommonSharedTypeKind::Record(*record),
                            &format!("record r{} for field `{}`", record.0, field.name),
                            &nodes,
                            &by_kind,
                            &mut deps,
                        )?,
                        ir::StateFieldKind::FixedVec { ty } => {
                            push_ir_type_dependency(
                                index,
                                ty,
                                &format!("field `{}`", field.name),
                                &nodes,
                                &by_kind,
                                &mut deps,
                            )?;
                        }
                        ir::StateFieldKind::Queue { .. } => {}
                    }
                }
            }
            CommonSharedTypeKind::Covergroup(_) => {}
            CommonSharedTypeKind::Testbench(testbench) => {
                let schema = &prog.testbenches[testbench.index()];
                for (_, record) in &schema.record_fields {
                    push_type_dependency(
                        index,
                        CommonSharedTypeKind::Record(*record),
                        &format!("record r{}", record.0),
                        &nodes,
                        &by_kind,
                        &mut deps,
                    )?;
                }
                for (_, scoreboard) in &schema.scoreboard_fields {
                    push_type_dependency(
                        index,
                        CommonSharedTypeKind::Scoreboard(*scoreboard),
                        &format!("scoreboard sb{}", scoreboard.0),
                        &nodes,
                        &by_kind,
                        &mut deps,
                    )?;
                }
                for (_, covgroup) in &schema.cov_fields {
                    push_type_dependency(
                        index,
                        CommonSharedTypeKind::Covergroup(*covgroup),
                        &format!("covergroup cg{}", covgroup.0),
                        &nodes,
                        &by_kind,
                        &mut deps,
                    )?;
                }
                for field in &schema.state_fields {
                    if let ir::TbStateFieldSchema::Queue(field) = field {
                        if let ir::QueueElem::Record(record) = field.elem {
                            push_type_dependency(
                                index,
                                CommonSharedTypeKind::Record(record),
                                &format!("record r{} for field `{}`", record.0, field.name),
                                &nodes,
                                &by_kind,
                                &mut deps,
                            )?;
                        }
                    }
                }
            }
        }
    }
    for edges in &mut deps {
        edges.sort_unstable();
        edges.dedup();
    }

    fn visit(
        index: usize,
        nodes: &[CommonSharedTypePlan],
        deps: &[Vec<usize>],
        state: &mut [u8],
        stack: &mut Vec<usize>,
        order: &mut Vec<usize>,
    ) -> Result<(), EmitError> {
        match state[index] {
            2 => return Ok(()),
            1 => {
                let start = stack.iter().position(|node| *node == index).unwrap_or(0);
                let mut names: Vec<&str> = stack[start..]
                    .iter()
                    .map(|node| nodes[*node].name.as_str())
                    .collect();
                names.push(nodes[index].name.as_str());
                return Err(EmitError(format!(
                    "TB-IR common layout shared type dependency cycle: {}",
                    names.join(" -> ")
                )));
            }
            _ => {}
        }
        state[index] = 1;
        stack.push(index);
        for &dependency in &deps[index] {
            visit(dependency, nodes, deps, state, stack, order)?;
        }
        stack.pop();
        state[index] = 2;
        order.push(index);
        Ok(())
    }

    let mut state = vec![0; nodes.len()];
    let mut order = Vec::with_capacity(nodes.len());
    for index in 0..nodes.len() {
        visit(
            index,
            &nodes,
            &deps,
            &mut state,
            &mut Vec::new(),
            &mut order,
        )?;
    }
    Ok(order
        .into_iter()
        .map(|index| nodes[index].clone())
        .collect())
}

fn push_type_dependency(
    from: usize,
    dependency: CommonSharedTypeKind,
    detail: &str,
    nodes: &[CommonSharedTypePlan],
    by_kind: &HashMap<CommonSharedTypeKind, usize>,
    deps: &mut [Vec<usize>],
) -> Result<(), EmitError> {
    let Some(&to) = by_kind.get(&dependency) else {
        return Err(EmitError(format!(
            "TB-IR common layout shared type `{}` references missing {detail}",
            nodes[from].name
        )));
    };
    deps[from].push(to);
    Ok(())
}

fn push_ir_type_dependency(
    from: usize,
    ty: &IrType,
    detail: &str,
    nodes: &[CommonSharedTypePlan],
    by_kind: &HashMap<CommonSharedTypeKind, usize>,
    deps: &mut [Vec<usize>],
) -> Result<(), EmitError> {
    match ty {
        IrType::Record(record) | IrType::RecordSeq(record) => push_type_dependency(
            from,
            CommonSharedTypeKind::Record(*record),
            &format!("record r{} for {detail}", record.0),
            nodes,
            by_kind,
            deps,
        ),
        IrType::FixedVec { elem, .. } | IrType::Seq(elem) => {
            push_ir_type_dependency(from, elem, detail, nodes, by_kind, deps)
        }
        IrType::Component(component) => push_type_dependency(
            from,
            CommonSharedTypeKind::Component(*component),
            &format!("component c{} for {detail}", component.0),
            nodes,
            by_kind,
            deps,
        ),
        IrType::Event(ir::EventPayload::Record(record)) => push_type_dependency(
            from,
            CommonSharedTypeKind::Record(*record),
            &format!("record r{} for {detail}", record.0),
            nodes,
            by_kind,
            deps,
        ),
        IrType::Event(ir::EventPayload::FixedVec { elem, .. }) => {
            push_ir_type_dependency(from, elem, detail, nodes, by_kind, deps)
        }
        IrType::UInt(_)
        | IrType::SInt(_)
        | IrType::Bool
        | IrType::String
        | IrType::PortSnapshot
        | IrType::Event(ir::EventPayload::Scalar { .. })
        | IrType::Unknown => Ok(()),
    }
}

fn plan_callables(
    prog: &TbProgram,
) -> Result<
    (
        Vec<CommonCallablePlan>,
        Vec<CommonCallablePlan>,
        BTreeSet<FunctionId>,
    ),
    EmitError,
> {
    let catalog = crate::ir::passes::callable_placement::analyze(prog)
        .map_err(|error| EmitError(format!("TB-IR common layout {error}")))?;
    for index in 0..prog.testbench_types.len() {
        catalog
            .testbench_method_order(ir::TestbenchTypeId(index as u32))
            .map_err(|error| EmitError(format!("TB-IR common layout {error}")))?;
    }
    catalog
        .component_method_order()
        .map_err(|error| EmitError(format!("TB-IR common layout {error}")))?;
    let (legacy_shared, contextual_tseqs) = plan_shared_callables(prog)?;
    let legacy_order = legacy_shared
        .iter()
        .enumerate()
        .map(|(index, callable)| (callable.function, index))
        .collect::<HashMap<_, _>>();
    let legacy_by_function = legacy_shared
        .into_iter()
        .map(|callable| (callable.function, callable))
        .collect::<HashMap<_, _>>();
    if let Some(entry) = catalog.callables().iter().find(|entry| {
        matches!(
            entry.placement,
            CallablePlacement::Invalid {
                reason: InvalidPlacementReason::RegblockState { .. }
            }
        )
    }) {
        let CallablePlacement::Invalid { reason } = &entry.placement else {
            unreachable!("filtered to invalid register-block placement")
        };
        return Err(invalid_callable_placement(
            entry.function,
            &entry.name,
            reason,
        ));
    }
    let mut callables = Vec::with_capacity(catalog.callables().len());
    for entry in catalog.callables() {
        let function = prog.function(entry.function);
        let kind = match entry.kind {
            crate::ir::passes::callable_placement::CallableKind::Run => CommonCallableKind::Run,
            crate::ir::passes::callable_placement::CallableKind::Check => CommonCallableKind::Check,
            crate::ir::passes::callable_placement::CallableKind::SamplerAuto => {
                CommonCallableKind::SamplerAuto
            }
            crate::ir::passes::callable_placement::CallableKind::Helper => {
                CommonCallableKind::Helper
            }
            crate::ir::passes::callable_placement::CallableKind::TestbenchMethod => {
                CommonCallableKind::TestbenchMethod
            }
            crate::ir::passes::callable_placement::CallableKind::ComponentMethod => {
                CommonCallableKind::ComponentMethod
            }
            crate::ir::passes::callable_placement::CallableKind::TransactorMethod => {
                CommonCallableKind::TransactorMethod
            }
            crate::ir::passes::callable_placement::CallableKind::Tseq => CommonCallableKind::Tseq {
                needs_context: contextual_tseqs.contains(&entry.function),
            },
            crate::ir::passes::callable_placement::CallableKind::TestHook => {
                CommonCallableKind::TestHook
            }
            crate::ir::passes::callable_placement::CallableKind::TestbenchLifecycle => {
                CommonCallableKind::TestbenchLifecycle
            }
        };
        if let CallablePlacement::Invalid { reason } = &entry.placement {
            return Err(invalid_callable_placement(
                entry.function,
                &entry.name,
                reason,
            ));
        }
        if let CallablePlacement::CapsuleLocal { reason, .. } = &entry.placement {
            if !matches!(entry.owner, CallableOwner::Test { .. }) {
                return Err(EmitError(format!(
                    "TB-IR common layout callable fn{} `{}` requires capsule-local placement ({reason:?}) without a capsule-owned concrete bus adapter",
                    entry.function.0, entry.name
                )));
            }
        }
        if let CallablePlacement::CapsuleScoped { reason } = &entry.placement {
            let supported = matches!(
                entry.owner,
                CallableOwner::Transactor {
                    member: crate::ir::passes::callable_placement::TransactorCallableMember::TargetMethod(_),
                    ..
                }
            ) || matches!(
                (&entry.kind, &entry.owner),
                (
                    crate::ir::passes::callable_placement::CallableKind::TestbenchLifecycle,
                    CallableOwner::TestbenchType(_)
                )
            );
            if !supported {
                return Err(EmitError(format!(
                    "TB-IR common layout callable fn{} `{}` has unsupported capsule-scoped placement ({reason:?})",
                    entry.function.0, entry.name
                )));
            }
        }
        if entry.placement == CallablePlacement::Common {
            match kind {
                CommonCallableKind::Helper | CommonCallableKind::Tseq { .. } => {
                    if !legacy_by_function.contains_key(&entry.function) {
                        return Err(EmitError(format!(
                            "TB-IR common layout callable fn{} `{}` was not validated as a shared helper/tseq",
                            entry.function.0, entry.name
                        )));
                    }
                }
                CommonCallableKind::TestbenchMethod => {
                    validate_shared_testbench_method(prog, function)?;
                }
                CommonCallableKind::ComponentMethod => {
                    validate_shared_component_method(prog, function)?;
                }
                CommonCallableKind::TransactorMethod => {
                    validate_shared_transactor_method(prog, function)?;
                }
                _ => {
                    return Err(EmitError(format!(
                        "TB-IR common layout callable fn{} `{}` has unsupported common placement",
                        entry.function.0, entry.name
                    )));
                }
            }
        }
        let bus_bindings = plan_callable_bus_bindings(prog, function, &entry.owner)?;
        let callable = CommonCallablePlan {
            function: entry.function,
            name: entry.name.clone(),
            kind,
            owner: entry.owner.clone(),
            placement: entry.placement.clone(),
            bus_adapter: plan_callable_bus_adapter(function, &bus_bindings)?,
            bus_bindings,
        };
        callables.push(callable);
    }
    propagate_callable_bus_adapters(prog, &mut callables)?;
    let mut shared = callables
        .iter()
        .filter(|callable| callable.placement == CallablePlacement::Common)
        .cloned()
        .collect::<Vec<_>>();
    shared.sort_by_key(|callable| {
        legacy_order
            .get(&callable.function)
            .copied()
            .unwrap_or(legacy_order.len() + callable.function.index())
    });
    Ok((callables, shared, contextual_tseqs))
}

fn propagate_callable_bus_adapters(
    prog: &TbProgram,
    callables: &mut [CommonCallablePlan],
) -> Result<(), EmitError> {
    loop {
        let current = callables
            .iter()
            .map(|callable| {
                (
                    callable.function,
                    (
                        callable.placement.clone(),
                        callable.kind,
                        callable.bus_adapter.clone(),
                    ),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut changed = false;
        for callable in callables.iter_mut().filter(|callable| {
            callable.placement == CallablePlacement::Common
                && matches!(
                    callable.kind,
                    CommonCallableKind::TestbenchMethod
                        | CommonCallableKind::ComponentMethod
                        | CommonCallableKind::TransactorMethod
                )
        }) {
            let function = prog.function(callable.function);
            let mut signals = callable
                .bus_adapter
                .as_ref()
                .map(|adapter| {
                    adapter
                        .signals
                        .iter()
                        .map(|signal| {
                            (
                                (
                                    signal.field.clone(),
                                    signal.channel.clone(),
                                    signal.signal.clone(),
                                ),
                                signal.ty.clone(),
                            )
                        })
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            for callee in direct_adapter_callees(function) {
                let Some((callee_placement, callee_kind, adapter)) = current.get(&callee) else {
                    return Err(EmitError(format!(
                        "TB-IR common layout fn{} `{}` calls missing testbench fn{}",
                        function.id.0, function.name, callee.0
                    )));
                };
                let expected_kind = callable.kind;
                if *callee_placement != CallablePlacement::Common || *callee_kind != expected_kind {
                    return Err(EmitError(format!(
                        "TB-IR common layout fn{} `{}` calls fn{} without matching common placement",
                        function.id.0, function.name, callee.0
                    )));
                }
                let Some(adapter) = adapter else {
                    continue;
                };
                for signal in &adapter.signals {
                    insert_bus_adapter_signal(
                        function,
                        &mut signals,
                        &signal.field,
                        &signal.channel,
                        &signal.signal,
                        signal.ty.clone(),
                    )?;
                }
            }
            let next = bus_adapter_from_signals(function.id, signals);
            if callable.bus_adapter != next {
                callable.bus_adapter = next;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
    }
}

fn direct_adapter_callees(function: &TbFunction) -> BTreeSet<FunctionId> {
    function
        .blocks
        .iter()
        .flat_map(|block| &block.stmts)
        .filter_map(|stmt| match stmt {
            Stmt::TestbenchCall { function, .. } => Some(*function),
            Stmt::ComponentCall {
                base,
                component,
                function: callee,
                ..
            } if matches!(
                function.kind,
                FunctionKind::ComponentMethod {
                    component: owner,
                    ..
                } if owner == *component
            ) && (matches!(base, ir::ComponentBase::SelfField)
                || matches!(base, ir::ComponentBase::Path(path) if path.as_slice() == ["self"])) =>
            {
                Some(*callee)
            }
            Stmt::TransactorSelfCall {
                call: Expr::Call(ir::CallTarget::TransactorSelfMethod { function, .. }, _),
                ..
            } => Some(*function),
            _ => None,
        })
        .collect()
}

fn plan_callable_bus_bindings(
    prog: &TbProgram,
    function: &TbFunction,
    owner: &CallableOwner,
) -> Result<Vec<ir::BusBindingSchema>, EmitError> {
    let fields = crate::ir::passes::callable_placement::function_concrete_bus_fields(function);
    if matches!(
        owner,
        CallableOwner::Transactor {
            member: crate::ir::passes::callable_placement::TransactorCallableMember::TargetMethod(
                _
            ),
            ..
        }
    ) {
        // Target responders are capsule-scoped. Their downstream bus names
        // are resolved against the owning test's exact binding table by
        // `emit_target_actor`; they must not be mistaken for the transactor's
        // own initiator-side `bound_bus_instances` adapter.
        return Ok(Vec::new());
    }
    let bound_owner = match owner {
        CallableOwner::Transactor { transactor, .. } => {
            Some(ir::BoundBusOwner::Transactor(*transactor))
        }
        CallableOwner::Component { component, .. } => {
            Some(ir::BoundBusOwner::Component(*component))
        }
        _ => None,
    };
    if let Some(bound_owner) = bound_owner {
        if !crate::ir::passes::callable_placement::function_uses_bound_bus(function) {
            return Ok(Vec::new());
        }
        let mut canonical: Option<ir::BusBindingSchema> = None;
        for test in &prog.tests {
            let testbench = prog
                .testbenches
                .get(test.testbench.index())
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` references missing testbench tb{}",
                        test.name, test.testbench.0
                    ))
                })?;
            for instance in testbench
                .bound_bus_instances
                .iter()
                .filter(|instance| instance.owner == bound_owner)
            {
                let binding = testbench.bus_binding(instance.binding).ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` bound owner {bound_owner:?} references missing binding bb{}",
                        test.name, instance.binding.0
                    ))
                })?;
                if let Some(reference) = &canonical {
                    if !crate::ir::passes::callable_placement::same_bound_bus_binding_semantics(
                        reference, binding,
                    ) {
                        return Err(EmitError(format!(
                            "TB-IR common layout fn{} `{}` has non-identical bound-bus schemas `{}` and `{}`",
                            function.id.0, function.name, reference.bus, binding.bus
                        )));
                    }
                } else {
                    let mut logical = binding.clone();
                    logical.field = "bus".to_string();
                    logical.remap.clear();
                    canonical = Some(logical);
                }
            }
        }
        return canonical.map(|binding| vec![binding]).ok_or_else(|| {
            EmitError(format!(
                "TB-IR common layout fn{} `{}` has no explicit bound-bus instance",
                function.id.0, function.name
            ))
        });
    }
    if fields.is_empty() {
        return Ok(Vec::new());
    }
    let CallableOwner::TestbenchType(testbench_type) = owner else {
        return Ok(Vec::new());
    };
    let mut planned = Vec::with_capacity(fields.len());
    for field in fields {
        let mut canonical: Option<ir::BusBindingSchema> = None;
        let mut matched_impl = false;
        for test in &prog.tests {
            let Some(testbench) = prog.testbenches.get(test.testbench.index()) else {
                continue;
            };
            if testbench.type_id != *testbench_type {
                continue;
            }
            matched_impl = true;
            let binding = testbench
                .bus_bindings
                .iter()
                .find(|binding| binding.field == field)
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` has no explicit binding for logical bus field `{field}` required by fn{} `{}`",
                        test.name, function.id.0, function.name
                    ))
                })?;
            if let Some(reference) = &canonical {
                if !crate::ir::passes::callable_placement::same_bus_binding_semantics(
                    reference, binding,
                ) {
                    return Err(EmitError(format!(
                        "TB-IR common layout fn{} `{}` has non-identical adapters for logical bus field `{field}`",
                        function.id.0, function.name
                    )));
                }
            } else {
                canonical = Some(binding.clone());
            }
        }
        if !matched_impl {
            return Err(EmitError(format!(
                "TB-IR common layout fn{} `{}` has no implementation for reusable testbench type tbt{}",
                function.id.0, function.name, testbench_type.0
            )));
        }
        planned.push(canonical.expect("matched implementation has a checked binding"));
    }
    Ok(planned)
}

fn plan_callable_bus_adapter(
    function: &TbFunction,
    bindings: &[ir::BusBindingSchema],
) -> Result<Option<super::expr::TestbenchBusAdapterPlan>, EmitError> {
    if bindings.is_empty() {
        return Ok(None);
    }
    let mut signals = BTreeMap::<(String, String, String), IrType>::new();
    for block in &function.blocks {
        for stmt in &block.stmts {
            match stmt {
                Stmt::DutWrite(port, _) | Stmt::DutRead(_, port) | Stmt::ProbeRelease(port) => {
                    add_bus_adapter_port(function, &mut signals, port)?;
                }
                Stmt::TlmFork(desc) => {
                    add_bus_adapter_tlm(
                        function,
                        bindings,
                        &mut signals,
                        &desc.bus_field,
                        &desc.method,
                        &desc.target,
                    )?;
                }
                Stmt::TlmJoinAll(pending) => {
                    for desc in pending {
                        add_bus_adapter_tlm(
                            function,
                            bindings,
                            &mut signals,
                            &desc.bus_field,
                            &desc.method,
                            &desc.target,
                        )?;
                    }
                }
                _ => {}
            }
            ir::visit::try_visit_stmt_exprs(stmt, &mut |expr| {
                ir::visit::try_walk_expr(expr, &mut |node| {
                    match node {
                        Expr::Port(port) | Expr::PortSnapshotLane { port, .. } => {
                            add_bus_adapter_port(function, &mut signals, port)?
                        }
                        Expr::Call(
                            ir::CallTarget::TransactorMethod {
                                bus_field,
                                method,
                                target,
                            },
                            _,
                        ) => add_bus_adapter_tlm(
                            function,
                            bindings,
                            &mut signals,
                            bus_field,
                            method,
                            target,
                        )?,
                        _ => {}
                    }
                    Ok(())
                })
            })?;
        }
        ir::visit::try_visit_terminator_exprs(&block.terminator, &mut |expr| {
            ir::visit::try_walk_expr(expr, &mut |node| {
                match node {
                    Expr::Port(port) | Expr::PortSnapshotLane { port, .. } => {
                        add_bus_adapter_port(function, &mut signals, port)?
                    }
                    Expr::Call(
                        ir::CallTarget::TransactorMethod {
                            bus_field,
                            method,
                            target,
                        },
                        _,
                    ) => add_bus_adapter_tlm(
                        function,
                        bindings,
                        &mut signals,
                        bus_field,
                        method,
                        target,
                    )?,
                    _ => {}
                }
                Ok(())
            })
        })?;
    }
    Ok(bus_adapter_from_signals(function.id, signals))
}

fn bus_adapter_from_signals(
    function: FunctionId,
    signals: BTreeMap<(String, String, String), IrType>,
) -> Option<super::expr::TestbenchBusAdapterPlan> {
    let signals = signals
        .into_iter()
        .enumerate()
        .map(
            |(index, ((field, channel, signal), ty))| super::expr::BusSignalAdapterPlan {
                field,
                channel,
                signal,
                ty,
                symbol: format!("_harc_bus_signal_{index}"),
            },
        )
        .collect::<Vec<_>>();
    (!signals.is_empty()).then_some(super::expr::TestbenchBusAdapterPlan { function, signals })
}

fn add_bus_adapter_port(
    function: &TbFunction,
    signals: &mut BTreeMap<(String, String, String), IrType>,
    port: &PortRef,
) -> Result<(), EmitError> {
    let field = match &port.origin {
        ir::PortOrigin::BusBinding { field, .. } => field.as_str(),
        ir::PortOrigin::BoundBus => "bus",
        ir::PortOrigin::Dut => return Ok(()),
    };
    let (channel, signal) = match port.port_path.as_slice() {
        [root, channel, signal] if root == field => (channel.clone(), signal.clone()),
        [root, signal] if root == field => (String::new(), signal.clone()),
        [signal] => (String::new(), signal.clone()),
        _ => {
            return Err(EmitError(format!(
                "TB-IR common layout fn{} `{}` has malformed logical bus path `{}`",
                function.id.0,
                function.name,
                port.port_path.join(".")
            )))
        }
    };
    let ty = port.value_type.clone().ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout fn{} `{}` has untyped logical bus signal `{field}.{}.{signal}`",
            function.id.0, function.name, channel
        ))
    })?;
    insert_bus_adapter_signal(function, signals, field, &channel, &signal, ty)
}

fn add_bus_adapter_tlm(
    function: &TbFunction,
    bindings: &[ir::BusBindingSchema],
    signals: &mut BTreeMap<(String, String, String), IrType>,
    bus_field: &str,
    method: &str,
    target: &ir::TransactorMethodTarget,
) -> Result<(), EmitError> {
    let (field, expected_bus) = match target {
        ir::TransactorMethodTarget::TestbenchBusField { field, bus, .. } => {
            (field.as_str(), Some(bus.as_str()))
        }
        ir::TransactorMethodTarget::BoundBus => ("bus", None),
        _ => return Ok(()),
    };
    if field != bus_field {
        return Err(EmitError(format!(
            "TB-IR common layout fn{} `{}` carries mismatched TLM bus fields `{bus_field}` and `{field}`",
            function.id.0, function.name
        )));
    }
    let binding = bindings
        .iter()
        .find(|binding| binding.field == field && expected_bus.is_none_or(|bus| binding.bus == bus))
        .ok_or_else(|| {
            EmitError(format!(
                "TB-IR common layout fn{} `{}` has no planned adapter for `{bus_field}.{method}`",
                function.id.0, function.name
            ))
        })?;
    let schema = binding
        .methods
        .iter()
        .find(|candidate| candidate.name == method)
        .ok_or_else(|| {
            EmitError(format!(
                "TB-IR common layout fn{} `{}` has no planned TLM schema for `{bus_field}.{method}`",
                function.id.0, function.name
            ))
        })?;
    for (name, ty) in schema.args.iter().zip(&schema.arg_types) {
        insert_bus_adapter_signal(function, signals, bus_field, method, name, ty.clone())?;
    }
    for (signal, ty) in [
        ("req_valid", IrType::Bool),
        ("req_ready", IrType::Bool),
        ("rsp_ready", IrType::Bool),
        ("rsp_valid", IrType::Bool),
    ] {
        insert_bus_adapter_signal(function, signals, bus_field, method, signal, ty)?;
    }
    if let Some(ret) = &schema.ret_type {
        insert_bus_adapter_signal(
            function,
            signals,
            bus_field,
            method,
            "rsp_data",
            ret.clone(),
        )?;
    }
    if let ir::TlmMethodMode::OutOfOrder { tags } = schema.mode {
        let width = (u64::BITS - tags.saturating_sub(1).leading_zeros()).max(1);
        let ty = IrType::UInt(Some(width));
        insert_bus_adapter_signal(function, signals, bus_field, method, "req_tag", ty.clone())?;
        insert_bus_adapter_signal(function, signals, bus_field, method, "rsp_tag", ty)?;
    }
    Ok(())
}

fn insert_bus_adapter_signal(
    function: &TbFunction,
    signals: &mut BTreeMap<(String, String, String), IrType>,
    field: &str,
    channel: &str,
    signal: &str,
    ty: IrType,
) -> Result<(), EmitError> {
    if !matches!(ty, IrType::Record(_)) {
        validate_common_scalar_type(&ty).map_err(|feature| {
            EmitError(format!(
                "TB-IR common layout fn{} `{}` bus signal `{field}.{}.{signal}` has {feature}",
                function.id.0, function.name, channel
            ))
        })?;
    }
    let key = (field.to_string(), channel.to_string(), signal.to_string());
    if let Some(existing) = signals.get(&key) {
        if existing != &ty {
            return Err(EmitError(format!(
                "TB-IR common layout fn{} `{}` bus signal `{field}.{}.{signal}` has conflicting types {existing:?} and {ty:?}",
                function.id.0, function.name, channel
            )));
        }
    } else {
        signals.insert(key, ty);
    }
    Ok(())
}

fn callable_bus_profile_lines(callables: &[CommonCallablePlan]) -> Vec<String> {
    let mut lines = Vec::new();
    for callable in callables {
        let Some(adapter) = &callable.bus_adapter else {
            continue;
        };
        lines.push(format!(
            "bus-callable:fn{}:{}",
            callable.function.0, callable.name
        ));
        for signal in &adapter.signals {
            lines.push(format!(
                "bus-signal:{}:{}:{}:{:?}",
                signal.field, signal.channel, signal.signal, signal.ty
            ));
        }
        for binding in &callable.bus_bindings {
            lines.push(format!("bus-binding:{}:{}", binding.field, binding.bus));
            for method in &binding.methods {
                lines.push(format!(
                    "bus-method:{}:{}:{}:{:?}:{:?}:{:?}",
                    binding.field,
                    method.name,
                    method.args.join(","),
                    method.arg_types,
                    method.ret_type,
                    method.mode
                ));
            }
        }
    }
    lines
}

fn randomize_site_profile(
    randomize: &TbirRandomizeEmissionPlan,
    site: crate::ir::ConstraintRef,
) -> String {
    let mut profile = String::new();
    if let Some(snippet) = randomize.snippets.get(site.index()) {
        profile.push_str(snippet);
    }
    if let Some(state) = randomize.site_states.get(site.index()) {
        if let Some(problem_id) = state.problem_id {
            if let Some(problem) = randomize
                .runtime_table
                .problems
                .iter()
                .find(|problem| problem.id == problem_id)
            {
                profile.push_str(&problem.manifest());
            }
        }
        for cell in &state.cells {
            profile.push_str(&cell.tag);
            profile.push(':');
            profile.push_str(&cell.ctype);
            profile.push(':');
            profile.push_str(cell.init.as_deref().unwrap_or(""));
            profile.push('\n');
        }
    }
    profile
}

fn full_randomize_profile(randomize: &TbirRandomizeEmissionPlan) -> String {
    let mut profile = randomize.runtime_table.manifest();
    for index in 0..randomize.snippets.len() {
        profile.push_str(&randomize_site_profile(
            randomize,
            crate::ir::ConstraintRef(index as u32),
        ));
    }
    profile
}

fn test_bus_profile_lines_for_test(prog: &TbProgram, test: &TestSchema) -> Vec<String> {
    let Some(testbench) = prog.testbenches.get(test.testbench.index()) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for binding in &testbench.bus_bindings {
        lines.push(format!(
            "test-bus:{}:{}:{}",
            test.name, binding.field, binding.bus
        ));
        for method in &binding.methods {
            lines.push(format!(
                "test-bus-method:{}:{}:{}:{}:{:?}:{:?}:{:?}",
                test.name,
                binding.field,
                method.name,
                method.args.join(","),
                method.arg_types,
                method.ret_type,
                method.mode
            ));
        }
        for ((channel, signal), port) in &binding.remap {
            lines.push(format!(
                "test-bus-remap:{}:{}:{channel}.{signal}={port}",
                test.name, binding.field
            ));
        }
    }
    lines
}

fn clock_profile_lines_for_test(test: &TestSchema, topology: &CommonTestClockPlan) -> Vec<String> {
    if topology.clocks.is_empty() {
        return vec![format!("test-clock:{}:clockless", test.name)];
    }
    topology
        .clocks
        .iter()
        .enumerate()
        .map(|(index, clock)| {
            format!(
                "test-clock:{}:{index}:{}:{}:{}",
                test.name,
                clock.name,
                clock.period_ps,
                clock.domain.as_deref().unwrap_or("")
            )
        })
        .collect()
}

fn invalid_callable_placement(
    function: FunctionId,
    name: &str,
    reason: &InvalidPlacementReason,
) -> EmitError {
    let ownership = match reason {
        InvalidPlacementReason::RegblockState {
            callback_bearing, ..
        } => {
            let callbacks = if *callback_bearing {
                " including callback-bearing mirror state"
            } else {
                ""
            };
            format!(
                "; register-block mirror state{callbacks} is test-binding-owned and cannot be placed in a reusable common callable"
            )
        }
        InvalidPlacementReason::UnsupportedLifecycleHandler => {
            "; the lifecycle callable has no supported ticket 06 owner".to_string()
        }
        InvalidPlacementReason::UnsupportedTransactorBody
        | InvalidPlacementReason::MissingConcreteBusBinding
        | InvalidPlacementReason::ConflictingBusBindings { .. } => {
            "; transactor and bus callables require explicit semantically compatible bindings"
                .to_string()
        }
        _ => String::new(),
    };
    EmitError(format!(
        "TB-IR common layout callable fn{} `{name}` has invalid placement: {reason:?}{ownership}",
        function.0
    ))
}

fn component_method_schema<'a>(
    prog: &'a TbProgram,
    function: &TbFunction,
    component: ComponentId,
) -> Result<&'a ir::ComponentMethodSchema, EmitError> {
    let FunctionKind::ComponentMethod { member, .. } = function.kind else {
        return Err(unsupported_unplaced_function(prog, function));
    };
    prog.components
        .get(component.index())
        .and_then(|schema| schema.methods.get(member.index()))
        .filter(|method| method.function == function.id)
        .ok_or_else(|| unsupported_unplaced_function(prog, function))
}

fn transactor_method_schema<'a>(
    prog: &'a TbProgram,
    function: &TbFunction,
    transactor: TransactorId,
) -> Result<&'a ir::TransactorMethodSchema, EmitError> {
    let FunctionKind::TransactorBody { member, .. } = function.kind else {
        return Err(unsupported_unplaced_function(prog, function));
    };
    prog.transactors
        .get(transactor.index())
        .and_then(|schema| schema.methods.get(member.index()))
        .filter(|method| method.function == function.id)
        .ok_or_else(|| unsupported_unplaced_function(prog, function))
}

fn component_callable_symbol(
    prog: &TbProgram,
    component: ComponentId,
    member: ComponentCallableMember,
) -> Result<String, EmitError> {
    let schema = prog.components.get(component.index()).ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout references missing component c{}",
            component.0
        ))
    })?;
    match member {
        ComponentCallableMember::Method(index) => schema
            .methods
            .get(index)
            .map(|method| format!("{}_{}", schema.name, method.name)),
        ComponentCallableMember::OnHandler(index) => schema
            .on_handlers
            .get(index)
            .map(|handler| func::on_handler_lambda_name(schema, handler)),
        ComponentCallableMember::PeriodicHandler(index) => schema
            .periodic_handlers
            .get(index)
            .map(|handler| func::periodic_handler_lambda_name(schema, handler)),
        ComponentCallableMember::CycleHandler(index) => schema
            .cycle_handlers
            .get(index)
            .map(|handler| func::cycle_handler_lambda_name(schema, handler)),
        ComponentCallableMember::Watchdog => schema
            .watchdog
            .as_ref()
            .map(|handler| func::watchdog_lambda_name(schema, handler)),
    }
    .ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout component `{}` has no callable member {member:?}",
            schema.name
        ))
    })
}

fn plan_shared_callables(
    prog: &TbProgram,
) -> Result<(Vec<CommonCallablePlan>, BTreeSet<FunctionId>), EmitError> {
    let mut callables = Vec::new();
    for function in &prog.functions {
        if function.kind == FunctionKind::Helper {
            validate_shared_helper(prog, function)?;
            callables.push(CommonCallablePlan {
                function: function.id,
                name: function.name.clone(),
                kind: CommonCallableKind::Helper,
                owner: CallableOwner::Suite,
                placement: CallablePlacement::Common,
                bus_bindings: Vec::new(),
                bus_adapter: None,
            });
        }
    }

    let mut dependencies = HashMap::<FunctionId, BTreeSet<FunctionId>>::new();
    for function_id in func::tseq_emit_order(prog)? {
        let function = prog.function(function_id);
        validate_shared_tseq(prog, function)?;
        let calls = func::tseq_dependencies(function);
        dependencies.insert(function.id, calls);
        callables.push(CommonCallablePlan {
            function: function.id,
            name: function.name.clone(),
            kind: CommonCallableKind::Tseq {
                needs_context: tseq_directly_uses_context(function),
            },
            owner: CallableOwner::Suite,
            placement: CallablePlacement::Common,
            bus_bindings: Vec::new(),
            bus_adapter: None,
        });
    }

    let mut contextual: BTreeSet<FunctionId> = callables
        .iter()
        .filter_map(|callable| match callable.kind {
            CommonCallableKind::Tseq {
                needs_context: true,
            } => Some(callable.function),
            _ => None,
        })
        .collect();
    loop {
        let mut changed = false;
        for (function, calls) in &dependencies {
            if !contextual.contains(function) && calls.iter().any(|call| contextual.contains(call))
            {
                contextual.insert(*function);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for callable in &mut callables {
        if matches!(callable.kind, CommonCallableKind::Tseq { .. }) {
            callable.kind = CommonCallableKind::Tseq {
                needs_context: contextual.contains(&callable.function),
            };
        }
    }
    Ok((callables, contextual))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SharedCallableSurface {
    Helper,
    Tseq,
    TestbenchMethod,
    ComponentMethod,
    TransactorMethod,
}

fn validate_shared_testbench_method(
    prog: &TbProgram,
    function: &TbFunction,
) -> Result<(), EmitError> {
    let FunctionKind::TestbenchMethod { testbench, .. } = function.kind else {
        return Err(EmitError(format!(
            "TB-IR common layout planner expected `{}` to be a testbench method",
            function.name
        )));
    };
    if function.owner.is_some() {
        return Err(EmitError(format!(
            "TB-IR common layout testbench method `{}` unexpectedly has a per-test owner",
            function.name
        )));
    }
    let schema = prog.testbench_types.get(testbench.index()).ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout testbench method `{}` references missing type tbt{}",
            function.name, testbench.0
        ))
    })?;
    let claims = schema
        .methods
        .iter()
        .filter(|method| method.function == function.id)
        .collect::<Vec<_>>();
    if claims.len() != 1 {
        return Err(EmitError(format!(
            "TB-IR common layout testbench method `{}` must have exactly one type owner; found {}",
            function.name,
            claims.len()
        )));
    }
    for module in claims[0].module_param_types.iter().flatten() {
        let mismatched = prog
            .testbenches
            .iter()
            .filter(|instance| instance.type_id == testbench)
            .find(|instance| instance.dut_type != *module);
        if let Some(instance) = mismatched {
            return Err(EmitError(format!(
                "TB-IR common layout testbench method `{}` has module parameter `{module}`, but implementation `{}` owns DUT `{}`",
                function.name, instance.name, instance.dut_type
            )));
        }
    }
    validate_shared_callable(prog, function, SharedCallableSurface::TestbenchMethod)
}

fn validate_shared_helper(prog: &TbProgram, function: &TbFunction) -> Result<(), EmitError> {
    if function.owner.is_some() {
        return Err(EmitError(format!(
            "TB-IR common layout helper `{}` unexpectedly has a testbench owner",
            function.name
        )));
    }
    validate_shared_callable(prog, function, SharedCallableSurface::Helper)
}

fn validate_shared_tseq(prog: &TbProgram, function: &TbFunction) -> Result<(), EmitError> {
    if function.owner.is_some() {
        return Err(EmitError(format!(
            "TB-IR common layout tseq `{}` unexpectedly has a testbench owner",
            function.name
        )));
    }
    let FunctionKind::Tseq { elem } = &function.kind else {
        return Err(EmitError(format!(
            "TB-IR common layout planner expected `{}` to be a tseq",
            function.name
        )));
    };
    let Some(ret) = function.ret else {
        return Err(EmitError(format!(
            "TB-IR common layout tseq `{}` has no return accumulator",
            function.name
        )));
    };
    let Some(local) = function.locals.get(ret.index()) else {
        return Err(EmitError(format!(
            "TB-IR common layout tseq `{}` return local %{} does not resolve",
            function.name, ret.0
        )));
    };
    if local.ty != elem.seq_type() {
        return Err(EmitError(format!(
            "TB-IR common layout tseq `{}` return type {:?} does not match {:?}",
            function.name,
            local.ty,
            elem.seq_type()
        )));
    }
    validate_shared_callable(prog, function, SharedCallableSurface::Tseq)
}

fn validate_shared_component_method(
    prog: &TbProgram,
    function: &TbFunction,
) -> Result<(), EmitError> {
    if function.owner.is_some() {
        return Err(EmitError(format!(
            "TB-IR common layout component method `{}` unexpectedly has a testbench owner",
            function.name
        )));
    }
    validate_shared_callable(prog, function, SharedCallableSurface::ComponentMethod)
}

fn validate_shared_transactor_method(
    prog: &TbProgram,
    function: &TbFunction,
) -> Result<(), EmitError> {
    if function.owner.is_some() {
        return Err(EmitError(format!(
            "TB-IR common layout transactor method `{}` unexpectedly has a testbench owner",
            function.name
        )));
    }
    let FunctionKind::TransactorBody {
        transactor,
        member,
        name,
    } = &function.kind
    else {
        return Err(EmitError(format!(
            "TB-IR common layout planner expected `{}` to be a transactor method",
            function.name
        )));
    };
    let schema = prog.transactors.get(transactor.index()).ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout transactor method `{}` references missing transactor x{}",
            function.name, transactor.0
        ))
    })?;
    let method = schema.methods.get(member.index()).ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout transactor method `{}` references missing member xm{}",
            function.name, member.0
        ))
    })?;
    if method.function != function.id || method.name != *name {
        return Err(EmitError(format!(
            "TB-IR common layout transactor method `{}` has stale owner metadata",
            function.name
        )));
    }
    validate_shared_callable(prog, function, SharedCallableSurface::TransactorMethod)
}

fn validate_shared_callable(
    prog: &TbProgram,
    function: &TbFunction,
    surface: SharedCallableSurface,
) -> Result<(), EmitError> {
    if function.params.len() > function.locals.len() {
        return Err(EmitError(format!(
            "TB-IR common layout {} `{}` has {} params but only {} locals",
            match surface {
                SharedCallableSurface::Helper => "helper",
                SharedCallableSurface::Tseq => "tseq",
                SharedCallableSurface::TestbenchMethod => "testbench method",
                SharedCallableSurface::ComponentMethod => "component method",
                SharedCallableSurface::TransactorMethod => "transactor method",
            },
            function.name,
            function.params.len(),
            function.locals.len()
        )));
    }
    for (index, param) in function.params.iter().enumerate() {
        let Some(local) = function.locals.get(index) else {
            unreachable!("parameter count checked above")
        };
        if local.ty != param.ty {
            return Err(EmitError(format!(
                "TB-IR common layout {} `{}` param {} metadata {:?} does not match mirrored local {:?}",
                match surface {
                    SharedCallableSurface::Helper => "helper",
                    SharedCallableSurface::Tseq => "tseq",
                    SharedCallableSurface::TestbenchMethod => "testbench method",
                    SharedCallableSurface::ComponentMethod => "component method",
                    SharedCallableSurface::TransactorMethod => "transactor method",
                },
                function.name,
                index,
                param.ty,
                local.ty
            )));
        }
    }
    for local in &function.locals {
        match (&local.ty, surface) {
            (
                IrType::Component(component),
                SharedCallableSurface::ComponentMethod | SharedCallableSurface::TestbenchMethod,
            ) => {
                if prog.components.get(component.index()).is_none() {
                    return Err(EmitError(format!(
                        "TB-IR common layout component method `{}` local `{}` references missing component c{}",
                        function.name, local.name, component.0
                    )));
                }
            }
            _ => validate_common_local_type(prog, &local.ty).map_err(|(feature, ticket)| {
                unsupported_function(
                    function,
                    &format!("local `{}` with {feature}", local.name),
                    ticket,
                )
            })?,
        }
    }

    for (block_index, block) in function.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            match stmt {
                Stmt::Assign(local, value) => {
                    validate_local(function, *local)?;
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::DutWrite(port, value)
                    if matches!(
                        surface,
                        SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TransactorMethod
                    ) =>
                {
                    validate_shared_port(prog, function, block_index, port, surface)?;
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::DutRead(local, port)
                    if matches!(
                        surface,
                        SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TransactorMethod
                    ) =>
                {
                    validate_local(function, *local)?;
                    validate_shared_port(prog, function, block_index, port, surface)?;
                }
                Stmt::ProbeRelease(port)
                    if matches!(
                        surface,
                        SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TransactorMethod
                    ) =>
                {
                    validate_shared_port(prog, function, block_index, port, surface)?;
                }
                Stmt::RecordInit(local, record) => {
                    validate_local(function, *local)?;
                    if prog.records.get(record.index()).is_none() {
                        return Err(EmitError(format!(
                            "TB-IR common layout {} `{}` b{} references missing record r{}",
                            shared_surface_name(surface),
                            function.name,
                            block_index,
                            record.0
                        )));
                    }
                }
                Stmt::RecordFieldWrite {
                    local,
                    mid_indices,
                    index,
                    value,
                    ..
                } => {
                    validate_local(function, *local)?;
                    for (_, expr) in mid_indices {
                        validate_shared_expr(prog, function, block_index, expr, surface)?;
                    }
                    if let Some(expr) = index {
                        validate_shared_expr(prog, function, block_index, expr, surface)?;
                    }
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::TransactorStateWrite {
                    instance, value, ..
                } if matches!(
                    surface,
                    SharedCallableSurface::TestbenchMethod
                        | SharedCallableSurface::TransactorMethod
                ) =>
                {
                    validate_shared_transactor_state_receiver(prog, function, instance, surface)?;
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::TransactorStateRecordFieldWrite {
                    instance,
                    mid_indices,
                    index,
                    value,
                    ..
                } if matches!(
                    surface,
                    SharedCallableSurface::TestbenchMethod
                        | SharedCallableSurface::TransactorMethod
                ) =>
                {
                    validate_shared_transactor_state_receiver(prog, function, instance, surface)?;
                    for (_, index) in mid_indices {
                        validate_shared_expr(prog, function, block_index, index, surface)?;
                    }
                    if let Some(index) = index {
                        validate_shared_expr(prog, function, block_index, index, surface)?;
                    }
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::TransactorStateQueuePush {
                    instance, value, ..
                } if matches!(
                    surface,
                    SharedCallableSurface::TestbenchMethod
                        | SharedCallableSurface::TransactorMethod
                ) =>
                {
                    validate_shared_transactor_state_receiver(prog, function, instance, surface)?;
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::TransactorStateQueuePop { instance, dest, .. }
                    if matches!(
                        surface,
                        SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::TransactorMethod
                    ) =>
                {
                    validate_shared_transactor_state_receiver(prog, function, instance, surface)?;
                    validate_local(function, *dest)?;
                }
                Stmt::SeqPush { seq, value } if surface == SharedCallableSurface::Tseq => {
                    validate_local(function, *seq)?;
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::Log { args, .. } if surface == SharedCallableSurface::Tseq => {
                    validate_shared_fmt(prog, function, block_index, args, surface)?;
                }
                Stmt::AssertCheck { cond, on_fail } | Stmt::AssumeCheck { cond, on_fail }
                    if matches!(
                        surface,
                        SharedCallableSurface::Tseq
                            | SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TransactorMethod
                    ) =>
                {
                    validate_shared_expr(prog, function, block_index, cond, surface)?;
                    validate_shared_fmt(prog, function, block_index, on_fail, surface)?;
                }
                Stmt::Log { args, .. }
                    if matches!(
                        surface,
                        SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TransactorMethod
                    ) =>
                {
                    validate_shared_fmt(prog, function, block_index, args, surface)?;
                }
                Stmt::ComponentFieldWrite { base, value, .. }
                | Stmt::ComponentQueuePush { base, value, .. }
                    if matches!(
                        surface,
                        SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TestbenchMethod
                    ) =>
                {
                    validate_shared_component_base(prog, function, base, surface)?;
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::ComponentEmit { base, args, .. }
                    if matches!(
                        surface,
                        SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TestbenchMethod
                    ) =>
                {
                    validate_shared_component_base(prog, function, base, surface)?;
                    for arg in args {
                        validate_shared_expr(prog, function, block_index, arg, surface)?;
                    }
                }
                Stmt::ComponentVecElementWrite {
                    base,
                    index,
                    inner_index,
                    value,
                    ..
                } if matches!(
                    surface,
                    SharedCallableSurface::ComponentMethod | SharedCallableSurface::TestbenchMethod
                ) =>
                {
                    validate_shared_component_base(prog, function, base, surface)?;
                    validate_shared_expr(prog, function, block_index, index, surface)?;
                    if let Some(inner) = inner_index {
                        validate_shared_expr(prog, function, block_index, inner, surface)?;
                    }
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::ComponentQueuePop { base, dest, .. }
                    if matches!(
                        surface,
                        SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TestbenchMethod
                    ) =>
                {
                    validate_shared_component_base(prog, function, base, surface)?;
                    validate_local(function, *dest)?;
                }
                Stmt::ComponentSubAssign { dst, src, .. } | Stmt::ComponentAssign { dst, src }
                    if matches!(
                        surface,
                        SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TestbenchMethod
                    ) =>
                {
                    validate_shared_component_base(prog, function, dst, surface)?;
                    validate_shared_component_base(prog, function, src, surface)?;
                }
                Stmt::ComponentInit {
                    local, component, ..
                } if matches!(
                    surface,
                    SharedCallableSurface::ComponentMethod | SharedCallableSurface::TestbenchMethod
                ) =>
                {
                    validate_local(function, *local)?;
                    if prog.components.get(component.index()).is_none() {
                        return Err(EmitError(format!(
                            "TB-IR common layout component method `{}` references missing component c{}",
                            function.name, component.0
                        )));
                    }
                }
                Stmt::ComponentCall { base, args, .. }
                    if matches!(
                        surface,
                        SharedCallableSurface::ComponentMethod
                            | SharedCallableSurface::TestbenchMethod
                    ) =>
                {
                    validate_shared_component_base(prog, function, base, surface)?;
                    for arg in args {
                        validate_shared_expr(prog, function, block_index, arg, surface)?;
                    }
                }
                Stmt::TbFieldWrite { value, .. } | Stmt::TbQueuePush { value, .. }
                    if surface == SharedCallableSurface::TestbenchMethod =>
                {
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::TbFieldVecElementWrite {
                    index,
                    inner_index,
                    value,
                    ..
                } if surface == SharedCallableSurface::TestbenchMethod => {
                    validate_shared_expr(prog, function, block_index, index, surface)?;
                    if let Some(inner) = inner_index {
                        validate_shared_expr(prog, function, block_index, inner, surface)?;
                    }
                    validate_shared_expr(prog, function, block_index, value, surface)?;
                }
                Stmt::TbQueuePop { dest, .. }
                    if surface == SharedCallableSurface::TestbenchMethod =>
                {
                    validate_local(function, *dest)?;
                }
                Stmt::ScoreboardOp {
                    nested_path, op, ..
                } if matches!(
                    surface,
                    SharedCallableSurface::TestbenchMethod | SharedCallableSurface::ComponentMethod
                ) =>
                {
                    match (surface, nested_path.as_deref()) {
                        (SharedCallableSurface::TestbenchMethod, None) => {}
                        (SharedCallableSurface::ComponentMethod, Some(path))
                            if path.first().map(String::as_str) == Some("self") => {}
                        _ => {
                            return Err(unsupported_function(
                                function,
                                "scoreboard state without a typed callable receiver",
                                "ticket 05",
                            ));
                        }
                    }
                    match op {
                        ir::ScoreboardOp::QueuePush { value, .. }
                        | ir::ScoreboardOp::ScalarWrite { value, .. } => {
                            validate_shared_expr(prog, function, block_index, value, surface)?;
                        }
                        ir::ScoreboardOp::QueuePop { dest, .. } => {
                            validate_local(function, *dest)?;
                        }
                    }
                }
                Stmt::TestbenchCall {
                    function: callee,
                    args,
                    dest,
                    ..
                } if surface == SharedCallableSurface::TestbenchMethod => {
                    let target = prog.functions.get(callee.index()).ok_or_else(|| {
                        EmitError(format!(
                            "TB-IR common layout testbench method `{}` references missing fn{}",
                            function.name, callee.0
                        ))
                    })?;
                    if !matches!(target.kind, FunctionKind::TestbenchMethod { .. }) {
                        return Err(EmitError(format!(
                            "TB-IR common layout testbench method `{}` calls non-testbench fn{}",
                            function.name, callee.0
                        )));
                    }
                    for arg in args {
                        validate_shared_expr(prog, function, block_index, arg, surface)?;
                    }
                    if let Some(dest) = dest {
                        validate_local(function, *dest)?;
                    }
                }
                Stmt::TransactorCall { dest, call }
                    if surface == SharedCallableSurface::TestbenchMethod =>
                {
                    let Expr::Call(
                        ir::CallTarget::TransactorMethod {
                            bus_field,
                            method,
                            target:
                                ir::TransactorMethodTarget::Callable {
                                    transactor,
                                    function: callee,
                                },
                        },
                        args,
                    ) = call
                    else {
                        return Err(unsupported_function(
                            function,
                            "malformed transactor call",
                            "invalid transactor call provenance",
                        ));
                    };
                    validate_shared_testbench_transactor_field(
                        prog,
                        function,
                        bus_field,
                        *transactor,
                    )?;
                    let method_schema = prog
                        .transactors
                        .get(transactor.index())
                        .and_then(|schema| schema.method(method))
                        .filter(|method| method.function == *callee)
                        .ok_or_else(|| {
                            unsupported_function(
                                function,
                                "stale transactor call target",
                                "invalid transactor call provenance",
                            )
                        })?;
                    if args.len() != method_schema.param_tys.len() {
                        return Err(unsupported_function(
                            function,
                            "transactor call with inconsistent argument metadata",
                            "invalid transactor call provenance",
                        ));
                    }
                    for arg in args {
                        validate_shared_expr(prog, function, block_index, arg, surface)?;
                    }
                    if let Some(dest) = dest {
                        validate_local(function, *dest)?;
                    }
                }
                Stmt::TlmFork(desc)
                    if matches!(
                        surface,
                        SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::TransactorMethod
                    ) =>
                {
                    validate_shared_tlm_desc(prog, function, block_index, desc, surface)?;
                }
                Stmt::TlmJoinAll(pending)
                    if matches!(
                        surface,
                        SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::TransactorMethod
                    ) =>
                {
                    for desc in pending {
                        validate_shared_tlm_desc(prog, function, block_index, desc, surface)?;
                    }
                }
                Stmt::TransactorSelfCall { dest, call }
                    if surface == SharedCallableSurface::TransactorMethod =>
                {
                    let Expr::Call(
                        ir::CallTarget::TransactorSelfMethod {
                            transactor,
                            function: callee,
                            ..
                        },
                        args,
                    ) = call
                    else {
                        return Err(unsupported_function(
                            function,
                            "malformed transactor self-call",
                            "invalid transactor call provenance",
                        ));
                    };
                    if !matches!(
                        function.kind,
                        FunctionKind::TransactorBody { transactor: owner, .. }
                            if owner == *transactor
                    ) {
                        return Err(unsupported_function(
                            function,
                            "cross-transactor self-call",
                            "invalid transactor call provenance",
                        ));
                    }
                    let target = prog.functions.get(callee.index()).ok_or_else(|| {
                        EmitError(format!(
                            "TB-IR common layout transactor method `{}` references missing fn{}",
                            function.name, callee.0
                        ))
                    })?;
                    if !matches!(
                        target.kind,
                        FunctionKind::TransactorBody { transactor: owner, .. }
                            if owner == *transactor
                    ) {
                        return Err(unsupported_function(
                            function,
                            "stale transactor self-call target",
                            "invalid transactor call provenance",
                        ));
                    }
                    for arg in args {
                        validate_shared_expr(prog, function, block_index, arg, surface)?;
                    }
                    if let Some(dest) = dest {
                        validate_local(function, *dest)?;
                    }
                }
                Stmt::PropertyCheck(_)
                | Stmt::CycleHandler(_)
                | Stmt::EventSubscribe { .. }
                | Stmt::MethodHookSubscribe { .. }
                    if matches!(
                        surface,
                        SharedCallableSurface::TestbenchMethod
                            | SharedCallableSurface::ComponentMethod
                    ) =>
                {
                    return Err(unsupported_function(
                        function,
                        &format!(
                            "{} registration with undefined once-versus-per-call semantics",
                            stmt_kind(stmt)
                        ),
                        "ticket 06 fail-closed boundary",
                    ));
                }
                other => {
                    return Err(unsupported_function(
                        function,
                        stmt_kind(other),
                        stmt_ticket(other),
                    ));
                }
            }
        }
        match &block.terminator {
            Terminator::Jump(_) | Terminator::Return => {}
            Terminator::Branch(cond, _, _) => {
                validate_shared_expr(prog, function, block_index, cond, surface)?;
            }
            Terminator::WaitCycles(cycles, None, _) | Terminator::WaitCyclesSync(cycles, _)
                if matches!(
                    surface,
                    SharedCallableSurface::Tseq
                        | SharedCallableSurface::TestbenchMethod
                        | SharedCallableSurface::ComponentMethod
                        | SharedCallableSurface::TransactorMethod
                ) =>
            {
                validate_shared_expr(prog, function, block_index, cycles, surface)?;
            }
            Terminator::WaitCycles(cycles, Some(_), _)
                if matches!(
                    surface,
                    SharedCallableSurface::TestbenchMethod
                        | SharedCallableSurface::ComponentMethod
                        | SharedCallableSurface::TransactorMethod
                ) =>
            {
                validate_shared_expr(prog, function, block_index, cycles, surface)?;
            }
            Terminator::WaitUntil { preds, .. }
                if matches!(
                    surface,
                    SharedCallableSurface::Tseq
                        | SharedCallableSurface::TestbenchMethod
                        | SharedCallableSurface::ComponentMethod
                        | SharedCallableSurface::TransactorMethod
                ) =>
            {
                for pred in preds {
                    validate_shared_expr(prog, function, block_index, &pred.expr, surface)?;
                }
            }
            Terminator::WaitTimePs(_, _)
                if matches!(
                    surface,
                    SharedCallableSurface::TestbenchMethod
                        | SharedCallableSurface::ComponentMethod
                        | SharedCallableSurface::TransactorMethod
                ) => {}
            Terminator::Randomize { target, .. }
                if matches!(
                    surface,
                    SharedCallableSurface::Tseq
                        | SharedCallableSurface::TestbenchMethod
                        | SharedCallableSurface::ComponentMethod
                        | SharedCallableSurface::TransactorMethod
                ) =>
            {
                validate_local(function, *target)?;
            }
            other => {
                return Err(unsupported_function(
                    function,
                    terminator_kind(other),
                    terminator_ticket(other),
                ));
            }
        }
    }
    Ok(())
}

fn shared_surface_name(surface: SharedCallableSurface) -> &'static str {
    match surface {
        SharedCallableSurface::Helper => "helper",
        SharedCallableSurface::Tseq => "tseq",
        SharedCallableSurface::TestbenchMethod => "testbench method",
        SharedCallableSurface::ComponentMethod => "component method",
        SharedCallableSurface::TransactorMethod => "transactor method",
    }
}

fn validate_component_base(
    function: &TbFunction,
    base: &ir::ComponentBase,
) -> Result<(), EmitError> {
    match base {
        ir::ComponentBase::SelfField | ir::ComponentBase::Path(_) => Ok(()),
        ir::ComponentBase::Local(local) => validate_local(function, *local),
    }
}

fn validate_shared_testbench_transactor_field(
    prog: &TbProgram,
    function: &TbFunction,
    field: &str,
    transactor: TransactorId,
) -> Result<(), EmitError> {
    let FunctionKind::TestbenchMethod { testbench, .. } = function.kind else {
        return Err(unsupported_function(
            function,
            "transactor field outside a testbench method",
            "invalid shared transactor receiver",
        ));
    };
    let mut implementations = 0usize;
    for instance in prog
        .testbenches
        .iter()
        .filter(|instance| instance.type_id == testbench)
    {
        implementations += 1;
        let matches = instance
            .transactor_fields
            .iter()
            .filter(|(candidate, owner)| candidate == field && *owner == transactor)
            .count();
        if matches != 1 {
            return Err(EmitError(format!(
                "TB-IR common layout testbench method `{}` requires transactor field `{field}` x{}, but implementation `{}` has {matches} matching binding(s)",
                function.name, transactor.0, instance.name
            )));
        }
    }
    if implementations == 0 {
        return Err(EmitError(format!(
            "TB-IR common layout testbench method `{}` has no concrete implementation for type tbt{}",
            function.name, testbench.0
        )));
    }
    Ok(())
}

fn validate_shared_transactor_state_receiver(
    prog: &TbProgram,
    function: &TbFunction,
    instance: &str,
    surface: SharedCallableSurface,
) -> Result<(), EmitError> {
    match surface {
        SharedCallableSurface::TransactorMethod if instance.is_empty() => Ok(()),
        SharedCallableSurface::TransactorMethod => Err(unsupported_function(
            function,
            "transactor state with a baked instance receiver",
            "invalid shared transactor receiver",
        )),
        SharedCallableSurface::TestbenchMethod if !instance.is_empty() => {
            let fields = func::testbench_method_transactor_state_fields(prog, function)?;
            if fields.iter().any(|(field, _)| field == instance) {
                Ok(())
            } else {
                Err(unsupported_function(
                    function,
                    &format!("transactor state for unbound field `{instance}`"),
                    "invalid shared transactor receiver",
                ))
            }
        }
        _ => Err(unsupported_function(
            function,
            "transactor state without a typed callable receiver",
            "invalid shared transactor receiver",
        )),
    }
}

fn validate_shared_component_base(
    prog: &TbProgram,
    function: &TbFunction,
    base: &ir::ComponentBase,
    surface: SharedCallableSurface,
) -> Result<(), EmitError> {
    if surface == SharedCallableSurface::ComponentMethod {
        return validate_component_base(function, base);
    }
    let FunctionKind::TestbenchMethod { testbench, .. } = function.kind else {
        return Err(unsupported_function(
            function,
            "component receiver outside a component/testbench method",
            "ticket 05",
        ));
    };
    match base {
        ir::ComponentBase::Local(local) => validate_local(function, *local),
        ir::ComponentBase::Path(path) => {
            let Some(root) = path.first() else {
                return Err(unsupported_function(
                    function,
                    "empty testbench component path",
                    "ticket 05",
                ));
            };
            let schema = prog
                .testbench_types
                .get(testbench.index())
                .ok_or_else(|| unsupported_unplaced_function(prog, function))?;
            if schema
                .component_fields
                .iter()
                .any(|(field, _)| field == root)
            {
                Ok(())
            } else {
                Err(unsupported_function(
                    function,
                    &format!("component path rooted at non-member `{root}`"),
                    "ticket 05",
                ))
            }
        }
        ir::ComponentBase::SelfField => Err(unsupported_function(
            function,
            "component self receiver in a testbench method",
            "ticket 05",
        )),
    }
}

fn validate_shared_fmt(
    prog: &TbProgram,
    function: &TbFunction,
    block: usize,
    args: &FmtArgs,
    surface: SharedCallableSurface,
) -> Result<(), EmitError> {
    for arg in &args.args {
        validate_shared_expr(prog, function, block, &arg.expr, surface)?;
    }
    Ok(())
}

fn validate_shared_port(
    prog: &TbProgram,
    function: &TbFunction,
    block: usize,
    port: &PortRef,
    surface: SharedCallableSurface,
) -> Result<(), EmitError> {
    match &port.origin {
        ir::PortOrigin::Dut => {}
        ir::PortOrigin::BusBinding { .. } if surface == SharedCallableSurface::TestbenchMethod => {}
        ir::PortOrigin::BoundBus
            if matches!(
                surface,
                SharedCallableSurface::ComponentMethod | SharedCallableSurface::TransactorMethod
            ) => {}
        _ => {
            return Err(unsupported_function(
                function,
                "bus-relative DUT access without a planned callable adapter",
                "missing callable bus adapter",
            ));
        }
    }
    if matches!(port.origin, ir::PortOrigin::Dut) {
        let Some(probe) = port.probe else {
            return ir::visit::try_visit_port_lane_expr(port, &mut |index| {
                validate_shared_expr(prog, function, block, index, surface)
            });
        };
        let schema = prog.probes.get(probe.index()).ok_or_else(|| {
            EmitError(format!(
                "TB-IR common layout {} `{}` b{block} references missing probe p{}",
                shared_surface_name(surface),
                function.name,
                probe.0
            ))
        })?;
        if !schema.shared {
            return Err(EmitError(format!(
                "TB-IR common layout {} `{}` b{block} references probe p{} `{}` outside its exact test cohort",
                shared_surface_name(surface),
                function.name,
                probe.0,
                schema.name
            )));
        }
    }
    ir::visit::try_visit_port_lane_expr(port, &mut |index| {
        validate_shared_expr(prog, function, block, index, surface)
    })
}

fn validate_shared_expr(
    prog: &TbProgram,
    function: &TbFunction,
    block: usize,
    expr: &Expr,
    surface: SharedCallableSurface,
) -> Result<(), EmitError> {
    let recur = |expr| validate_shared_expr(prog, function, block, expr, surface);
    match expr {
        Expr::Literal { ty, .. } => validate_common_scalar_type(ty)
            .map_err(|feature| unsupported_function(function, &feature, "ticket 04")),
        Expr::WideLiteral(_) => Ok(()),
        Expr::Local(local) => validate_local(function, *local),
        Expr::Port(port)
            if matches!(
                surface,
                SharedCallableSurface::TestbenchMethod
                    | SharedCallableSurface::ComponentMethod
                    | SharedCallableSurface::TransactorMethod
            ) =>
        {
            validate_shared_port(prog, function, block, port, surface)?;
            Ok(())
        }
        Expr::RecordField {
            local,
            mid_indices,
            index,
            ..
        } => {
            validate_local(function, *local)?;
            for (_, expr) in mid_indices {
                recur(expr)?;
            }
            if let Some(expr) = index {
                recur(expr)?;
            }
            Ok(())
        }
        Expr::DynamicListQuery { target, .. } => recur(target),
        Expr::SeqLen(local)
            if matches!(
                surface,
                SharedCallableSurface::Tseq
                    | SharedCallableSurface::TestbenchMethod
                    | SharedCallableSurface::ComponentMethod
            ) =>
        {
            validate_local(function, *local)
        }
        Expr::SeqIndex { seq, index }
            if matches!(
                surface,
                SharedCallableSurface::Tseq
                    | SharedCallableSurface::TestbenchMethod
                    | SharedCallableSurface::ComponentMethod
            ) =>
        {
            validate_local(function, *seq)?;
            recur(index)
        }
        Expr::CycleCount | Expr::ErrorCount
            if matches!(
                surface,
                SharedCallableSurface::Tseq
                    | SharedCallableSurface::TestbenchMethod
                    | SharedCallableSurface::ComponentMethod
            ) =>
        {
            Ok(())
        }
        Expr::ComponentField { base, .. } | Expr::ComponentValue { base }
            if matches!(
                surface,
                SharedCallableSurface::ComponentMethod | SharedCallableSurface::TestbenchMethod
            ) =>
        {
            validate_shared_component_base(prog, function, base, surface)
        }
        Expr::ComponentVecElement {
            base,
            index,
            inner_index,
            ..
        } if matches!(
            surface,
            SharedCallableSurface::ComponentMethod | SharedCallableSurface::TestbenchMethod
        ) =>
        {
            validate_shared_component_base(prog, function, base, surface)?;
            recur(index)?;
            if let Some(inner) = inner_index {
                recur(inner)?;
            }
            Ok(())
        }
        Expr::ComponentQueueQuery { base, .. }
            if matches!(
                surface,
                SharedCallableSurface::ComponentMethod | SharedCallableSurface::TestbenchMethod
            ) =>
        {
            validate_shared_component_base(prog, function, base, surface)
        }
        Expr::ScoreboardQuery { nested_path, .. }
            if matches!(
                surface,
                SharedCallableSurface::TestbenchMethod | SharedCallableSurface::ComponentMethod
            ) =>
        {
            match (surface, nested_path.as_deref()) {
                (SharedCallableSurface::TestbenchMethod, None) => Ok(()),
                (SharedCallableSurface::ComponentMethod, Some(path))
                    if path.first().map(String::as_str) == Some("self") =>
                {
                    Ok(())
                }
                _ => Err(unsupported_function(
                    function,
                    "scoreboard query without a typed callable receiver",
                    "ticket 05",
                )),
            }
        }
        Expr::TbField { .. } | Expr::TbQueueQuery { .. }
            if surface == SharedCallableSurface::TestbenchMethod =>
        {
            Ok(())
        }
        Expr::TransactorState { instance, .. }
        | Expr::TransactorStateQueueQuery { instance, .. }
            if matches!(
                surface,
                SharedCallableSurface::TestbenchMethod | SharedCallableSurface::TransactorMethod
            ) =>
        {
            validate_shared_transactor_state_receiver(prog, function, instance, surface)
        }
        Expr::TransactorStateRecordField {
            instance,
            mid_indices,
            index,
            ..
        } if matches!(
            surface,
            SharedCallableSurface::TestbenchMethod | SharedCallableSurface::TransactorMethod
        ) =>
        {
            validate_shared_transactor_state_receiver(prog, function, instance, surface)?;
            for (_, index) in mid_indices {
                recur(index)?;
            }
            if let Some(index) = index {
                recur(index)?;
            }
            Ok(())
        }
        Expr::TransactorIdle {
            field,
            transactor,
            n,
            ..
        } if surface == SharedCallableSurface::TestbenchMethod => {
            validate_shared_testbench_transactor_field(prog, function, field, *transactor)?;
            recur(n)
        }
        Expr::Binary(_, lhs, rhs) => {
            recur(lhs)?;
            recur(rhs)
        }
        Expr::Unary(_, inner) | Expr::BitSlice { target: inner, .. } => recur(inner),
        Expr::BitSliceDyn { target, hi, lo } => {
            recur(target)?;
            recur(hi)?;
            recur(lo)
        }
        Expr::Ternary(cond, lhs, rhs) => {
            recur(cond)?;
            recur(lhs)?;
            recur(rhs)
        }
        Expr::WidthCast { width, inner, .. } => {
            if *width == 0 || *width > crate::MAX_WIDTH_METHOD_BITS {
                return Err(unsupported_function(
                    function,
                    &format!("a width cast to uint<{width}>"),
                    "ticket 04",
                ));
            }
            recur(inner)
        }
        Expr::PortSnapshotLane {
            snapshot,
            port,
            index,
        } if matches!(
            surface,
            SharedCallableSurface::TestbenchMethod | SharedCallableSurface::ComponentMethod
        ) =>
        {
            validate_local(function, *snapshot)?;
            validate_shared_port(prog, function, block, port, surface)?;
            recur(index)
        }
        Expr::Call(
            ir::CallTarget::Helper {
                function: callee,
                name,
                ..
            },
            args,
        ) => {
            if prog.functions.get(callee.index()).is_none_or(|candidate| {
                candidate.id != *callee
                    || candidate.kind != FunctionKind::Helper
                    || candidate.name != *name
            }) {
                return Err(EmitError(format!(
                    "TB-IR common layout {} `{}` b{} references inconsistent helper fn{} `{name}`",
                    shared_surface_name(surface),
                    function.name,
                    block,
                    callee.0
                )));
            }
            for arg in args {
                recur(arg)?;
            }
            Ok(())
        }
        Expr::Call(
            ir::CallTarget::Tseq {
                function: callee,
                name,
            },
            args,
        ) if matches!(
            surface,
            SharedCallableSurface::Tseq
                | SharedCallableSurface::TestbenchMethod
                | SharedCallableSurface::ComponentMethod
        ) =>
        {
            if prog.functions.get(callee.index()).is_none_or(|candidate| {
                candidate.id != *callee
                    || !matches!(candidate.kind, FunctionKind::Tseq { .. })
                    || candidate.name != *name
            }) {
                return Err(EmitError(format!(
                    "TB-IR common layout {} `{}` b{} references inconsistent tseq fn{} `{name}`",
                    shared_surface_name(surface),
                    function.name,
                    block,
                    callee.0
                )));
            }
            for arg in args {
                recur(arg)?;
            }
            Ok(())
        }
        Expr::Call(
            ir::CallTarget::TransactorMethod {
                bus_field,
                method,
                target,
            },
            args,
        ) if matches!(
            surface,
            SharedCallableSurface::TestbenchMethod | SharedCallableSurface::TransactorMethod
        ) =>
        {
            validate_shared_tlm_target(function, block, bus_field, method, target, surface)?;
            for arg in args {
                recur(arg)?;
            }
            Ok(())
        }
        Expr::StringLiteral(_) => Ok(()),
        Expr::Call(ir::CallTarget::ExternFn { .. }, args) => {
            for arg in args {
                recur(arg)?;
            }
            Ok(())
        }
        other => Err(unsupported_function(
            function,
            expr_kind(other),
            expr_ticket(other),
        )),
    }
}

fn validate_shared_tlm_desc(
    prog: &TbProgram,
    function: &TbFunction,
    block: usize,
    desc: &ir::TlmForkDesc,
    surface: SharedCallableSurface,
) -> Result<(), EmitError> {
    validate_shared_tlm_target(
        function,
        block,
        &desc.bus_field,
        &desc.method,
        &desc.target,
        surface,
    )?;
    for arg in &desc.args {
        validate_shared_expr(prog, function, block, arg, surface)?;
    }
    if let Some(dest) = desc.dest {
        validate_local(function, dest)?;
    }
    Ok(())
}

fn validate_shared_tlm_target(
    function: &TbFunction,
    block: usize,
    bus_field: &str,
    method: &str,
    target: &ir::TransactorMethodTarget,
    surface: SharedCallableSurface,
) -> Result<(), EmitError> {
    match (surface, target) {
        (
            SharedCallableSurface::TestbenchMethod,
            ir::TransactorMethodTarget::TestbenchBusField {
                testbench,
                field,
                bus: _,
            },
        ) if field == bus_field
            && matches!(
                function.kind,
                FunctionKind::TestbenchMethod { testbench: owner, .. } if owner == *testbench
            ) => Ok(()),
        (SharedCallableSurface::TransactorMethod, ir::TransactorMethodTarget::BoundBus)
            if bus_field == "bus" =>
        {
            Ok(())
        }
        _ => Err(EmitError(format!(
            "TB-IR common layout {} `{}` b{block} carries a mismatched adapter target for `{bus_field}.{method}`",
            shared_surface_name(surface), function.name
        ))),
    }
}

fn tseq_directly_uses_context(function: &TbFunction) -> bool {
    if function.blocks.iter().any(|block| {
        block.stmts.iter().any(|stmt| {
            matches!(
                stmt,
                Stmt::Log { .. } | Stmt::AssertCheck { .. } | Stmt::AssumeCheck { .. }
            )
        }) || matches!(
            block.terminator,
            Terminator::WaitCycles(_, _, _)
                | Terminator::WaitCyclesSync(_, _)
                | Terminator::WaitTimePs(_, _)
                | Terminator::WaitUntil { .. }
                | Terminator::WaitUntilTimeout { .. }
                | Terminator::Randomize { .. }
                | Terminator::Fatal(_)
        )
    }) {
        return true;
    }
    let mut contextual = false;
    func::for_each_function_expr(function, |expr| {
        contextual |= matches!(expr, Expr::CycleCount | Expr::ErrorCount);
    });
    contextual
}

fn validate_testbench(
    prog: &TbProgram,
    name: &str,
    tb: &ir::TestbenchSchema,
) -> Result<(), EmitError> {
    // As above, omitting `..` makes future testbench state fail closed at
    // compile time instead of silently bypassing the tracer-bullet allowlist.
    let ir::TestbenchSchema {
        type_id: _,
        name: _,
        dut_field: _,
        dut_type: _,
        probes: _,
        cov_fields,
        scalar_fields: _,
        queue_fields: _,
        state_fields: _,
        connects: _,
        record_fields: _,
        bus_bindings: _,
        bound_bus_instances,
        transactor_fields,
        passive_transactor_fields,
        scoreboard_fields: _,
        regblock_bindings,
        target_tlm_actors,
        component_fields: _,
        unbound_state_actors,
        synthetic: _,
        periodic_services: _,
        cycle_services: _,
    } = tb;
    let type_schema = prog
        .testbench_types
        .get(tb.type_id.index())
        .ok_or_else(|| {
            EmitError(format!(
                "TB-IR common layout test `{name}` references missing testbench type tbt{}",
                tb.type_id.0
            ))
        })?;
    for (field, component) in &type_schema.component_fields {
        let matches = tb
            .component_fields
            .iter()
            .filter(|binding| binding.field == *field && binding.component == *component)
            .count();
        if matches != 1 {
            return Err(EmitError(format!(
                "TB-IR common layout test `{name}` must provide exactly one typed binding for declared component field `{field}` c{}; found {matches}",
                component.0
            )));
        }
    }
    for (field, covgroup) in cov_fields {
        if prog.covgroups.get(covgroup.index()).is_none() {
            return Err(EmitError(format!(
                "TB-IR common layout test `{name}` field `{field}` references missing covergroup cg{}",
                covgroup.0
            )));
        }
    }
    let _ = (
        regblock_bindings,
        bound_bus_instances,
        transactor_fields,
        passive_transactor_fields,
        target_tlm_actors,
        unbound_state_actors,
    );
    Ok(())
}

fn validate_testbench_services(
    prog: &TbProgram,
    opts: &EmitOpts,
    test: &ir::TestSchema,
    tb: &ir::TestbenchSchema,
) -> Result<(), EmitError> {
    let validate_body = |function: FunctionId| -> Result<(), EmitError> {
        let body = prog.functions.get(function.index()).ok_or_else(|| {
            EmitError(format!(
                "TB-IR common layout test `{}` references missing lifecycle body fn{}",
                test.name, function.0
            ))
        })?;
        if body.id != function
            || !matches!(body.kind, FunctionKind::TestHook { .. })
            || body.owner != Some(test.testbench)
            || !body.params.is_empty()
        {
            return Err(EmitError(format!(
                "TB-IR common layout test `{}` has invalid lifecycle body fn{}",
                test.name, function.0
            )));
        }
        for (block, cfg) in body.blocks.iter().enumerate() {
            for stmt in &cfg.stmts {
                validate_stmt(prog, opts, test, body, block, stmt)?;
            }
            validate_terminator(prog, opts, test, body, block, &cfg.terminator)?;
        }
        Ok(())
    };
    for service in &tb.periodic_services {
        if service.period == 0 {
            return Err(EmitError(format!(
                "TB-IR common layout test `{}` has a zero-period lifecycle service",
                test.name
            )));
        }
        validate_body(service.function)?;
    }
    for service in &tb.cycle_services {
        validate_expr(
            opts,
            test,
            prog.function(service.function),
            0,
            &service.trigger,
        )?;
        validate_body(service.function)?;
    }
    Ok(())
}

fn plan_test_clocks(
    test_index: usize,
    test: &ir::TestSchema,
    dut_access: &DutAccessPlan,
) -> Result<CommonTestClockPlan, EmitError> {
    let mut clocks = Vec::with_capacity(test.clocks.len());
    for clock in &test.clocks {
        if clock.period_ps <= 0 || clock.period_ps % 2 != 0 {
            return Err(unsupported_in_test(
                &test.name,
                &format!(
                    "clock `{}` with non-positive or odd period {} ps",
                    clock.name, clock.period_ps
                ),
                "invalid clock topology",
            ));
        }
        dut_access
            .validate_clock(test.id, &clock.name)
            .map_err(|error| {
                EmitError(format!(
                    "TB-IR common layout test `{}` has an invalid clock access: {error}",
                    test.name
                ))
            })?;
        clocks.push(CommonClockPlan {
            name: clock.name.clone(),
            period_ps: clock.period_ps,
            domain: clock.domain.clone(),
        });
    }
    Ok(CommonTestClockPlan { test_index, clocks })
}

fn validate_test_function(
    prog: &TbProgram,
    opts: &EmitOpts,
    test: &ir::TestSchema,
    function: &TbFunction,
    expected_member: ir::TestCallableMember,
) -> Result<(), EmitError> {
    // Function metadata additions must be reviewed alongside statement and
    // expression additions before common-layout placement can accept them.
    let TbFunction {
        id: _,
        name: _,
        kind: _,
        params: _,
        locals: _,
        blocks: _,
        entry: _,
        owner: _,
        testbench_record_locals: _,
        ret: _,
        implicit_returns: _,
    } = function;
    let expected_kind = FunctionKind::TestBody {
        test: test.id,
        member: expected_member,
        name: test.name.clone(),
    };
    if function.kind != expected_kind
        || function.name
            != format!(
                "{}_{}",
                match expected_member {
                    ir::TestCallableMember::Run => "run",
                    ir::TestCallableMember::Check => "check",
                },
                test.name
            )
    {
        return Err(EmitError(format!(
            "TB-IR common layout planner expected test `{}` function fn{} to have kind {}",
            test.name,
            function.id.0,
            function_kind_name(&expected_kind),
        )));
    }
    if function.owner != Some(test.testbench) {
        return Err(EmitError(format!(
            "TB-IR common layout planner found test `{}` {} function fn{} with the wrong owner",
            test.name,
            function_kind_name(&expected_kind),
            function.id.0
        )));
    }
    if !function.params.is_empty() || function.ret.is_some() {
        return Err(unsupported_function(
            function,
            &format!(
                "{} parameters or return storage",
                function_kind_name(&expected_kind)
            ),
            "ticket 03",
        ));
    }
    validate_capsule_bus_fields(prog, test, function)?;
    for local in &function.locals {
        validate_common_local_type(prog, &local.ty).map_err(|(feature, ticket)| {
            unsupported_function(
                function,
                &format!("local `{}` with {feature}", local.name),
                ticket,
            )
        })?;
    }
    for (block_index, block) in function.blocks.iter().enumerate() {
        for stmt in &block.stmts {
            validate_stmt(prog, opts, test, function, block_index, stmt)?;
        }
        validate_terminator(prog, opts, test, function, block_index, &block.terminator)?;
    }
    Ok(())
}

fn validate_capsule_bus_fields(
    prog: &TbProgram,
    test: &ir::TestSchema,
    function: &TbFunction,
) -> Result<(), EmitError> {
    let fields = crate::ir::passes::callable_placement::function_concrete_bus_fields(function);
    if fields.is_empty() {
        return Ok(());
    }
    let testbench = prog
        .testbenches
        .get(test.testbench.index())
        .ok_or_else(|| {
            EmitError(format!(
                "TB-IR common layout test `{}` references missing testbench tb{}",
                test.name, test.testbench.0
            ))
        })?;
    for field in fields {
        if !testbench
            .bus_bindings
            .iter()
            .any(|binding| binding.field == field)
        {
            return Err(unsupported_at(
                test,
                function,
                0,
                &format!("bus-relative DUT access through missing binding `{field}`"),
                "missing concrete bus adapter",
            ));
        }
    }
    Ok(())
}

fn validate_stmt(
    prog: &TbProgram,
    opts: &EmitOpts,
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    stmt: &Stmt,
) -> Result<(), EmitError> {
    match stmt {
        Stmt::Assign(local, value) => {
            validate_local(function, *local)?;
            validate_expr(opts, test, function, block, value)
        }
        Stmt::DutWrite(port, value) => {
            validate_port(opts, test, function, block, port)?;
            validate_expr(opts, test, function, block, value)
        }
        Stmt::DutRead(local, port) => {
            validate_local(function, *local)?;
            validate_port(opts, test, function, block, port)
        }
        Stmt::ProbeRelease(port) => validate_port(opts, test, function, block, port),
        Stmt::Log { args, .. } => validate_fmt_args(opts, test, function, block, args),
        Stmt::AssertCheck { cond, on_fail } | Stmt::AssumeCheck { cond, on_fail } => {
            validate_expr(opts, test, function, block, cond)?;
            validate_fmt_args(opts, test, function, block, on_fail)
        }
        Stmt::RecordInit(local, _) | Stmt::AggregateInit(local) => {
            validate_local(function, *local)?;
            Ok(())
        }
        Stmt::RecordRead {
            dest, local, addr, ..
        } => {
            validate_local(function, *dest)?;
            validate_local(function, *local)?;
            validate_expr(opts, test, function, block, addr)
        }
        Stmt::RecordWrite {
            local, addr, value, ..
        } => {
            validate_local(function, *local)?;
            validate_expr(opts, test, function, block, addr)?;
            validate_expr(opts, test, function, block, value)
        }
        Stmt::RecordFieldWrite {
            local,
            mid_indices,
            index,
            value,
            ..
        } => {
            validate_local(function, *local)?;
            for (_, expr) in mid_indices {
                validate_expr(opts, test, function, block, expr)?;
            }
            if let Some(expr) = index {
                validate_expr(opts, test, function, block, expr)?;
            }
            validate_expr(opts, test, function, block, value)
        }
        Stmt::TbFieldWrite { value, .. } | Stmt::TbQueuePush { value, .. } => {
            validate_expr(opts, test, function, block, value)
        }
        Stmt::TbFieldVecElementWrite {
            index,
            inner_index,
            value,
            ..
        } => {
            validate_expr(opts, test, function, block, index)?;
            if let Some(inner) = inner_index {
                validate_expr(opts, test, function, block, inner)?;
            }
            validate_expr(opts, test, function, block, value)
        }
        Stmt::TbQueuePop { dest, .. } => validate_local(function, *dest),
        Stmt::ScoreboardOp { op, .. } => match op {
            ir::ScoreboardOp::QueuePush { value, .. }
            | ir::ScoreboardOp::ScalarWrite { value, .. } => {
                validate_expr(opts, test, function, block, value)
            }
            ir::ScoreboardOp::QueuePop { dest, .. } => validate_local(function, *dest),
        },
        Stmt::ComponentCall {
            base,
            component,
            method,
            function: target,
            args,
            dest,
        } => {
            validate_component_base(function, base)?;
            let schema = prog
                .components
                .get(component.index())
                .and_then(|component| component.methods.iter().find(|entry| entry.name == *method))
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout component call in test `{}` function `{}` references missing component method c{}.{method}",
                        test.name, function.name, component.0
                    ))
                })?;
            if schema.function != *target {
                return Err(EmitError(format!(
                    "TB-IR common layout component call `c{}.{method}` in test `{}` resolves to fn{} but carries fn{}",
                    component.0, test.name, schema.function.0, target.0
                )));
            }
            if schema.param_tys.len() != args.len() {
                return Err(EmitError(format!(
                    "TB-IR common layout component call `c{}.{method}` in test `{}` has {} argument(s), expected {}",
                    component.0,
                    test.name,
                    args.len(),
                    schema.param_tys.len()
                )));
            }
            if let Some(local) = dest {
                validate_local(function, *local)?;
            }
            for arg in args {
                validate_expr(opts, test, function, block, arg)?;
            }
            Ok(())
        }
        Stmt::ComponentEmit { base, args, .. } => {
            validate_component_base(function, base)?;
            for arg in args {
                validate_expr(opts, test, function, block, arg)?;
            }
            Ok(())
        }
        Stmt::ComponentFieldWrite { base, value, .. }
        | Stmt::ComponentQueuePush { base, value, .. } => {
            validate_component_base(function, base)?;
            validate_expr(opts, test, function, block, value)
        }
        Stmt::ComponentVecElementWrite {
            base,
            index,
            inner_index,
            value,
            ..
        } => {
            validate_component_base(function, base)?;
            validate_expr(opts, test, function, block, index)?;
            if let Some(inner) = inner_index {
                validate_expr(opts, test, function, block, inner)?;
            }
            validate_expr(opts, test, function, block, value)
        }
        Stmt::ComponentQueuePop { base, dest, .. } => {
            validate_component_base(function, base)?;
            validate_local(function, *dest)
        }
        Stmt::ComponentInit {
            local, component, ..
        } => {
            validate_local(function, *local)?;
            if prog.components.get(component.index()).is_none() {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` initializes missing component c{}",
                    test.name, component.0
                )));
            }
            Ok(())
        }
        Stmt::ComponentSubAssign { dst, src, .. } | Stmt::ComponentAssign { dst, src } => {
            validate_component_base(function, dst)?;
            validate_component_base(function, src)
        }
        Stmt::TestbenchCall { args, dest, .. } => {
            for arg in args {
                validate_expr(opts, test, function, block, arg)?;
            }
            if let Some(local) = dest {
                validate_local(function, *local)?;
            }
            Ok(())
        }
        Stmt::PropertyCheck(property) => {
            validate_capsule_registration_site(
                test,
                function,
                block,
                "concurrent property registration",
            )?;
            let schema = prog.property_checks.get(property.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout test `{}` function `{}` references missing property p{}",
                    test.name, function.name, property.0
                ))
            })?;
            for temporal in &schema.temporals {
                validate_expr(opts, test, function, block, &temporal.inner)?;
            }
            match &schema.shape {
                ir::PropertyShape::Implies { ante, cons }
                | ir::PropertyShape::ImpliesNext { ante, cons } => {
                    validate_expr(opts, test, function, block, ante)?;
                    validate_expr(opts, test, function, block, cons)?;
                }
                ir::PropertyShape::Invariant(expr) => {
                    validate_expr(opts, test, function, block, expr)?;
                }
            }
            if let Some(message) = &schema.message {
                validate_fmt_args(opts, test, function, block, message)?;
            }
            Ok(())
        }
        Stmt::CoverCheck(cover) => {
            validate_capsule_registration_site(
                test,
                function,
                block,
                "concurrent cover registration",
            )?;
            let schema = prog.cover_checks.get(cover.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout test `{}` function `{}` references missing cover c{}",
                    test.name, function.name, cover.0
                ))
            })?;
            validate_expr(opts, test, function, block, &schema.cond)?;
            for temporal in &schema.temporals {
                validate_expr(opts, test, function, block, &temporal.inner)?;
            }
            Ok(())
        }
        Stmt::CycleHandler(handler) => {
            validate_capsule_registration_site(
                test,
                function,
                block,
                "cycle-handler registration",
            )?;
            let schema = prog.cycle_handlers.get(handler.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout test `{}` function `{}` references missing cycle handler h{}",
                    test.name, function.name, handler.0
                ))
            })?;
            if let ir::CycleHandlerKind::Trigger { trigger, .. } = &schema.kind {
                validate_expr(opts, test, function, block, trigger)?;
            }
            let body = prog.functions.get(schema.function.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout cycle handler h{} references missing fn{}",
                    handler.0, schema.function.0
                ))
            })?;
            if body.id != schema.function
                || !matches!(body.kind, FunctionKind::TestHook { .. })
                || body.owner != Some(test.testbench)
                || !body.params.is_empty()
            {
                return Err(EmitError(format!(
                    "TB-IR common layout cycle handler h{} has an invalid body fn{}",
                    handler.0, schema.function.0
                )));
            }
            for (body_block, cfg) in body.blocks.iter().enumerate() {
                for stmt in &cfg.stmts {
                    validate_stmt(prog, opts, test, body, body_block, stmt)?;
                }
                validate_terminator(prog, opts, test, body, body_block, &cfg.terminator)?;
            }
            Ok(())
        }
        Stmt::EventSubscribe { event, handler, .. } => {
            validate_capsule_registration_site(test, function, block, "event subscription")?;
            let payload = match event {
                ir::EventChannelRef::Local(local) => {
                    validate_local(function, *local)?;
                    let Some(IrType::Event(payload)) =
                        function.locals.get(local.index()).map(|local| &local.ty)
                    else {
                        return Err(EmitError(format!(
                            "TB-IR common layout event subscription in `{}` references non-event local %{}",
                            function.name, local.0
                        )));
                    };
                    payload.clone()
                }
                ir::EventChannelRef::Component {
                    base,
                    component,
                    event,
                    payload,
                } => {
                    validate_component_base(function, base)?;
                    let field_payload = prog
                        .components
                        .get(component.index())
                        .and_then(|schema| schema.field(event))
                        .and_then(|field| match &field.kind {
                            ir::ComponentFieldKind::Event { payload } => Some(payload.clone()),
                            _ => None,
                        })
                        .ok_or_else(|| {
                            EmitError(format!(
                                "TB-IR common layout event subscription in `{}` references missing component event c{}.{event}",
                                function.name, component.0
                            ))
                        })?;
                    if field_payload != *payload {
                        return Err(EmitError(format!(
                            "TB-IR common layout event subscription in `{}` carries a stale payload for c{}.{event}",
                            function.name, component.0
                        )));
                    }
                    payload.clone()
                }
            };
            let body = prog.functions.get(handler.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout event subscription references missing fn{}",
                    handler.0
                ))
            })?;
            let expected = event_payload_ir_type(payload);
            if body.id != *handler
                || !matches!(body.kind, FunctionKind::TestHook { .. })
                || body.owner != Some(test.testbench)
                || body.params.len() != 1
                || body.locals.first().map(|local| &local.ty) != Some(&expected)
            {
                return Err(EmitError(format!(
                    "TB-IR common layout event subscription has invalid body fn{}",
                    handler.0
                )));
            }
            for (body_block, cfg) in body.blocks.iter().enumerate() {
                for stmt in &cfg.stmts {
                    validate_stmt(prog, opts, test, body, body_block, stmt)?;
                }
                validate_terminator(prog, opts, test, body, body_block, &cfg.terminator)?;
            }
            Ok(())
        }
        Stmt::EventEmit { event, args } => {
            validate_local(function, *event)?;
            if !matches!(
                function.locals.get(event.index()).map(|local| &local.ty),
                Some(IrType::Event(_))
            ) {
                return Err(EmitError(format!(
                    "TB-IR common layout event emission in `{}` references non-event local %{}",
                    function.name, event.0
                )));
            }
            if args.len() != 1 {
                return Err(EmitError(format!(
                    "TB-IR common layout event emission in `{}` has {} payload values, expected 1",
                    function.name,
                    args.len()
                )));
            }
            for arg in args {
                validate_expr(opts, test, function, block, arg)?;
            }
            Ok(())
        }
        Stmt::MethodHookSubscribe {
            target:
                ir::MethodHookTarget::Component {
                    base,
                    component,
                    method,
                },
            handler,
            captures,
            ..
        } => {
            validate_capsule_registration_site(test, function, block, "method-hook subscription")?;
            validate_component_base(function, base)?;
            let method_schema = prog
                .components
                .get(component.index())
                .and_then(|schema| schema.methods.iter().find(|entry| entry.name == *method))
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` hook references missing component method c{}.{method}",
                        test.name, component.0
                    ))
                })?;
            if !method_schema.hookable {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` hooks non-hookable component method c{}.{method}",
                    test.name, component.0
                )));
            }
            for capture in captures {
                validate_local(function, *capture)?;
            }
            let body = prog.functions.get(handler.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout component hook references missing fn{}",
                    handler.0
                ))
            })?;
            if body.id != *handler
                || !matches!(body.kind, FunctionKind::TestHook { .. })
                || body.owner != Some(test.testbench)
            {
                return Err(EmitError(format!(
                    "TB-IR common layout component hook has invalid body fn{}",
                    handler.0
                )));
            }
            for (body_block, cfg) in body.blocks.iter().enumerate() {
                for stmt in &cfg.stmts {
                    validate_stmt(prog, opts, test, body, body_block, stmt)?;
                }
                validate_terminator(prog, opts, test, body, body_block, &cfg.terminator)?;
            }
            Ok(())
        }
        Stmt::MethodHookSubscribe {
            target:
                ir::MethodHookTarget::Transactor {
                    field,
                    transactor,
                    method,
                },
            handler,
            captures,
            ..
        } => {
            validate_capsule_registration_site(test, function, block, "method-hook subscription")?;
            let tb = prog.testbench(test.testbench);
            if !tb
                .transactor_fields
                .iter()
                .any(|(candidate, ty)| candidate == field && ty == transactor)
            {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` hook references missing transactor field `{field}` x{}",
                    test.name, transactor.0
                )));
            }
            if !tb
                .unbound_state_actors
                .iter()
                .any(|actor| actor.field == *field && actor.transactor == *transactor)
            {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` hook `{field}.{method}` has no planned instance state",
                    test.name
                )));
            }
            let method_schema = prog
                .transactors
                .get(transactor.index())
                .and_then(|schema| schema.method(method))
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` hook references missing transactor method x{}.{method}",
                        test.name, transactor.0
                    ))
                })?;
            if !method_schema.hookable {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` hooks non-hookable transactor method x{}.{method}",
                    test.name, transactor.0
                )));
            }
            for capture in captures {
                validate_local(function, *capture)?;
            }
            let body = prog.functions.get(handler.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout transactor hook references missing fn{}",
                    handler.0
                ))
            })?;
            if body.id != *handler
                || !matches!(body.kind, FunctionKind::TestHook { .. })
                || body.owner != Some(test.testbench)
            {
                return Err(EmitError(format!(
                    "TB-IR common layout transactor hook has invalid body fn{}",
                    handler.0
                )));
            }
            for (body_block, cfg) in body.blocks.iter().enumerate() {
                for stmt in &cfg.stmts {
                    validate_stmt(prog, opts, test, body, body_block, stmt)?;
                }
                validate_terminator(prog, opts, test, body, body_block, &cfg.terminator)?;
            }
            Ok(())
        }
        Stmt::TlmFork(desc) => validate_capsule_tlm_desc(opts, test, function, block, desc),
        Stmt::TlmJoinAll(pending) => {
            for desc in pending {
                validate_capsule_tlm_desc(opts, test, function, block, desc)?;
            }
            Ok(())
        }
        Stmt::TransactorCall { dest, call } => {
            let Expr::Call(
                ir::CallTarget::TransactorMethod {
                    bus_field,
                    method,
                    target:
                        ir::TransactorMethodTarget::Callable {
                            transactor,
                            function: callee,
                        },
                },
                args,
            ) = call
            else {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` function `{}` carries a malformed transactor call",
                    test.name, function.name
                )));
            };
            let schema = prog.transactors.get(transactor.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout test `{}` calls missing transactor x{}",
                    test.name, transactor.0
                ))
            })?;
            let method_schema = schema
                .method(method)
                .filter(|candidate| candidate.function == *callee)
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` carries stale transactor call `{bus_field}.{method}` to fn{}",
                        test.name, callee.0
                    ))
                })?;
            if args.len() != method_schema.param_tys.len() {
                return Err(EmitError(format!(
                    "TB-IR common layout transactor call `{bus_field}.{method}` in test `{}` has {} argument(s), expected {}",
                    test.name,
                    args.len(),
                    method_schema.param_tys.len()
                )));
            }
            for arg in args {
                validate_expr(opts, test, function, block, arg)?;
            }
            if let Some(dest) = dest {
                validate_local(function, *dest)?;
            }
            Ok(())
        }
        Stmt::FailDiag { guard, args } => {
            if let Some(guard) = guard {
                validate_expr(opts, test, function, block, guard)?;
            }
            validate_fmt_args(opts, test, function, block, args)
        }
        Stmt::CovReport(inst) => {
            let testbench = prog
                .testbenches
                .get(test.testbench.index())
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` references missing testbench tb{}",
                        test.name, test.testbench.0
                    ))
                })?;
            if !testbench
                .cov_fields
                .iter()
                .any(|(field, covgroup)| field == &inst.tb_field && *covgroup == inst.covgroup)
            {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` function `{}` b{block} references stale covergroup field `{}`",
                    test.name, function.name, inst.tb_field
                )));
            }
            Ok(())
        }
        Stmt::RecordWriteCb { local, value, .. } => {
            validate_local(function, *local)?;
            validate_expr(opts, test, function, block, value)
        }
        Stmt::TransactorStateWrite { .. }
        | Stmt::TransactorStateRecordFieldWrite { .. }
        | Stmt::TransactorStateQueuePush { .. }
        | Stmt::TransactorStateQueuePop { .. }
        | Stmt::TransactorSelfCall { .. }
        | Stmt::SeqPush { .. } => Err(unsupported_at(
            test,
            function,
            block,
            stmt_kind(stmt),
            stmt_ticket(stmt),
        )),
    }
}

fn validate_terminator(
    prog: &TbProgram,
    opts: &EmitOpts,
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    terminator: &Terminator,
) -> Result<(), EmitError> {
    match terminator {
        Terminator::Jump(_) | Terminator::Return => Ok(()),
        Terminator::Branch(cond, _, _) => validate_expr(opts, test, function, block, cond),
        Terminator::WaitCycles(cycles, None, _) => {
            validate_expr(opts, test, function, block, cycles)
        }
        Terminator::WaitCycles(cycles, Some(clock), _) => {
            if !test
                .clocks
                .iter()
                .any(|candidate| candidate.name == clock.name)
            {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` callable fn{} `{}` waits on clock `{}` absent from the test topology",
                    test.name, function.id.0, function.name, clock.name
                )));
            }
            validate_expr(opts, test, function, block, cycles)
        }
        Terminator::Fatal(args) => validate_fmt_args(opts, test, function, block, args),
        Terminator::WaitUntil { preds, .. } => {
            for pred in preds {
                validate_expr(opts, test, function, block, &pred.expr)?;
            }
            Ok(())
        }
        Terminator::WaitUntilTimeout { preds, cycles, .. } => {
            for pred in preds {
                validate_expr(opts, test, function, block, &pred.expr)?;
            }
            validate_expr(opts, test, function, block, cycles)
        }
        Terminator::Randomize { target, .. } => validate_local(function, *target),
        Terminator::TbLifecycleCall {
            function: lifecycle,
            ..
        } => {
            let body = prog.functions.get(lifecycle.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout test `{}` references missing testbench lifecycle fn{}",
                    test.name, lifecycle.0
                ))
            })?;
            let lifecycle_owner = match body.kind {
                FunctionKind::TestbenchLifecycle { testbench, .. } => Some(testbench),
                _ => None,
            };
            let same_testbench_type = lifecycle_owner.is_some_and(|owner| {
                prog.testbenches
                    .get(owner.index())
                    .zip(prog.testbenches.get(test.testbench.index()))
                    .is_some_and(|(lhs, rhs)| lhs.type_id == rhs.type_id)
            });
            if body.id != *lifecycle || !same_testbench_type || !body.params.is_empty() {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` has invalid testbench lifecycle fn{}",
                    test.name, lifecycle.0
                )));
            }
            for (body_block, cfg) in body.blocks.iter().enumerate() {
                for stmt in &cfg.stmts {
                    validate_stmt(prog, opts, test, body, body_block, stmt)?;
                }
                validate_terminator(prog, opts, test, body, body_block, &cfg.terminator)?;
            }
            Ok(())
        }
        Terminator::WaitCyclesSync(_, _) | Terminator::WaitTimePs(_, _) => Err(unsupported_at(
            test,
            function,
            block,
            terminator_kind(terminator),
            terminator_ticket(terminator),
        )),
    }
}

fn validate_fmt_args(
    opts: &EmitOpts,
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    args: &FmtArgs,
) -> Result<(), EmitError> {
    for arg in &args.args {
        validate_expr(opts, test, function, block, &arg.expr)?;
    }
    Ok(())
}

fn validate_expr(
    opts: &EmitOpts,
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    expr: &Expr,
) -> Result<(), EmitError> {
    match expr {
        Expr::Literal {
            ty: IrType::Unknown,
            ..
        } => Ok(()),
        Expr::Literal { ty, .. } => validate_common_scalar_type(ty)
            .map_err(|feature| unsupported_at(test, function, block, &feature, "ticket 04")),
        Expr::Local(local) => validate_local(function, *local),
        Expr::Port(port) => validate_port(opts, test, function, block, port),
        Expr::CycleCount | Expr::ErrorCount => Ok(()),
        Expr::Binary(op, lhs, rhs) => {
            match op {
                ir::BinOp::Add
                | ir::BinOp::Sub
                | ir::BinOp::Mul
                | ir::BinOp::Div
                | ir::BinOp::Mod
                | ir::BinOp::Eq
                | ir::BinOp::Ne
                | ir::BinOp::Lt
                | ir::BinOp::Le
                | ir::BinOp::Gt
                | ir::BinOp::Ge
                | ir::BinOp::And
                | ir::BinOp::Or
                | ir::BinOp::BitAnd
                | ir::BinOp::BitOr
                | ir::BinOp::BitXor
                | ir::BinOp::Shl
                | ir::BinOp::Shr => {}
            }
            validate_expr(opts, test, function, block, lhs)?;
            validate_expr(opts, test, function, block, rhs)
        }
        Expr::Unary(op, inner) => {
            match op {
                ir::UnOp::Neg | ir::UnOp::Not | ir::UnOp::BitNot | ir::UnOp::BitNotHost => {}
            }
            validate_expr(opts, test, function, block, inner)
        }
        Expr::Ternary(cond, lhs, rhs) => {
            validate_expr(opts, test, function, block, cond)?;
            validate_expr(opts, test, function, block, lhs)?;
            validate_expr(opts, test, function, block, rhs)
        }
        Expr::BitSlice { target, .. } => validate_expr(opts, test, function, block, target),
        Expr::BitSliceDyn { target, hi, lo } => {
            validate_expr(opts, test, function, block, target)?;
            validate_expr(opts, test, function, block, hi)?;
            validate_expr(opts, test, function, block, lo)
        }
        Expr::WidthCast {
            kind,
            width,
            src_width: _,
            inner,
        } => {
            match kind {
                ir::WidthCastKind::Trunc
                | ir::WidthCastKind::Zext
                | ir::WidthCastKind::Sext
                | ir::WidthCastKind::Resize => {}
            }
            if *width == 0 || *width > crate::MAX_WIDTH_METHOD_BITS {
                return Err(unsupported_at(
                    test,
                    function,
                    block,
                    &format!("a width cast to uint<{width}>"),
                    "ticket 04",
                ));
            }
            validate_expr(opts, test, function, block, inner)
        }
        Expr::WideLiteral(_) | Expr::StringLiteral(_) | Expr::TbField(_) => Ok(()),
        Expr::RecordField {
            local,
            mid_indices,
            index,
            ..
        } => {
            validate_local(function, *local)?;
            for (_, expr) in mid_indices {
                validate_expr(opts, test, function, block, expr)?;
            }
            if let Some(expr) = index {
                validate_expr(opts, test, function, block, expr)?;
            }
            Ok(())
        }
        Expr::TbFieldVecElement {
            index, inner_index, ..
        } => {
            validate_expr(opts, test, function, block, index)?;
            if let Some(inner) = inner_index {
                validate_expr(opts, test, function, block, inner)?;
            }
            Ok(())
        }
        Expr::TbQueueQuery { .. }
        | Expr::ScoreboardQuery {
            nested_path: None, ..
        } => Ok(()),
        Expr::ComponentField { base, .. } | Expr::ComponentValue { base } => {
            validate_component_base(function, base)
        }
        Expr::ComponentVecElement {
            base,
            index,
            inner_index,
            ..
        } => {
            validate_component_base(function, base)?;
            validate_expr(opts, test, function, block, index)?;
            if let Some(inner) = inner_index {
                validate_expr(opts, test, function, block, inner)?;
            }
            Ok(())
        }
        Expr::ComponentQueueQuery { base, .. } => validate_component_base(function, base),
        Expr::DynamicListQuery { target, .. } => validate_expr(opts, test, function, block, target),
        Expr::SeqLen(local) => validate_local(function, *local),
        Expr::SeqIndex { seq, index } => {
            validate_local(function, *seq)?;
            validate_expr(opts, test, function, block, index)
        }
        Expr::Call(ir::CallTarget::Helper { .. } | ir::CallTarget::Tseq { .. }, args) => {
            for arg in args {
                validate_expr(opts, test, function, block, arg)?;
            }
            Ok(())
        }
        Expr::Call(ir::CallTarget::ExternFn { .. }, args) => {
            for arg in args {
                validate_expr(opts, test, function, block, arg)?;
            }
            Ok(())
        }
        Expr::Call(
            ir::CallTarget::TransactorMethod {
                bus_field,
                method,
                target,
            },
            args,
        ) => {
            validate_capsule_tlm_target(test, function, block, bus_field, method, target)?;
            for arg in args {
                validate_expr(opts, test, function, block, arg)?;
            }
            Ok(())
        }
        Expr::TemporalSlot { .. } => Ok(()),
        Expr::TransactorState { .. } | Expr::TransactorStateQueueQuery { .. } => Ok(()),
        Expr::TransactorStateRecordField {
            mid_indices, index, ..
        } => {
            for (_, index) in mid_indices {
                validate_expr(opts, test, function, block, index)?;
            }
            if let Some(index) = index {
                validate_expr(opts, test, function, block, index)?;
            }
            Ok(())
        }
        Expr::TransactorIdle { n, .. } => validate_expr(opts, test, function, block, n),
        Expr::PortSnapshotLane {
            snapshot,
            port,
            index,
        } => {
            validate_local(function, *snapshot)?;
            validate_port(opts, test, function, block, port)?;
            validate_expr(opts, test, function, block, index)
        }
        Expr::CovBin { .. } => Ok(()),
        Expr::ScoreboardQuery {
            nested_path: Some(_),
            ..
        }
        | Expr::ComponentIdle { .. }
        | Expr::CovHookParam { .. }
        | Expr::CovHookArg { .. }
        | Expr::Call(_, _) => Err(unsupported_at(
            test,
            function,
            block,
            expr_kind(expr),
            expr_ticket(expr),
        )),
        Expr::RegRead { mirror, .. } => validate_local(function, *mirror),
    }
}

fn validate_local(function: &TbFunction, local: ir::LocalId) -> Result<(), EmitError> {
    function.locals.get(local.index()).ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout planner found missing local %{} in function `{}`",
            local.0, function.name
        ))
    })?;
    Ok(())
}

fn validate_capsule_tlm_desc(
    opts: &EmitOpts,
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    desc: &ir::TlmForkDesc,
) -> Result<(), EmitError> {
    validate_capsule_tlm_target(
        test,
        function,
        block,
        &desc.bus_field,
        &desc.method,
        &desc.target,
    )?;
    for arg in &desc.args {
        validate_expr(opts, test, function, block, arg)?;
    }
    if let Some(dest) = desc.dest {
        validate_local(function, dest)?;
    }
    Ok(())
}

fn validate_capsule_tlm_target(
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    bus_field: &str,
    method: &str,
    target: &ir::TransactorMethodTarget,
) -> Result<(), EmitError> {
    let ir::TransactorMethodTarget::ConcreteBusBinding { field, .. } = target else {
        return Err(unsupported_at(
            test,
            function,
            block,
            &format!("transactor call `{bus_field}.{method}` without a concrete test adapter"),
            "missing concrete bus adapter",
        ));
    };
    if field != bus_field {
        return Err(EmitError(format!(
            "TB-IR common layout test `{}` function `{}` b{block} carries a mismatched concrete adapter target for `{bus_field}.{method}`",
            test.name, function.name
        )));
    }
    Ok(())
}

fn validate_capsule_registration_site(
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    construct: &str,
) -> Result<(), EmitError> {
    if matches!(function.kind, FunctionKind::TestBody { .. }) {
        return Ok(());
    }
    Err(unsupported_at(
        test,
        function,
        block,
        &format!(
            "{construct} inside a reusable callback with undefined once-versus-per-call semantics"
        ),
        "ticket 06 fail-closed boundary",
    ))
}

fn event_payload_ir_type(payload: ir::EventPayload) -> IrType {
    payload.value_ir_type()
}

fn validate_common_scalar_type(ty: &IrType) -> Result<(), String> {
    match ty {
        IrType::UInt(None) | IrType::SInt(None) | IrType::Bool | IrType::Unknown => Ok(()),
        IrType::UInt(Some(width)) | IrType::SInt(Some(width))
            if (1..=crate::MAX_WIDTH_METHOD_BITS).contains(width) =>
        {
            Ok(())
        }
        IrType::UInt(Some(width)) | IrType::SInt(Some(width)) => {
            Err(format!("unsupported scalar width {width}"))
        }
        IrType::String => Ok(()),
        IrType::Record(_) => Err("record type".into()),
        IrType::RecordSeq(_) | IrType::Seq(_) => Err("sequence type".into()),
        IrType::FixedVec { .. } => Err("fixed-vector type".into()),
        IrType::Component(_) => Err("component type".into()),
        IrType::PortSnapshot => Err("port-snapshot type".into()),
        IrType::Event(_) => Err("event type".into()),
    }
}

fn validate_common_local_type(prog: &TbProgram, ty: &IrType) -> Result<(), (String, &'static str)> {
    match ty {
        IrType::UInt(_) | IrType::SInt(_) | IrType::Bool | IrType::Unknown => {
            validate_common_scalar_type(ty).map_err(|feature| (feature, "ticket 04"))
        }
        IrType::Record(record) | IrType::RecordSeq(record) => {
            if prog.records.get(record.index()).is_some() {
                Ok(())
            } else {
                Err((format!("missing record r{}", record.0), "ticket 04"))
            }
        }
        IrType::Seq(elem) => validate_common_scalar_type(elem)
            .map_err(|feature| (format!("sequence element with {feature}"), "ticket 04")),
        IrType::String => Ok(()),
        IrType::FixedVec { .. } => Err(("fixed-vector local type".into(), "ticket 04")),
        IrType::Component(component) => {
            if prog.components.get(component.index()).is_some() {
                Ok(())
            } else {
                Err((format!("missing component c{}", component.0), "ticket 05"))
            }
        }
        IrType::PortSnapshot => Ok(()),
        IrType::Event(payload) => match payload {
            ir::EventPayload::Scalar { .. } => validate_common_scalar_type(
                &payload.scalar_ir_type().expect("scalar event payload"),
            )
            .map_err(|feature| (format!("event payload with {feature}"), "ticket 06")),
            ir::EventPayload::Record(record) => {
                if prog.records.get(record.index()).is_some() {
                    Ok(())
                } else {
                    Err((
                        format!("event payload references missing record r{}", record.0),
                        "ticket 06",
                    ))
                }
            }
            ir::EventPayload::FixedVec { .. } => {
                Err(("fixed-vector event payload".into(), "ticket 06"))
            }
        },
    }
}

fn validate_port(
    opts: &EmitOpts,
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    port: &PortRef,
) -> Result<(), EmitError> {
    if matches!(port.origin, ir::PortOrigin::BoundBus) {
        return Err(unsupported_at(
            test,
            function,
            block,
            "bound-bus DUT access",
            "invalid bound-bus provenance",
        ));
    }
    if port.port_path.is_empty() {
        return Err(EmitError(format!(
            "TB-IR common layout function `{}` block {block} has an empty DUT access path",
            function.name
        )));
    }
    ir::visit::try_visit_port_lane_expr(port, &mut |index| {
        validate_expr(opts, test, function, block, index)
    })?;
    if let ir::PortOrigin::BusBinding { binding: _, field } = &port.origin {
        if port.probe.is_some() || port.access != PortAccess::Port {
            return Err(EmitError(format!(
                "TB-IR common layout function `{}` block {block} has noncanonical bus-relative access metadata",
                function.name
            )));
        }
        if port.port_path.first() != Some(field) {
            return Err(EmitError(format!(
                "TB-IR common layout function `{}` block {block} carries bus field `{field}` with mismatched path `{}`",
                function.name,
                port.port_path.join(".")
            )));
        }
        return Ok(());
    }
    if let Some(value_type) = &port.value_type {
        validate_common_scalar_type(value_type).map_err(|feature| {
            unsupported_at(
                test,
                function,
                block,
                &format!("DUT access with {feature}"),
                "ticket 07",
            )
        })?;
    }
    if matches!(port.access, PortAccess::Probe | PortAccess::Force) {
        let Some(width) = port.width else {
            return Err(unsupported_at(
                test,
                function,
                block,
                "probe access whose width is unresolved",
                "ticket 07",
            ));
        };
        if port.probe.is_none() || !port.aggregate_path || port.direction.is_some() {
            return Err(unsupported_at(
                test,
                function,
                block,
                "probe access with noncanonical verified metadata",
                "ticket 07",
            ));
        }
        return validate_common_scalar_type(port.value_type.as_ref().ok_or_else(|| {
            unsupported_at(
                test,
                function,
                block,
                &format!(
                    "probe `{}` without a resolved scalar type",
                    port.port_path[0]
                ),
                "DUT interface capability",
            )
        })?)
        .map_err(|feature| {
            unsupported_at(
                test,
                function,
                block,
                &format!(
                    "probe `{}` with width {width} and {feature}",
                    port.port_path[0]
                ),
                "DUT interface capability",
            )
        });
    }
    if port.probe.is_some() {
        return Err(unsupported_at(
            test,
            function,
            block,
            "ordinary DUT port carrying a probe identity",
            "ticket 07",
        ));
    }
    if opts.dut_interface.is_none() {
        return Err(unsupported_at(
            test,
            function,
            block,
            "DUT access without a resolved interface catalog",
            "ticket 07",
        ));
    }
    Ok(())
}

fn unsupported(feature: &str, ticket: &str) -> EmitError {
    EmitError(format!(
        "TB-IR common layout does not yet support {feature}; use `--cpp-split-layout self-contained` ({ticket})"
    ))
}

fn unsupported_in_test(test: &str, feature: &str, ticket: &str) -> EmitError {
    EmitError(format!(
        "TB-IR common layout does not yet support {feature} in test `{test}`; use `--cpp-split-layout self-contained` ({ticket})"
    ))
}

fn unsupported_function(function: &TbFunction, feature: &str, ticket: &str) -> EmitError {
    EmitError(format!(
        "TB-IR common layout does not yet support {feature} in function `{}`; use `--cpp-split-layout self-contained` ({ticket})",
        function.name
    ))
}

fn unsupported_unplaced_function(prog: &TbProgram, function: &TbFunction) -> EmitError {
    let feature = match function.kind {
        FunctionKind::ComponentMethod { component, .. } => {
            let owner = prog
                .components
                .get(component.index())
                .map(|schema| schema.name.as_str())
                .unwrap_or("<missing-component>");
            let method = prog
                .components
                .get(component.index())
                .and_then(|schema| {
                    schema
                        .methods
                        .iter()
                        .find(|method| method.function == function.id)
                })
                .map(|method| method.name.as_str())
                .unwrap_or(function.name.as_str());
            format!("component method `{owner}.{method}`")
        }
        FunctionKind::TransactorBody { transactor, .. } => {
            let owner = prog
                .transactors
                .get(transactor.index())
                .map(|schema| schema.name.as_str())
                .unwrap_or("<missing-transactor>");
            let method = prog
                .transactors
                .get(transactor.index())
                .and_then(|schema| {
                    schema
                        .methods
                        .iter()
                        .find(|method| method.function == function.id)
                        .map(|method| method.name.as_str())
                        .or_else(|| {
                            schema
                                .target_methods
                                .iter()
                                .find(|method| method.function == function.id)
                                .map(|method| method.name.as_str())
                        })
                })
                .unwrap_or(function.name.as_str());
            format!("transactor method `{owner}.{method}`")
        }
        _ => function_kind_name(&function.kind).to_string(),
    };
    unsupported_function(function, &feature, function_kind_ticket(&function.kind))
}

fn unsupported_at(
    test: &ir::TestSchema,
    function: &TbFunction,
    block: usize,
    feature: &str,
    ticket: &str,
) -> EmitError {
    EmitError(format!(
        "TB-IR common layout does not yet support {feature} in test `{}`, function `{}`, block b{block}; use `--cpp-split-layout self-contained` ({ticket})",
        test.name, function.name
    ))
}

fn function_kind_name(kind: &FunctionKind) -> &'static str {
    match kind {
        FunctionKind::TestBody {
            member: ir::TestCallableMember::Run,
            ..
        } => "a run function",
        FunctionKind::TestBody {
            member: ir::TestCallableMember::Check,
            ..
        } => "a check function",
        FunctionKind::SamplerAuto { .. } => "an automatic coverage sampler",
        FunctionKind::Helper => "a helper function",
        FunctionKind::TestbenchMethod { .. } => "a testbench method",
        FunctionKind::TransactorBody { .. } => "a transactor method",
        FunctionKind::ComponentMethod { .. } => "a component method",
        FunctionKind::Tseq { .. } => "a transaction sequence",
        FunctionKind::TestHook { .. } => "a test hook",
        FunctionKind::TestbenchLifecycle { .. } => "a testbench lifecycle phase",
    }
}

fn function_kind_ticket(kind: &FunctionKind) -> &'static str {
    match kind {
        FunctionKind::TestBody { .. } => "ticket 03",
        FunctionKind::Helper | FunctionKind::Tseq { .. } => "ticket 04",
        FunctionKind::TestbenchMethod { .. } | FunctionKind::ComponentMethod { .. } => "ticket 05",
        FunctionKind::SamplerAuto { .. }
        | FunctionKind::TestHook { .. }
        | FunctionKind::TestbenchLifecycle { .. } => "ticket 06",
        FunctionKind::TransactorBody { .. } => "missing common transactor placement",
    }
}

fn stmt_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Assign(_, _) => "scalar assignment",
        Stmt::DutWrite(_, _) => "direct DUT write",
        Stmt::DutRead(_, _) => "direct DUT read",
        Stmt::ProbeRelease(_) => "probe release",
        Stmt::RecordInit(_, _) => "record initialization",
        Stmt::AggregateInit(_) => "aggregate initialization",
        Stmt::RecordFieldWrite { .. } => "record-field write",
        Stmt::RecordRead { .. } => "RAL record read",
        Stmt::RecordWrite { .. } => "RAL record write",
        Stmt::RecordWriteCb { .. } => "RAL write callback",
        Stmt::TbFieldWrite { .. } => "testbench-state write",
        Stmt::TbFieldVecElementWrite { .. } => "testbench-vector write",
        Stmt::TbQueuePush { .. } | Stmt::TbQueuePop { .. } => "testbench queue operation",
        Stmt::TransactorStateWrite { .. }
        | Stmt::TransactorStateRecordFieldWrite { .. }
        | Stmt::TransactorStateQueuePush { .. }
        | Stmt::TransactorStateQueuePop { .. } => "transactor-state operation",
        Stmt::Log { .. } => "logging",
        Stmt::AssertCheck { .. } => "immediate assertion",
        Stmt::AssumeCheck { .. } => "immediate assumption",
        Stmt::PropertyCheck(_) => "concurrent property",
        Stmt::CoverCheck(_) => "concurrent cover",
        Stmt::CycleHandler(_) => "cycle handler",
        Stmt::EventSubscribe { .. } | Stmt::EventEmit { .. } => "event operation",
        Stmt::MethodHookSubscribe { .. } => "method hook",
        Stmt::CovReport(_) => "coverage report",
        Stmt::TransactorCall { .. } | Stmt::TransactorSelfCall { .. } => "transactor call",
        Stmt::FailDiag { .. } => "wait diagnostic",
        Stmt::ScoreboardOp { .. } => "scoreboard operation",
        Stmt::ComponentFieldWrite { .. }
        | Stmt::ComponentVecElementWrite { .. }
        | Stmt::ComponentEmit { .. }
        | Stmt::ComponentCall { .. }
        | Stmt::ComponentQueuePush { .. }
        | Stmt::ComponentQueuePop { .. }
        | Stmt::ComponentInit { .. }
        | Stmt::ComponentSubAssign { .. }
        | Stmt::ComponentAssign { .. } => "component operation",
        Stmt::TestbenchCall { .. } => "testbench method call",
        Stmt::SeqPush { .. } => "transaction-sequence operation",
        Stmt::TlmFork(_) | Stmt::TlmJoinAll(_) => "TLM operation",
    }
}

fn stmt_ticket(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::ProbeRelease(_) => "ticket 07",
        Stmt::RecordInit(_, _)
        | Stmt::AggregateInit(_)
        | Stmt::RecordFieldWrite { .. }
        | Stmt::ScoreboardOp { .. }
        | Stmt::SeqPush { .. } => "ticket 04",
        Stmt::RecordRead { .. }
        | Stmt::RecordWrite { .. }
        | Stmt::RecordWriteCb { .. }
        | Stmt::CovReport(_) => "per-run state plan",
        Stmt::ComponentEmit { .. } => "ticket 06",
        Stmt::TbFieldWrite { .. }
        | Stmt::TbFieldVecElementWrite { .. }
        | Stmt::TbQueuePush { .. }
        | Stmt::TbQueuePop { .. }
        | Stmt::PropertyCheck(_)
        | Stmt::CycleHandler(_)
        | Stmt::EventSubscribe { .. }
        | Stmt::MethodHookSubscribe { .. }
        | Stmt::EventEmit { .. }
        | Stmt::FailDiag { .. } => "ticket 06",
        Stmt::CoverCheck(_) => "per-run coverage plan",
        Stmt::TransactorStateWrite { .. }
        | Stmt::TransactorStateRecordFieldWrite { .. }
        | Stmt::TransactorStateQueuePush { .. }
        | Stmt::TransactorStateQueuePop { .. }
        | Stmt::TransactorCall { .. }
        | Stmt::TransactorSelfCall { .. }
        | Stmt::TlmFork(_)
        | Stmt::TlmJoinAll(_) => "missing common bus/transactor plan",
        Stmt::ComponentFieldWrite { .. }
        | Stmt::ComponentVecElementWrite { .. }
        | Stmt::ComponentCall { .. }
        | Stmt::ComponentQueuePush { .. }
        | Stmt::ComponentQueuePop { .. }
        | Stmt::ComponentInit { .. }
        | Stmt::ComponentSubAssign { .. }
        | Stmt::ComponentAssign { .. } => "ticket 05",
        Stmt::TestbenchCall { .. } => "ticket 05",
        Stmt::Assign(_, _)
        | Stmt::DutWrite(_, _)
        | Stmt::DutRead(_, _)
        | Stmt::Log { .. }
        | Stmt::AssertCheck { .. }
        | Stmt::AssumeCheck { .. } => "ticket 02",
    }
}

fn terminator_kind(terminator: &Terminator) -> &'static str {
    match terminator {
        Terminator::Jump(_) => "control-flow jump",
        Terminator::Branch(_, _, _) => "control-flow branch",
        Terminator::WaitCycles(_, None, _) => "primary-clock wait",
        Terminator::WaitCycles(_, Some(_), _) => "clock-qualified wait",
        Terminator::WaitCyclesSync(_, _) => "synchronous helper wait",
        Terminator::WaitTimePs(_, _) => "wall-clock wait",
        Terminator::WaitUntil { .. } | Terminator::WaitUntilTimeout { .. } => "predicate wait",
        Terminator::Randomize { .. } => "randomization",
        Terminator::Return => "return",
        Terminator::TbLifecycleCall { .. } => "testbench lifecycle call",
        Terminator::Fatal(_) => "fatal termination",
    }
}

fn terminator_ticket(terminator: &Terminator) -> &'static str {
    match terminator {
        Terminator::WaitCycles(_, Some(_), _)
        | Terminator::WaitCyclesSync(_, _)
        | Terminator::WaitTimePs(_, _)
        | Terminator::WaitUntil { .. }
        | Terminator::WaitUntilTimeout { .. } => "missing common scheduler plan",
        Terminator::Randomize { .. } => "source-backed randomization plan",
        Terminator::TbLifecycleCall { .. } => "ticket 06",
        Terminator::Jump(_)
        | Terminator::Branch(_, _, _)
        | Terminator::WaitCycles(_, None, _)
        | Terminator::Return
        | Terminator::Fatal(_) => "ticket 02",
    }
}

fn expr_kind(expr: &Expr) -> &'static str {
    match expr {
        Expr::Literal { .. } => "scalar literal",
        Expr::StringLiteral(_) => "string value",
        Expr::WideLiteral(_) => "wide literal",
        Expr::Local(_) => "scalar local",
        Expr::Port(_) => "direct DUT port",
        Expr::RecordField { .. } => "record-field read",
        Expr::TbField(_) | Expr::TbFieldVecElement { .. } => "testbench-state read",
        Expr::TemporalSlot { .. } => "temporal property state",
        Expr::TbQueueQuery { .. } => "testbench queue query",
        Expr::TransactorState { .. }
        | Expr::TransactorStateRecordField { .. }
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::TransactorIdle { .. } => "transactor-state read",
        Expr::ScoreboardQuery { .. } => "scoreboard query",
        Expr::ComponentField { .. }
        | Expr::ComponentVecElement { .. }
        | Expr::ComponentValue { .. }
        | Expr::ComponentQueueQuery { .. }
        | Expr::ComponentIdle { .. } => "component expression",
        Expr::DynamicListQuery { .. } => "dynamic-list query",
        Expr::CycleCount => "cycle counter",
        Expr::ErrorCount => "error counter",
        Expr::Binary(_, _, _) => "binary expression",
        Expr::Unary(_, _) => "unary expression",
        Expr::Ternary(_, _, _) => "ternary expression",
        Expr::BitSlice { .. } | Expr::BitSliceDyn { .. } => "bit slice",
        Expr::PortSnapshotLane { .. } => "DUT lane snapshot",
        Expr::WidthCast { .. } => "width cast",
        Expr::CovBin { .. } | Expr::CovHookParam { .. } | Expr::CovHookArg { .. } => {
            "coverage expression"
        }
        Expr::SeqLen(_) | Expr::SeqIndex { .. } => "transaction-sequence expression",
        Expr::Call(target, _) => match target {
            ir::CallTarget::Helper { .. } => "helper call",
            ir::CallTarget::Builtin(_) => "builtin call",
            ir::CallTarget::ExternFn { .. } => "external-function call",
            ir::CallTarget::TransactorMethod { .. }
            | ir::CallTarget::TransactorSelfMethod { .. } => "transactor call",
            ir::CallTarget::Tseq { .. } => "transaction-sequence call",
        },
        Expr::RegRead { .. } => "register read",
    }
}

fn expr_ticket(expr: &Expr) -> &'static str {
    match expr {
        Expr::RecordField { .. }
        | Expr::ScoreboardQuery { .. }
        | Expr::SeqLen(_)
        | Expr::SeqIndex { .. } => "ticket 04",
        Expr::TbField(_)
        | Expr::TbFieldVecElement { .. }
        | Expr::TemporalSlot { .. }
        | Expr::TbQueueQuery { .. } => "ticket 06",
        Expr::TransactorState { .. }
        | Expr::TransactorStateRecordField { .. }
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::TransactorIdle { .. } => "missing common transactor plan",
        Expr::ComponentField { .. }
        | Expr::ComponentVecElement { .. }
        | Expr::ComponentValue { .. }
        | Expr::ComponentQueueQuery { .. }
        | Expr::ComponentIdle { .. } => "ticket 05",
        Expr::Call(target, _) => match target {
            ir::CallTarget::Helper { .. } | ir::CallTarget::Tseq { .. } => "ticket 04",
            ir::CallTarget::TransactorMethod { .. }
            | ir::CallTarget::TransactorSelfMethod { .. } => "missing common transactor plan",
            ir::CallTarget::Builtin(_) | ir::CallTarget::ExternFn { .. } => {
                "source-backed callable metadata"
            }
        },
        Expr::StringLiteral(_)
        | Expr::WideLiteral(_)
        | Expr::DynamicListQuery { .. }
        | Expr::CovBin { .. }
        | Expr::CovHookParam { .. }
        | Expr::CovHookArg { .. } => "per-run coverage plan",
        Expr::RegRead { .. } => "RAL access plan",
        Expr::PortSnapshotLane { .. } => "ticket 07",
        Expr::Literal { .. }
        | Expr::Local(_)
        | Expr::Port(_)
        | Expr::CycleCount
        | Expr::ErrorCount
        | Expr::Binary(_, _, _)
        | Expr::Unary(_, _)
        | Expr::Ternary(_, _, _)
        | Expr::BitSlice { .. }
        | Expr::BitSliceDyn { .. }
        | Expr::WidthCast { .. } => "ticket 02",
    }
}

fn render_common_interface_template(plan: &CommonCppPlan) -> Result<String, EmitError> {
    let prog = plan.program();
    let profile = &plan.interface_build_profile;
    let mut out = String::new();
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(out, "// TB-IR common-object suite interface.").ok();
    writeln!(out, "// harc build-profile: {profile}").ok();
    writeln!(out).ok();
    writeln!(out, "#pragma once").ok();
    writeln!(out).ok();
    writeln!(out, "#include \"V{}.h\"", plan.dut_type).ok();
    writeln!(out, "#include \"verilated.h\"").ok();
    writeln!(out, "#if VM_COVERAGE").ok();
    writeln!(out, "#include \"verilated_cov.h\"").ok();
    writeln!(out, "#endif").ok();
    writeln!(out, "#if defined(HARC_TRACE_VCD)").ok();
    writeln!(out, "#include \"verilated_vcd_c.h\"").ok();
    writeln!(out, "#define HARC_TRACE_ENABLED 1").ok();
    writeln!(out, "using HarcTraceC = VerilatedVcdC;").ok();
    writeln!(out, "#elif defined(HARC_TRACE_FST)").ok();
    writeln!(out, "#include \"verilated_fst_c.h\"").ok();
    writeln!(out, "#define HARC_TRACE_ENABLED 1").ok();
    writeln!(out, "using HarcTraceC = VerilatedFstC;").ok();
    writeln!(out, "#else").ok();
    writeln!(out, "#define HARC_TRACE_ENABLED 0").ok();
    writeln!(out, "#endif").ok();
    writeln!(out, "#include <cstdio>").ok();
    writeln!(out, "#include <cstdarg>").ok();
    writeln!(out, "#include <cstdint>").ok();
    writeln!(out, "#include <cstring>").ok();
    writeln!(out, "#include <array>").ok();
    writeln!(out, "#include <atomic>").ok();
    writeln!(out, "#include <functional>").ok();
    writeln!(out, "#include <memory>").ok();
    writeln!(out, "#include <string>").ok();
    writeln!(out, "#include <thread>").ok();
    writeln!(out, "#include <vector>").ok();
    writeln!(out, "#include \"harc_thread_rt.h\"").ok();
    writeln!(out, "#include \"harc_random_rt.h\"").ok();
    if plan.randomize.is_some() {
        writeln!(out, "#include \"harc_z3_rt.h\"").ok();
    }
    writeln!(out, "#include \"harc_queue_rt.h\"").ok();
    writeln!(out, "#include \"harc_trace_rt.h\"").ok();
    writeln!(out, "#include \"harc_log_rt.h\"").ok();
    writeln!(out).ok();
    writeln!(out, "// === iface-begin ===").ok();
    if !plan.extern_declarations.is_empty() {
        writeln!(
            out,
            "// harc-extern-signatures: {}",
            common_artifacts::stable_hash_hex(plan.extern_declarations.as_bytes())
        )
        .ok();
        out.push_str(&plan.extern_declarations);
        writeln!(out).ok();
    }
    for line in plan.dut_access.abi_lines() {
        writeln!(out, "// harc-dut-access: {line}").ok();
    }
    writeln!(out).ok();
    for shared in &plan.shared_types {
        match shared.kind {
            CommonSharedTypeKind::Record(record) => super::common_record_declaration(
                &mut out,
                &prog.records[record.index()],
                &prog.records,
            ),
            CommonSharedTypeKind::Scoreboard(scoreboard) => super::runtime::scoreboard_struct(
                &mut out,
                scoreboard,
                &prog.scoreboards[scoreboard.index()],
                &prog.records,
                &plan.runtime_cells,
            )?,
            CommonSharedTypeKind::Component(component) => {
                super::runtime::component_struct(
                    &mut out,
                    prog,
                    component,
                    &prog.components[component.index()],
                    &prog.components,
                    &prog.scoreboards,
                    &prog.records,
                    &plan.runtime_cells,
                )?;
            }
            CommonSharedTypeKind::TransactorState(transactor) => {
                super::runtime::common_transactor_state_struct_decl(
                    &mut out,
                    prog,
                    transactor,
                    &prog.transactors[transactor.index()],
                    &prog.records,
                    &plan.runtime_cells,
                )?
            }
            CommonSharedTypeKind::Covergroup(covgroup) => {
                super::covergroup::common_covgroup_declaration(
                    &mut out,
                    &prog.covgroups[covgroup.index()],
                )
            }
            CommonSharedTypeKind::Testbench(testbench) => {
                let tb = &prog.testbenches[testbench.index()];
                let cov_fields = tb
                    .cov_fields
                    .iter()
                    .map(|(field, covgroup)| {
                        (field.clone(), prog.covgroups[covgroup.index()].name.clone())
                    })
                    .collect::<Vec<_>>();
                let scoreboard_fields = tb
                    .scoreboard_fields
                    .iter()
                    .map(|(field, scoreboard)| {
                        (
                            field.clone(),
                            prog.scoreboards[scoreboard.index()].name.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                super::runtime::tb_struct(
                    &mut out,
                    testbench,
                    tb,
                    plan.dut_type(),
                    &cov_fields,
                    &tb.state_fields,
                    &tb.record_fields,
                    &scoreboard_fields,
                    &prog.records,
                    &plan.runtime_cells,
                )?;
            }
        }
    }
    if prog.testbenches.iter().any(|tb| {
        tb.regblock_bindings
            .iter()
            .any(|binding| !binding.callbacks.is_empty())
    }) {
        out.push_str(
            "#ifndef HARC_RAL_CB_MAX_DEPTH\n\
             inline constexpr uint32_t HARC_RAL_CB_MAX_DEPTH = 16;\n\
             #endif\n\n",
        );
    }
    writeln!(out, "struct HarcClockState {{").ok();
    writeln!(out, "{INDENT}const char* name;").ok();
    writeln!(out, "{INDENT}long long half_period_ps;").ok();
    writeln!(out, "{INDENT}long long next_edge_ps;").ok();
    writeln!(out, "{INDENT}int level;").ok();
    writeln!(out, "{INDENT}long long rising_count;").ok();
    writeln!(out, "{INDENT}std::function<void(int)> drive;").ok();
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    writeln!(out, "struct HarcTestContext {{").ok();
    writeln!(out, "{INDENT}VerilatedContext verilated;").ok();
    writeln!(out, "{INDENT}V{}* dut = nullptr;", plan.dut_type).ok();
    writeln!(out, "#if HARC_TRACE_ENABLED").ok();
    writeln!(out, "{INDENT}HarcTraceC* tfp = nullptr;").ok();
    writeln!(out, "{INDENT}std::string wave_path;").ok();
    writeln!(out, "#endif").ok();
    writeln!(out, "{INDENT}uint64_t trace_time = 0;").ok();
    writeln!(out, "{INDENT}int errors = 0;").ok();
    writeln!(out, "{INDENT}bool fatal = false;").ok();
    writeln!(out, "{INDENT}bool run_complete = false;").ok();
    writeln!(out, "{INDENT}int cycle_count = 0;").ok();
    writeln!(out, "{INDENT}long long now_ps = 0;").ok();
    writeln!(out, "{INDENT}std::vector<HarcClockState> clocks;").ok();
    writeln!(out, "{INDENT}harc_rt::trace::HarcTraceWriter trace;").ok();
    writeln!(out, "{INDENT}harc_rt::log::HarcLogContext log_ctx;").ok();
    writeln!(out, "{INDENT}harc_rt::ThreadScheduler scheduler;").ok();
    writeln!(
        out,
        "{INDENT}std::function<void()> advance_actor_schedulers = []() {{}};"
    )
    .ok();
    writeln!(out, "{INDENT}bool actor_tick_due = false;").ok();
    writeln!(
        out,
        "{INDENT}std::vector<std::unique_ptr<harc_rt::ThreadScheduler>> actor_schedulers;"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}std::vector<std::unique_ptr<harc_rt::ThreadSlot>> actor_slots;"
    )
    .ok();
    writeln!(out, "{INDENT}harc_rt::ThreadSlot run_slot;").ok();
    let runtime_owner = crate::ir::passes::runtime_cells::RuntimeCellOwner::Runtime;
    let mut rng = false;
    let mut callbacks = BTreeSet::new();
    for cell in plan.runtime_cells.for_owner(&runtime_owner) {
        use crate::ir::passes::runtime_cells::{CallbackRegistryKind, RuntimeCellKind};
        if cell.registration()
            != crate::ir::passes::runtime_cells::RuntimeCellRegistrationPhase::RuntimeSetup
        {
            return Err(EmitError(format!(
                "TB-IR common runtime cell `{}` has non-runtime registration phase {:?}",
                cell.symbol(),
                cell.registration()
            )));
        }
        match cell.kind() {
            RuntimeCellKind::Rng => {
                if cell.initializer()
                    != crate::ir::passes::runtime_cells::RuntimeCellInitializer::SeedFromEnvironment
                {
                    return Err(EmitError(
                        "TB-IR common RNG cell must be seeded from the environment".into(),
                    ));
                }
                writeln!(out, "{INDENT}harc_rt::random::HarcRng rng;").ok();
                rng = true;
            }
            RuntimeCellKind::CallbackRegistry(registry) => {
                let field = match registry {
                    CallbackRegistryKind::Checker => "_checkers",
                    CallbackRegistryKind::PostEval => "_post_eval_services",
                    CallbackRegistryKind::AutomaticCoverageReport => "_auto_cov_reports",
                };
                let init = super::runtime::runtime_cell_initializer(cell)?;
                writeln!(
                    out,
                    "{INDENT}std::vector<std::function<void()>> {field}{init};"
                )
                .ok();
                callbacks.insert(*registry);
            }
            RuntimeCellKind::Solver => {
                plan.randomize.as_ref().ok_or_else(|| {
                    EmitError(
                        "TB-IR common layout has solver runtime state without a randomization plan"
                            .into(),
                    )
                })?;
            }
            other => {
                return Err(EmitError(format!(
                    "TB-IR common layout runtime owner has incompatible cell {other:?}"
                )));
            }
        }
    }
    let expected_callbacks = BTreeSet::from([
        crate::ir::passes::runtime_cells::CallbackRegistryKind::Checker,
        crate::ir::passes::runtime_cells::CallbackRegistryKind::PostEval,
        crate::ir::passes::runtime_cells::CallbackRegistryKind::AutomaticCoverageReport,
    ]);
    if !rng || callbacks != expected_callbacks {
        return Err(EmitError(
            "TB-IR common layout runtime-cell plan is missing the RNG or callback registries"
                .into(),
        ));
    }
    if let Some(randomize) = &plan.randomize {
        for cell in plan.runtime_cells.cells().iter().filter(|cell| {
            matches!(
                cell.owner(),
                crate::ir::passes::runtime_cells::RuntimeCellOwner::Callable { .. }
            )
        }) {
            let crate::ir::passes::runtime_cells::RuntimeCellKind::ConstraintState { site } =
                cell.kind()
            else {
                continue;
            };
            let state = randomize.site_states.get(site.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common shared constraint cell c{} has no emission state",
                    site.0
                ))
            })?;
            super::runtime::randomize_site_state_field(
                &mut out,
                1,
                &format!("_harc_randomize_c{}", site.0),
                state,
            );
        }
    }
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    if plan
        .randomize
        .as_ref()
        .is_some_and(|randomize| !randomize.runtime_table.problems.is_empty())
    {
        writeln!(
            out,
            "harc_rt::random::HarcRandomizeCall harc_prepare_randomize_call("
        )
        .ok();
        writeln!(
            out,
            "{INDENT}harc_rt::random::HarcRuntimeCallSite& call_site,"
        )
        .ok();
        writeln!(out, "{INDENT}harc_rt::random::harc_problem_id problem_id,").ok();
        writeln!(out, "{INDENT}harc_rt::random::harc_seed global_seed,").ok();
        writeln!(out, "{INDENT}harc_rt::random::harc_seed fallback_seed);").ok();
        writeln!(out).ok();
    }
    writeln!(
        out,
        "void harc_eval_clocks_until(HarcTestContext& ctx, long long target_ps);"
    )
    .ok();
    writeln!(
        out,
        "void harc_wait_clock_cycles(HarcTestContext& ctx, const char* clock, long long cycles);"
    )
    .ok();
    writeln!(out, "void harc_tseq_tick(HarcTestContext& ctx);").ok();
    writeln!(out).ok();
    for callable in &plan.shared_callables {
        let function = prog.function(callable.function);
        match callable.kind {
            CommonCallableKind::Helper => {
                func::emit_common_helper_declaration(&mut out, prog, function)?;
            }
            CommonCallableKind::Tseq { needs_context } => {
                func::emit_common_tseq_declaration(&mut out, prog, function, needs_context)?;
            }
            CommonCallableKind::ComponentMethod => {
                let CallableOwner::Component { component, member } = callable.owner else {
                    return Err(EmitError(format!(
                        "TB-IR common layout callable fn{} `{}` has a non-component owner",
                        function.id.0, function.name
                    )));
                };
                match member {
                    ComponentCallableMember::Method(_) => {
                        let method = component_method_schema(prog, function, component)?;
                        func::emit_common_component_method_declaration(
                            &mut out,
                            prog,
                            &prog.components[component.index()],
                            method,
                            callable.bus_adapter.as_ref(),
                        )?;
                    }
                    _ => {
                        let symbol = component_callable_symbol(prog, component, member)?;
                        func::emit_common_component_lifecycle_declaration(
                            &mut out,
                            prog,
                            &prog.components[component.index()],
                            function,
                            &symbol,
                            callable.bus_adapter.as_ref(),
                        )?;
                    }
                }
            }
            CommonCallableKind::TestbenchMethod => {
                func::emit_common_testbench_method_declaration(
                    &mut out,
                    prog,
                    function,
                    callable.bus_adapter.as_ref(),
                )?;
            }
            CommonCallableKind::TransactorMethod => {
                let CallableOwner::Transactor { transactor, member } = callable.owner else {
                    return Err(EmitError(format!(
                        "TB-IR common layout callable fn{} `{}` has a non-transactor owner",
                        function.id.0, function.name
                    )));
                };
                if !matches!(
                    member,
                    crate::ir::passes::callable_placement::TransactorCallableMember::Method(_)
                ) {
                    return Err(unsupported_unplaced_function(prog, function));
                }
                let schema = prog.transactor(transactor);
                let method = transactor_method_schema(prog, function, transactor)?;
                func::emit_common_transactor_method_declaration(
                    &mut out,
                    prog,
                    transactor,
                    schema,
                    method,
                    callable.bus_adapter.as_ref(),
                )?;
            }
            CommonCallableKind::Run
            | CommonCallableKind::Check
            | CommonCallableKind::SamplerAuto
            | CommonCallableKind::TestHook
            | CommonCallableKind::TestbenchLifecycle => {
                return Err(EmitError(format!(
                    "TB-IR common layout callable fn{} `{}` is marked common with non-common kind {:?}",
                    function.id.0, function.name, callable.kind
                )));
            }
        }
    }
    if !plan.shared_callables.is_empty() {
        writeln!(out).ok();
    }
    let outofline_lifecycle = super::shareable_lifecycle_map(prog);
    super::emit_shared_lifecycle_prototypes(&mut out, prog, &outofline_lifecycle);
    if !outofline_lifecycle.is_empty() {
        writeln!(out).ok();
    }
    writeln!(
        out,
        "using HarcTestStateCreate = void* (*)(HarcTestContext&);"
    )
    .ok();
    writeln!(
        out,
        "using HarcTestBody = harc_rt::HarcThread (*)(HarcTestContext&, harc_rt::ThreadSlot*, void*);"
    )
    .ok();
    writeln!(
        out,
        "using HarcTestClockConfigure = void (*)(HarcTestContext&);"
    )
    .ok();
    writeln!(out, "using HarcTestStateDestroy = void (*)(void*);").ok();
    writeln!(
        out,
        "using HarcTestStateReport = void (*)(HarcTestContext&, void*);"
    )
    .ok();
    writeln!(out, "struct HarcTestRunDescriptor {{").ok();
    writeln!(out, "{INDENT}const char* name;").ok();
    writeln!(out, "{INDENT}HarcTestClockConfigure configure_clocks;").ok();
    writeln!(out, "{INDENT}HarcTestStateCreate create_state;").ok();
    writeln!(out, "{INDENT}HarcTestBody body;").ok();
    writeln!(out, "{INDENT}HarcTestStateReport report_state;").ok();
    writeln!(out, "{INDENT}HarcTestStateDestroy destroy_state;").ok();
    writeln!(out, "}};").ok();
    writeln!(
        out,
        "int harc_run_test(const HarcTestRunDescriptor& test, int argc, char** argv);"
    )
    .ok();
    writeln!(out).ok();
    writeln!(out, "struct HarcTestDescriptor {{").ok();
    writeln!(out, "{INDENT}const char* name;").ok();
    writeln!(out, "{INDENT}int (*run)(int argc, char** argv);").ok();
    writeln!(out, "{INDENT}const char* abi_anchor;").ok();
    writeln!(out, "}};").ok();
    writeln!(out).ok();
    writeln!(
        out,
        "extern const char {}[];",
        common_artifacts::ABI_ANCHOR_PLACEHOLDER
    )
    .ok();
    writeln!(out).ok();
    writeln!(out, "// === iface-end ===").ok();
    Ok(out)
}

fn render_common_runtime_template(plan: &CommonCppPlan) -> Result<String, EmitError> {
    let prog = plan.program();
    let profile = &plan.runtime_build_profile;
    let mut out = String::new();
    let dut_type = plan.dut_type();
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(out, "// TB-IR common-object runtime implementation.").ok();
    writeln!(out, "// harc build-profile: {profile}").ok();
    writeln!(
        out,
        "// harc dut-access-profile: {}",
        plan.runtime_dut_access.digest()
    )
    .ok();
    writeln!(out).ok();
    writeln!(
        out,
        "#include \"{}suite_api.hpp\"",
        plan.artifact_plan.prefix()
    )
    .ok();
    if plan.runtime_dut_access.uses_probe() {
        writeln!(out, "#include \"V{}___024root.h\"", plan.dut_type).ok();
    }
    writeln!(out).ok();
    writeln!(
        out,
        "extern const char {}[];",
        common_artifacts::ABI_ANCHOR_PLACEHOLDER
    )
    .ok();
    writeln!(out).ok();
    if let Some(randomize) = &plan.randomize {
        if !randomize.runtime_table.problems.is_empty() {
            out.push_str(
                &randomize
                    .runtime_table
                    .render_cpp_private_descriptors("_harc_runtime_random_problem_table"),
            );
            writeln!(
                out,
                "harc_rt::random::HarcRandomizeCall harc_prepare_randomize_call("
            )
            .ok();
            writeln!(
                out,
                "{INDENT}harc_rt::random::HarcRuntimeCallSite& call_site,"
            )
            .ok();
            writeln!(out, "{INDENT}harc_rt::random::harc_problem_id problem_id,").ok();
            writeln!(out, "{INDENT}harc_rt::random::harc_seed global_seed,").ok();
            writeln!(out, "{INDENT}harc_rt::random::harc_seed fallback_seed) {{").ok();
            writeln!(
                out,
                "{INDENT}return harc_rt::random::harc_prepare_randomize_call("
            )
            .ok();
            writeln!(out, "{INDENT}{INDENT}_harc_runtime_random_problem_table,").ok();
            writeln!(out, "{INDENT}{INDENT}&call_site,").ok();
            writeln!(out, "{INDENT}{INDENT}1,").ok();
            writeln!(out, "{INDENT}{INDENT}problem_id,").ok();
            writeln!(out, "{INDENT}{INDENT}global_seed,").ok();
            writeln!(out, "{INDENT}{INDENT}fallback_seed);").ok();
            writeln!(out, "}}").ok();
            writeln!(out).ok();
        }
    }
    for shared in &plan.shared_types {
        match shared.kind {
            CommonSharedTypeKind::Record(record) => {
                super::common_record_definitions(&mut out, &prog.records[record.index()]);
            }
            CommonSharedTypeKind::Covergroup(covgroup) => {
                super::covergroup::common_covgroup_definition(
                    &mut out,
                    &prog.covgroups[covgroup.index()],
                );
            }
            CommonSharedTypeKind::Scoreboard(_)
            | CommonSharedTypeKind::Component(_)
            | CommonSharedTypeKind::TransactorState(_)
            | CommonSharedTypeKind::Testbench(_) => {}
        }
    }
    emit_common_tseq_tick(&mut out, plan);
    let outofline_lifecycle = super::shareable_lifecycle_map(prog);
    super::emit_shared_lifecycle_defs(
        &mut out,
        prog,
        &plan.vec_lane_widths,
        plan.randomize_snippets(),
        plan.dut_type(),
        Some(&plan.dut_access),
        &outofline_lifecycle,
        /* common_context */ true,
        /* static_linkage */ false,
        None,
    )?;
    for callable in &plan.shared_callables {
        let function = prog.function(callable.function);
        match callable.kind {
            CommonCallableKind::Helper => {
                func::emit_common_helper_function(&mut out, prog, function)?;
            }
            CommonCallableKind::Tseq { needs_context } => {
                func::emit_common_tseq_function(
                    &mut out,
                    prog,
                    function,
                    &prog.records,
                    plan.randomize_snippets(),
                    needs_context,
                    &plan.contextual_tseqs,
                )?;
            }
            CommonCallableKind::ComponentMethod => {
                let CallableOwner::Component { component, member } = callable.owner else {
                    return Err(EmitError(format!(
                        "TB-IR common layout callable fn{} `{}` has a non-component owner",
                        function.id.0, function.name
                    )));
                };
                match member {
                    ComponentCallableMember::Method(_) => {
                        let method = component_method_schema(prog, function, component)?;
                        func::emit_common_component_method_function(
                            &mut out,
                            prog,
                            &prog.components[component.index()],
                            method,
                            &callable.bus_bindings,
                            callable.bus_adapter.as_ref(),
                            &plan.bus_adapters,
                            plan.dut_type(),
                            &plan.dut_access,
                            plan.randomize_snippets(),
                            &plan.contextual_tseqs,
                        )?;
                    }
                    _ => {
                        let symbol = component_callable_symbol(prog, component, member)?;
                        func::emit_common_component_lifecycle_function(
                            &mut out,
                            prog,
                            &prog.components[component.index()],
                            function,
                            &symbol,
                            &callable.bus_bindings,
                            callable.bus_adapter.as_ref(),
                            &plan.bus_adapters,
                            plan.dut_type(),
                            &plan.dut_access,
                            plan.randomize_snippets(),
                            &plan.contextual_tseqs,
                        )?;
                    }
                }
            }
            CommonCallableKind::TestbenchMethod => {
                func::emit_common_testbench_method_function(
                    &mut out,
                    prog,
                    function,
                    &callable.bus_bindings,
                    callable.bus_adapter.as_ref(),
                    &plan.bus_adapters,
                    plan.dut_type(),
                    &plan.dut_access,
                    plan.randomize_snippets(),
                    &plan.contextual_tseqs,
                )?;
            }
            CommonCallableKind::TransactorMethod => {
                let CallableOwner::Transactor { transactor, member } = callable.owner else {
                    return Err(EmitError(format!(
                        "TB-IR common layout callable fn{} `{}` has a non-transactor owner",
                        function.id.0, function.name
                    )));
                };
                if !matches!(
                    member,
                    crate::ir::passes::callable_placement::TransactorCallableMember::Method(_)
                ) {
                    return Err(unsupported_unplaced_function(prog, function));
                }
                let schema = prog.transactor(transactor);
                let method = transactor_method_schema(prog, function, transactor)?;
                func::emit_common_transactor_method_function(
                    &mut out,
                    prog,
                    transactor,
                    schema,
                    method,
                    &callable.bus_bindings,
                    callable.bus_adapter.as_ref(),
                    &plan.bus_adapters,
                    plan.dut_type(),
                    &plan.dut_access,
                    plan.randomize_snippets(),
                    &plan.contextual_tseqs,
                )?;
            }
            CommonCallableKind::Run
            | CommonCallableKind::Check
            | CommonCallableKind::SamplerAuto
            | CommonCallableKind::TestHook
            | CommonCallableKind::TestbenchLifecycle => {
                return Err(EmitError(format!(
                    "TB-IR common layout callable fn{} `{}` is marked common with non-common kind {:?}",
                    function.id.0, function.name, callable.kind
                )));
            }
        }
        writeln!(out).ok();
    }
    writeln!(
        out,
        "int harc_run_test(const HarcTestRunDescriptor& test, int argc, char** argv) {{"
    )
    .ok();
    writeln!(out, "{INDENT}HarcTestContext ctx;").ok();
    writeln!(out, "{INDENT}ctx.verilated.commandArgs(argc, argv);").ok();
    writeln!(out, "{INDENT}ctx.dut = new V{dut_type}(&ctx.verilated);").ok();
    writeln!(out, "#if HARC_TRACE_ENABLED").ok();
    writeln!(out, "{INDENT}Verilated::traceEverOn(true);").ok();
    writeln!(out, "{INDENT}ctx.tfp = new HarcTraceC;").ok();
    writeln!(
        out,
        "{INDENT}ctx.wave_path = harc_rt::log::harc_open_wave_trace(ctx.dut, ctx.tfp, harc_rt::log::harc_wave_default_name());"
    )
    .ok();
    writeln!(out, "#endif").ok();
    writeln!(out, "{INDENT}test.configure_clocks(ctx);").ok();
    writeln!(out, "{INDENT}ctx.rng.seed_from_env();").ok();
    writeln!(
        out,
        "{INDENT}harc_rt::trace::harc_start_trace(ctx.trace, ctx.rng.state, \"{}\", test.name, ctx.cycle_count);",
        super::expr::escape_c(dut_type)
    )
    .ok();
    writeln!(out, "{INDENT}char seed_message[64];").ok();
    writeln!(
        out,
        "{INDENT}std::snprintf(seed_message, sizeof(seed_message), \"seed=%llu\", (long long)ctx.rng.state);"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}harc_rt::log::harc_log_line(ctx.log_ctx.sim_log, &ctx.trace, ctx.cycle_count, \"INFO\", seed_message);"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}HARC_RT_LOG_WAVE_FILE(ctx.log_ctx.sim_log, ctx.wave_path);"
    )
    .ok();
    writeln!(out).ok();
    writeln!(
        out,
        "{INDENT}harc_rt::HarcQueueFatalScope queue_fatal_scope([&]() {{"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}harc_rt::log::harc_log_line(ctx.log_ctx.sim_log, &ctx.trace, ctx.cycle_count, \"FATAL\", \"queue front/pop on an empty queue -- guard it with .empty()/.size(), or wait until the producer has pushed\");").ok();
    writeln!(out, "{INDENT}{INDENT}ctx.errors++;").ok();
    writeln!(out, "{INDENT}{INDENT}ctx.fatal = true;").ok();
    writeln!(out, "{INDENT}}});").ok();
    writeln!(out).ok();
    writeln!(out, "{INDENT}ctx.scheduler.slots.push_back(&ctx.run_slot);").ok();
    writeln!(out, "{INDENT}void* run_state = test.create_state(ctx);").ok();
    writeln!(
        out,
        "{INDENT}ctx.run_slot.thread = test.body(ctx, &ctx.run_slot, run_state);"
    )
    .ok();
    if plan.mt {
        writeln!(out, "{INDENT}ctx.advance_actor_schedulers = [&]() {{ for (auto& scheduler : ctx.actor_schedulers) scheduler->tick(); }};").ok();
    } else {
        writeln!(out, "{INDENT}ctx.advance_actor_schedulers = [&]() {{ ctx.scheduler.tick_except(&ctx.run_slot); }};").ok();
    }
    writeln!(out, "{INDENT}ctx.scheduler.bootstrap();").ok();
    if !plan.mt {
        writeln!(out, "{INDENT}for (size_t slot_index = 0; slot_index < ctx.scheduler.slots.size(); ++slot_index) {{").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}if (ctx.scheduler.slots[slot_index] != &ctx.run_slot) continue;"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}ctx.scheduler.slots.erase(ctx.scheduler.slots.begin() + slot_index);"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}ctx.scheduler.slots.push_back(&ctx.run_slot);"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}break;").ok();
        writeln!(out, "{INDENT}}}").ok();
    }
    writeln!(
        out,
        "{INDENT}for (auto& scheduler : ctx.actor_schedulers) scheduler->bootstrap();"
    )
    .ok();
    if plan.mt {
        writeln!(out, "{INDENT}std::atomic<bool> worker_shutdown{{false}};").ok();
        writeln!(out, "{INDENT}std::atomic<size_t> worker_turn{{0}};").ok();
        writeln!(
            out,
            "{INDENT}std::unique_ptr<harc_rt::Barrier> worker_start;"
        )
        .ok();
        writeln!(out, "{INDENT}std::unique_ptr<harc_rt::Barrier> worker_end;").ok();
        writeln!(out, "{INDENT}std::vector<std::thread> workers;").ok();
        writeln!(out, "{INDENT}if (!ctx.actor_schedulers.empty()) {{").ok();
        writeln!(out, "{INDENT}{INDENT}worker_start = std::make_unique<harc_rt::Barrier>((uint32_t)ctx.actor_schedulers.size() + 1);").ok();
        writeln!(out, "{INDENT}{INDENT}worker_end = std::make_unique<harc_rt::Barrier>((uint32_t)ctx.actor_schedulers.size() + 1);").ok();
        writeln!(out, "{INDENT}{INDENT}for (size_t worker_index = 0; worker_index < ctx.actor_schedulers.size(); ++worker_index) {{").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}auto* scheduler = ctx.actor_schedulers[worker_index].get();"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}workers.emplace_back([&, scheduler, worker_index]() {{"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{INDENT}while (true) {{").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}worker_start->wait();"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}if (worker_shutdown.load(std::memory_order_acquire)) break;").ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}while (worker_turn.load(std::memory_order_acquire) != worker_index) std::this_thread::yield();").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}scheduler->tick();"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}worker_turn.fetch_add(1, std::memory_order_release);").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}{INDENT}{INDENT}{INDENT}worker_end->wait();"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}{INDENT}{INDENT}}});").ok();
        writeln!(out, "{INDENT}{INDENT}}}").ok();
        writeln!(out, "{INDENT}}}").ok();
        writeln!(out, "{INDENT}if (worker_start) ctx.advance_actor_schedulers = [&]() {{ worker_turn.store(0, std::memory_order_release); worker_start->wait(); worker_end->wait(); }};").ok();
    }
    writeln!(out, "{INDENT}if (ctx.clocks.empty()) {{").ok();
    writeln!(out, "{INDENT}{INDENT}harc_eval_clockless_edge(ctx, 0);").ok();
    writeln!(out, "{INDENT}{INDENT}harc_trace_clockless_edge(ctx, 0);").ok();
    writeln!(out, "{INDENT}}} else {{").ok();
    writeln!(out, "{INDENT}{INDENT}ctx.dut->eval();").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}ctx.trace.set_timing((uint64_t)ctx.now_ps, \"\", 0);"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}{INDENT}HARC_RT_DUMP_WAVE_TRACE(ctx.tfp, (uint64_t)ctx.now_ps);"
    )
    .ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(
        out,
        "{INDENT}while (!ctx.run_complete && ctx.run_slot.kind != harc_rt::WaitKind::Done && !ctx.fatal) {{"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}if (ctx.clocks.empty()) {{").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}harc_eval_clockless_edge(ctx, 1);"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}harc_trace_clockless_edge(ctx, (uint64_t)(ctx.cycle_count + 1));"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}{INDENT}ctx.cycle_count++;").ok();
    super::runtime::post_eval_services(&mut out, 3, "ctx._post_eval_services", "ctx.dut");
    writeln!(out, "{INDENT}{INDENT}{INDENT}if (!ctx._post_eval_services.empty()) harc_trace_clockless_edge(ctx, (uint64_t)ctx.cycle_count);").ok();
    writeln!(out, "{INDENT}{INDENT}{INDENT}ctx.scheduler.tick();").ok();
    if plan.mt {
        writeln!(out, "{INDENT}{INDENT}{INDENT}if (worker_start && !ctx.run_complete && !ctx.fatal) {{ worker_turn.store(0, std::memory_order_release); worker_start->wait(); worker_end->wait(); }}").ok();
    }
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}harc_eval_clockless_edge(ctx, 0);"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}harc_trace_clockless_edge(ctx, (uint64_t)ctx.cycle_count);"
    )
    .ok();
    super::runtime::checker_callbacks(&mut out, 3, "ctx._checkers");
    writeln!(out, "{INDENT}{INDENT}}} else {{").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}long long target = ctx.now_ps + ctx.clocks[0].half_period_ps * 2;"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}harc_eval_clocks_until(ctx, target);"
    )
    .ok();
    if plan.mt {
        writeln!(out, "{INDENT}{INDENT}{INDENT}ctx.actor_tick_due = true;").ok();
    }
    writeln!(out, "{INDENT}{INDENT}{INDENT}ctx.scheduler.tick();").ok();
    if plan.mt {
        writeln!(out, "{INDENT}{INDENT}{INDENT}if (ctx.actor_tick_due && worker_start && !ctx.run_complete && !ctx.fatal) {{ ctx.advance_actor_schedulers(); ctx.actor_tick_due = false; }}").ok();
    }
    super::runtime::checker_callbacks(&mut out, 3, "ctx._checkers");
    writeln!(out, "{INDENT}{INDENT}}}").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out).ok();
    if plan.mt {
        writeln!(out, "{INDENT}if (worker_start) {{").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}worker_shutdown.store(true, std::memory_order_release);"
        )
        .ok();
        writeln!(out, "{INDENT}{INDENT}worker_start->wait();").ok();
        writeln!(
            out,
            "{INDENT}{INDENT}for (auto& worker : workers) worker.join();"
        )
        .ok();
        writeln!(out, "{INDENT}}}").ok();
    }
    super::runtime::automatic_coverage_reports(&mut out, 1, "ctx._auto_cov_reports");
    writeln!(
        out,
        "{INDENT}if (test.report_state) test.report_state(ctx, run_state);"
    )
    .ok();
    super::runtime::clear_run_callbacks(
        &mut out,
        1,
        "ctx._auto_cov_reports",
        "ctx._post_eval_services",
        "ctx._checkers",
    );
    writeln!(out, "{INDENT}for (auto& scheduler : ctx.actor_schedulers) harc_rt::harc_destroy_scheduler_threads(*scheduler);").ok();
    writeln!(out, "{INDENT}ctx.actor_schedulers.clear();").ok();
    writeln!(out, "{INDENT}for (auto& slot : ctx.actor_slots) {{ slot->pred = {{}}; slot->thread.destroy(); }}").ok();
    super::runtime::destroy_scheduler_threads(&mut out, 1, ["ctx.scheduler"]);
    writeln!(out, "{INDENT}ctx.actor_slots.clear();").ok();
    writeln!(out, "{INDENT}test.destroy_state(run_state);").ok();
    writeln!(out, "{INDENT}run_state = nullptr;").ok();
    writeln!(out, "{INDENT}ctx.dut->final();").ok();
    writeln!(
        out,
        "{INDENT}HARC_RT_WRITE_COVERAGE(ctx.verilated.coveragep());"
    )
    .ok();
    writeln!(out, "{INDENT}HARC_RT_CLOSE_WAVE_TRACE(ctx.tfp);").ok();
    writeln!(out, "{INDENT}delete ctx.dut;").ok();
    writeln!(out, "{INDENT}ctx.dut = nullptr;").ok();
    writeln!(
        out,
        "{INDENT}return harc_rt::log::harc_finish_sim_run(ctx.log_ctx, ctx.trace, ctx.cycle_count, ctx.errors);"
    )
    .ok();
    writeln!(out, "}}").ok();
    Ok(out)
}

fn emit_common_tseq_tick(out: &mut String, plan: &CommonCppPlan) {
    writeln!(
        out,
        "static void harc_eval_clockless_edge(HarcTestContext& ctx, int level) {{"
    )
    .ok();
    if plan
        .dut_access
        .interface()
        .port_by_physical_name("clk")
        .is_some()
    {
        writeln!(out, "{INDENT}ctx.dut->clk = level;").ok();
    } else {
        writeln!(out, "{INDENT}(void)level;").ok();
    }
    writeln!(out, "{INDENT}ctx.dut->eval();").ok();
    writeln!(out, "}}").ok();
    writeln!(out).ok();
    writeln!(
        out,
        "static void harc_trace_clockless_edge(HarcTestContext& ctx, uint64_t clock_cycle) {{"
    )
    .ok();
    writeln!(out, "{INDENT}uint64_t time = ctx.trace_time++;").ok();
    writeln!(
        out,
        "{INDENT}ctx.trace.set_timing(time, \"clk\", clock_cycle);"
    )
    .ok();
    writeln!(out, "{INDENT}ctx.now_ps = (long long)ctx.trace_time;").ok();
    writeln!(out, "{INDENT}HARC_RT_DUMP_WAVE_TRACE(ctx.tfp, time);").ok();
    writeln!(out, "}}").ok();
    writeln!(out).ok();
    writeln!(
        out,
        "void harc_eval_clocks_until(HarcTestContext& ctx, long long target_ps) {{"
    )
    .ok();
    writeln!(out, "{INDENT}if (ctx.clocks.empty()) {{").ok();
    writeln!(out, "{INDENT}{INDENT}if (target_ps <= ctx.now_ps) return;").ok();
    writeln!(out, "{INDENT}{INDENT}ctx.now_ps = target_ps;").ok();
    writeln!(out, "{INDENT}{INDENT}ctx.dut->eval();").ok();
    writeln!(out, "{INDENT}{INDENT}ctx.trace.set_timing((uint64_t)ctx.now_ps, \"\", (uint64_t)ctx.cycle_count);").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}HARC_RT_DUMP_WAVE_TRACE(ctx.tfp, (uint64_t)ctx.now_ps);"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}if (ctx.trace_time <= (uint64_t)ctx.now_ps) ctx.trace_time = (uint64_t)ctx.now_ps + 1;").ok();
    writeln!(out, "{INDENT}{INDENT}return;").ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "{INDENT}while (ctx.now_ps < target_ps) {{").ok();
    writeln!(out, "{INDENT}{INDENT}long long next = target_ps;").ok();
    writeln!(out, "{INDENT}{INDENT}for (auto& clock : ctx.clocks) if (clock.next_edge_ps < next) next = clock.next_edge_ps;").ok();
    writeln!(out, "{INDENT}{INDENT}ctx.now_ps = next;").ok();
    writeln!(out, "{INDENT}{INDENT}bool primary_rising = false;").ok();
    writeln!(out, "{INDENT}{INDENT}const char* last_edge_clock = \"\";").ok();
    writeln!(out, "{INDENT}{INDENT}uint64_t last_edge_cycle = 0;").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}for (size_t i = 0; i < ctx.clocks.size(); ++i) {{"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}{INDENT}auto& clock = ctx.clocks[i];").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}if (clock.next_edge_ps != ctx.now_ps) continue;"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}{INDENT}clock.level = !clock.level;").ok();
    writeln!(out, "{INDENT}{INDENT}{INDENT}clock.drive(clock.level);").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}clock.next_edge_ps += clock.half_period_ps;"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}if (clock.level == 1) clock.rising_count++;"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}{INDENT}last_edge_clock = clock.name;").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}{INDENT}last_edge_cycle = (uint64_t)clock.rising_count;"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}{INDENT}if (i == 0 && clock.level == 1) {{ ctx.cycle_count++; primary_rising = true; }}").ok();
    writeln!(out, "{INDENT}{INDENT}}}").ok();
    writeln!(out, "{INDENT}{INDENT}ctx.dut->eval();").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}ctx.trace.set_timing((uint64_t)ctx.now_ps, last_edge_clock, last_edge_cycle);"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}if (primary_rising) {{").ok();
    super::runtime::post_eval_services(out, 3, "ctx._post_eval_services", "ctx.dut");
    writeln!(out, "{INDENT}{INDENT}}}").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}HARC_RT_DUMP_WAVE_TRACE(ctx.tfp, (uint64_t)ctx.now_ps);"
    )
    .ok();
    writeln!(out, "{INDENT}}}").ok();
    writeln!(out, "}}").ok();
    writeln!(out).ok();
    writeln!(
        out,
        "void harc_wait_clock_cycles(HarcTestContext& ctx, const char* name, long long cycles) {{"
    )
    .ok();
    writeln!(out, "{INDENT}if (cycles <= 0) return;").ok();
    writeln!(out, "{INDENT}size_t index = ctx.clocks.size();").ok();
    writeln!(out, "{INDENT}for (size_t i = 0; i < ctx.clocks.size(); ++i) if (std::strcmp(ctx.clocks[i].name, name) == 0) {{ index = i; break; }}").ok();
    writeln!(
        out,
        "{INDENT}if (index == ctx.clocks.size()) {{ ctx.errors++; ctx.fatal = true; return; }}"
    )
    .ok();
    writeln!(
        out,
        "{INDENT}long long target = ctx.clocks[index].rising_count + cycles;"
    )
    .ok();
    writeln!(out, "{INDENT}if (ctx.actor_tick_due) {{ ctx.advance_actor_schedulers(); ctx.actor_tick_due = false; }}").ok();
    writeln!(
        out,
        "{INDENT}while (ctx.clocks[index].rising_count < target && !ctx.fatal) {{"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}int before_cycle = ctx.cycle_count;").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}long long next = ctx.clocks[0].next_edge_ps;"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}for (auto& clock : ctx.clocks) if (clock.next_edge_ps < next) next = clock.next_edge_ps;").ok();
    writeln!(out, "{INDENT}{INDENT}harc_eval_clocks_until(ctx, next);").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}if (ctx.cycle_count != before_cycle) ctx.advance_actor_schedulers();"
    )
    .ok();
    writeln!(out, "{INDENT}}}").ok();
    super::runtime::checker_callbacks(out, 1, "ctx._checkers");
    writeln!(out, "}}").ok();
    writeln!(out).ok();
    writeln!(out, "void harc_tseq_tick(HarcTestContext& ctx) {{").ok();
    writeln!(out, "{INDENT}if (ctx.clocks.empty()) {{").ok();
    writeln!(out, "{INDENT}{INDENT}harc_eval_clockless_edge(ctx, 0);").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}harc_trace_clockless_edge(ctx, (uint64_t)ctx.cycle_count);"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}harc_eval_clockless_edge(ctx, 1);").ok();
    writeln!(
        out,
        "{INDENT}{INDENT}harc_trace_clockless_edge(ctx, (uint64_t)(ctx.cycle_count + 1));"
    )
    .ok();
    writeln!(out, "{INDENT}{INDENT}ctx.cycle_count++;").ok();
    super::runtime::post_eval_services(out, 2, "ctx._post_eval_services", "ctx.dut");
    writeln!(out, "{INDENT}}} else {{").ok();
    writeln!(out, "{INDENT}{INDENT}harc_eval_clocks_until(ctx, ctx.now_ps + ctx.clocks[0].half_period_ps * 2);").ok();
    writeln!(out, "{INDENT}}}").ok();
    super::runtime::checker_callbacks(out, 1, "ctx._checkers");
    writeln!(out, "}}").ok();
    writeln!(out).ok();
}

fn fresh_capsule_identifier(base: &str, used: &mut HashSet<String>) -> String {
    let mut name = base.to_string();
    while used.contains(&name) {
        name = format!("_u_{name}");
    }
    used.insert(name.clone());
    name
}

fn add_component_registration_access_sites(
    prog: &TbProgram,
    component: ComponentId,
    mode: Option<ir::ComponentInstanceMode>,
    sites: &mut BTreeSet<DutAccessSite>,
) -> Result<(), EmitError> {
    let schema = prog.components.get(component.index()).ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout access profile references missing component c{}",
            component.0
        ))
    })?;
    for handler in &schema.periodic_handlers {
        if ir::component_mode_includes_activation(mode, handler.activation) {
            sites.insert(DutAccessSite::ComponentLifecycle {
                component,
                function: handler.function,
                activation: handler.activation,
            });
        }
    }
    for handler in &schema.cycle_handlers {
        if ir::component_mode_includes_activation(mode, handler.activation) {
            sites.insert(DutAccessSite::ComponentLifecycle {
                component,
                function: handler.function,
                activation: handler.activation,
            });
        }
    }
    if let Some(watchdog) = &schema.watchdog {
        if ir::component_mode_includes_activation(mode, watchdog.activation) {
            sites.insert(DutAccessSite::ComponentLifecycle {
                component,
                function: watchdog.function,
                activation: watchdog.activation,
            });
        }
    }
    for field in &schema.fields {
        let ir::ComponentFieldKind::Sub {
            component: child, ..
        } = &field.kind
        else {
            continue;
        };
        let child_mode = ir::resolve_component_path_mode(
            &prog.components,
            component,
            mode,
            std::slice::from_ref(&field.name),
        )
        .map_err(|detail| {
            EmitError(format!(
                "TB-IR common layout cannot resolve component access profile `{}.{}`: {detail}",
                schema.name, field.name
            ))
        })?
        .effective_mode;
        add_component_registration_access_sites(prog, *child, child_mode, sites)?;
    }
    Ok(())
}

fn capsule_dut_access_sites(
    prog: &TbProgram,
    capsule: &CommonCapsulePlan,
) -> Result<BTreeSet<DutAccessSite>, EmitError> {
    let mut sites = BTreeSet::new();
    for body in &capsule.test_bodies {
        let test = &prog.tests[body.test_index];
        let tb = prog.testbench(test.testbench);
        sites.insert(DutAccessSite::Function(body.run));
        if let Some(check) = body.check {
            sites.insert(DutAccessSite::Function(check));
        }
        sites.extend(body.test_hooks.iter().copied().map(DutAccessSite::Function));
        sites.extend(
            tb.cycle_services
                .iter()
                .map(|service| DutAccessSite::TestbenchService {
                    testbench: test.testbench,
                    function: service.function,
                }),
        );
        sites.extend(
            prog.functions
                .iter()
                .filter(|function| {
                    function.owner == Some(test.testbench)
                        && matches!(function.kind, ir::FunctionKind::SamplerAuto { .. })
                })
                .map(|function| DutAccessSite::Function(function.id)),
        );
        for binding in &tb.component_fields {
            add_component_registration_access_sites(
                prog,
                binding.component,
                binding.mode,
                &mut sites,
            )?;
        }
        for actor in &tb.target_tlm_actors {
            let transactor = prog.transactor(actor.transactor);
            sites.extend(
                transactor
                    .target_methods
                    .iter()
                    .filter(|method| {
                        actor.active
                            || !matches!(method.activation, crate::ir::Activation::ActiveOnly)
                    })
                    .map(|method| DutAccessSite::Function(method.function)),
            );
        }
    }
    Ok(sites)
}

fn emit_common_covergroup_registrations(
    out: &mut String,
    prog: &TbProgram,
    test: &ir::TestSchema,
    tb: &ir::TestbenchSchema,
    state: &str,
    testbench_receiver: Option<&str>,
    plan: &CommonCppPlan,
) -> Result<(), EmitError> {
    let samplers = prog
        .functions
        .iter()
        .filter(|function| {
            function.owner == Some(test.testbench)
                && matches!(function.kind, ir::FunctionKind::SamplerAuto { .. })
        })
        .collect::<Vec<_>>();
    if samplers.len() != tb.cov_fields.len() {
        return Err(EmitError(format!(
            "TB-IR common layout test `{}` has {} covergroup field(s) but {} sampler function(s)",
            test.name,
            tb.cov_fields.len(),
            samplers.len()
        )));
    }
    if samplers.is_empty() {
        return Ok(());
    }
    let testbench_receiver = testbench_receiver.ok_or_else(|| {
        EmitError(format!(
            "TB-IR common layout test `{}` has covergroup fields without testbench state",
            test.name
        ))
    })?;
    let no_port_widths = HashMap::new();
    for ((field, covgroup), sampler) in tb.cov_fields.iter().zip(samplers) {
        let ir::FunctionKind::SamplerAuto {
            covgroup: sampler_covgroup,
        } = sampler.kind
        else {
            unreachable!("filtered to SamplerAuto above");
        };
        if sampler_covgroup != *covgroup {
            return Err(EmitError(format!(
                "TB-IR common layout sampler `{}` is bound to cg{} but field `{field}` expects cg{}",
                sampler.name, sampler_covgroup.0, covgroup.0
            )));
        }
        let schema = prog.covgroups.get(covgroup.index()).ok_or_else(|| {
            EmitError(format!(
                "TB-IR common layout covergroup field `{field}` references missing cg{}",
                covgroup.0
            ))
        })?;
        let instance = format!("{testbench_receiver}.{field}");
        match &schema.trigger {
            ir::CovTrigger::PosedgeDutClk => super::covergroup::sampler_registration(
                out,
                prog,
                schema,
                &instance,
                &plan.vec_lane_widths,
                &no_port_widths,
                Some(&plan.dut_access),
            )?,
            ir::CovTrigger::Hook {
                receiver_path,
                method,
                side,
                ..
            } => {
                let [receiver] = receiver_path.as_slice() else {
                    return Err(EmitError(format!(
                        "TB-IR common layout hook-triggered covergroup `{}` has nested receiver path `{}`",
                        schema.name,
                        receiver_path.join(".")
                    )));
                };
                let side_name = match side {
                    crate::ast::HookSide::Pre => "pre",
                    crate::ast::HookSide::Post => "post",
                };
                if let Some(binding) = tb
                    .component_fields
                    .iter()
                    .find(|binding| binding.field == *receiver)
                {
                    let component = prog.components.get(binding.component.index()).ok_or_else(|| {
                        EmitError(format!(
                            "TB-IR common layout hook-triggered covergroup `{}` references missing component c{}",
                            schema.name, binding.component.0
                        ))
                    })?;
                    if let Some(target_method) = component.method(method) {
                        if !ir::component_mode_includes_activation(
                            binding.mode,
                            target_method.activation,
                        ) {
                            return Err(EmitError(format!(
                                "TB-IR common layout hook-triggered covergroup `{}` targets active-only method `{receiver}.{method}` through passive component binding",
                                schema.name
                            )));
                        }
                    }
                }
                let target = tb
                    .transactor_fields
                    .iter()
                    .find_map(|(candidate, transactor)| {
                        if candidate != receiver {
                            return None;
                        }
                        let schema = prog.transactor(*transactor);
                        schema.method(method).and_then(|method_schema| {
                            tb.unbound_state_actors
                                .iter()
                                .find(|actor| {
                                    actor.field == *candidate && actor.transactor == *transactor
                                })
                                .map(|actor| {
                                    (
                                        format!(
                                            "{state}.{}.{}",
                                            actor.storage,
                                            super::runtime::transactor_coverage_hook_field(
                                                schema,
                                                &method_schema.name,
                                                side_name,
                                            )
                                        ),
                                        method_schema.function,
                                        method_schema.param_names.len(),
                                    )
                                })
                        })
                    })
                    .or_else(|| {
                        tb.component_fields.iter().find_map(|binding| {
                            if binding.field != *receiver {
                                return None;
                            }
                            let component = &prog.components[binding.component.index()];
                            component.method(method).map(|method_schema| {
                                (
                                    format!(
                                        "{state}.{}._harc_cov_{}_{}",
                                        binding.field, method_schema.name, side_name
                                    ),
                                    method_schema.function,
                                    method_schema.param_names.len(),
                                )
                            })
                        })
                    })
                    .ok_or_else(|| {
                        EmitError(format!(
                            "TB-IR common layout hook-triggered covergroup `{}` references missing method `{receiver}.{method}`",
                            schema.name
                        ))
                    })?;
                super::covergroup::hook_sampler_registration(
                    out,
                    prog,
                    schema,
                    target.0,
                    target.1,
                    target.2,
                    &instance,
                    &plan.vec_lane_widths,
                    &no_port_widths,
                    Some(&plan.dut_access),
                )?;
            }
        }
    }
    Ok(())
}

fn render_common_capsule_template(
    plan: &CommonCppPlan,
    capsule_index: usize,
) -> Result<String, EmitError> {
    let capsule = plan.capsules.get(capsule_index).ok_or_else(|| {
        EmitError(format!(
            "tbir: common capsule index {capsule_index} is out of range for {} planned capsule(s)",
            plan.capsules.len()
        ))
    })?;
    let profile = &capsule.build_profile;
    let prog = plan.program();
    let outofline_lifecycle = super::shareable_lifecycle_map(prog);
    let artifact_capsule = plan
        .artifact_plan
        .capsules()
        .get(capsule.index)
        .ok_or_else(|| {
            EmitError(format!(
                "tbir: common capsule {} has no matching artifact-plan capsule",
                capsule.index
            ))
        })?;
    let mut out = String::new();
    writeln!(out, "// Auto-generated by harc — do not edit.").ok();
    writeln!(
        out,
        "// TB-IR test capsule: {}.",
        artifact_capsule.test_names().join(", ")
    )
    .ok();
    writeln!(out, "// harc build-profile: {profile}").ok();
    writeln!(
        out,
        "// harc dut-access-profile: {}",
        capsule.dut_access.digest()
    )
    .ok();
    writeln!(out).ok();
    writeln!(
        out,
        "#include \"{}suite_api.hpp\"",
        plan.artifact_plan.prefix()
    )
    .ok();
    if capsule.dut_access.uses_probe() {
        writeln!(out, "#include \"V{}___024root.h\"", plan.dut_type).ok();
    }
    writeln!(out).ok();
    for body in &capsule.test_bodies {
        let test = &prog.tests[body.test_index];
        let tb = prog.testbench(test.testbench);
        let run = prog.function(body.run);
        let artifact_test = &plan.artifact_plan.tests()[body.test_index];
        let stem = artifact_test.symbol_stem();
        let escaped_test_name = super::expr::escape_c(&test.name);
        let clock_topology = plan.clock_topology(body.test_index)?;
        writeln!(
            out,
            "static void harc_configure_clocks_{stem}(HarcTestContext& ctx) {{"
        )
        .ok();
        writeln!(out, "{INDENT}ctx.clocks.clear();").ok();
        writeln!(
            out,
            "{INDENT}ctx.clocks.reserve({});",
            clock_topology.clocks.len()
        )
        .ok();
        for clock in &clock_topology.clocks {
            let name = super::expr::escape_c(&clock.name);
            let half_period = clock.period_ps / 2;
            writeln!(
                out,
                "{INDENT}ctx.clocks.push_back(HarcClockState{{\"{name}\", {half_period}, {half_period}, 0, 0, [&ctx](int level) {{ ctx.dut->{name} = level; }}}});"
            )
            .ok();
            writeln!(out, "{INDENT}ctx.dut->{name} = 0;").ok();
        }
        writeln!(out, "}}").ok();
        writeln!(out).ok();
        let runtime_cells_type = super::runtime::test_runtime_cells_struct(
            &mut out,
            prog,
            &plan.runtime_cells,
            test,
            stem,
            &plan.vec_lane_widths,
            plan.dut_type(),
            Some(&plan.dut_access),
            plan.randomize.as_ref(),
        )?;
        let state_type =
            super::runtime::unique_generated_type_name(prog, &format!("HarcRunState_{stem}"));
        let mut state_members = tb
            .component_fields
            .iter()
            .map(|binding| binding.field.clone())
            .collect::<HashSet<_>>();
        let runtime_cells_member = runtime_cells_type
            .as_ref()
            .map(|_| fresh_capsule_identifier("_harc_runtime_cells", &mut state_members));
        let testbench_member = super::needs_tb_struct(tb)
            .then(|| fresh_capsule_identifier("_harc_testbench", &mut state_members));
        writeln!(out, "struct {state_type} {{").ok();
        if let (Some(runtime_cells_type), Some(member)) =
            (&runtime_cells_type, &runtime_cells_member)
        {
            writeln!(out, "{INDENT}{runtime_cells_type} {member}{{}};").ok();
        }
        if let Some(member) = &testbench_member {
            writeln!(out, "{INDENT}{} {member}{{}};", tb.name).ok();
        }
        for binding in &tb.component_fields {
            let component = prog
                .components
                .get(binding.component.index())
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` references missing component c{}",
                        test.name, binding.component.0
                    ))
                })?;
            writeln!(out, "{INDENT}{} {}{{}};", component.name, binding.field).ok();
        }
        for binding in &tb.regblock_bindings {
            if !state_members.insert(binding.field.clone()) {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` has colliding run-state member `{}`",
                    test.name, binding.field
                )));
            }
            let regblock = prog
                .regblocks
                .get(binding.regblock.index())
                .ok_or_else(|| {
                    EmitError(format!(
                        "TB-IR common layout test `{}` references missing regblock rb{}",
                        test.name, binding.regblock.0
                    ))
                })?;
            let mirror = prog.records.get(regblock.record.index()).ok_or_else(|| {
                EmitError(format!(
                    "TB-IR common layout regblock `{}` references missing mirror r{}",
                    regblock.name, regblock.record.0
                ))
            })?;
            writeln!(out, "{INDENT}{} {}{{}};", mirror.name, binding.field).ok();
            if !binding.callbacks.is_empty() {
                let depth = format!("{}_cb_depth", binding.field);
                if !state_members.insert(depth.clone()) {
                    return Err(EmitError(format!(
                        "TB-IR common layout test `{}` has colliding run-state member `{depth}`",
                        test.name
                    )));
                }
                writeln!(out, "{INDENT}uint32_t {depth} = 0;").ok();
            }
        }
        for actor in &tb.unbound_state_actors {
            if !state_members.insert(actor.storage.clone()) {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` has colliding run-state member `{}`",
                    test.name, actor.storage
                )));
            }
            writeln!(
                out,
                "{INDENT}{} {}{{}};",
                super::runtime::unbound_state_struct_ref(prog, actor.transactor),
                actor.storage
            )
            .ok();
        }
        for actor in tb
            .target_tlm_actors
            .iter()
            .filter(|actor| actor.host_component.is_none())
        {
            if !state_members.insert(actor.instance.clone()) {
                return Err(EmitError(format!(
                    "TB-IR common layout test `{}` has colliding run-state member `{}`",
                    test.name, actor.instance
                )));
            }
            writeln!(
                out,
                "{INDENT}{} {}{{}};",
                super::runtime::unbound_state_struct_ref(prog, actor.transactor),
                actor.instance
            )
            .ok();
        }
        writeln!(out, "}};").ok();
        writeln!(out).ok();
        writeln!(
            out,
            "static void* harc_create_state_{stem}(HarcTestContext& ctx) {{"
        )
        .ok();
        writeln!(out, "{INDENT}auto* state = new {state_type}{{}};").ok();
        if let Some(member) = &testbench_member {
            if !tb.synthetic {
                writeln!(out, "{INDENT}state->{member}.dut = ctx.dut;").ok();
            }
        }
        for binding in &tb.component_fields {
            super::emit_component_dut_bindings(
                &mut out,
                prog,
                binding.component,
                &format!("state->{}", binding.field),
                plan.dut_type(),
                "ctx.dut",
                1,
            )?;
        }
        writeln!(out, "{INDENT}return state;").ok();
        writeln!(out, "}}").ok();
        writeln!(out).ok();
        writeln!(
            out,
            "static void harc_report_state_{stem}(HarcTestContext& ctx, void* opaque) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}auto& state = *static_cast<{state_type}*>(opaque);"
        )
        .ok();
        let cover_reports = test
            .cover_checks
            .iter()
            .map(|cover| (*cover, &prog.cover_checks[cover.index()]))
            .collect::<Vec<_>>();
        let report_receiver = runtime_cells_member
            .as_ref()
            .map(|member| format!("state.{member}"));
        let report_runtime_cells =
            report_receiver
                .as_deref()
                .map(|receiver| super::expr::RuntimeCellRenderBinding {
                    plan: &plan.runtime_cells,
                    test,
                    receiver,
                });
        super::runtime::concurrent_coverage_reports(
            &mut out,
            1,
            &cover_reports,
            report_runtime_cells,
            "ctx.log_ctx.coverage_json",
        )?;
        if cover_reports.is_empty() {
            writeln!(out, "{INDENT}(void)ctx;").ok();
            writeln!(out, "{INDENT}(void)state;").ok();
        }
        writeln!(out, "}}").ok();
        writeln!(out).ok();
        writeln!(
            out,
            "static void harc_destroy_state_{stem}(void* opaque) {{"
        )
        .ok();
        writeln!(out, "{INDENT}delete static_cast<{state_type}*>(opaque);").ok();
        writeln!(out, "}}").ok();
        writeln!(out).ok();
        writeln!(
            out,
            "static harc_rt::HarcThread harc_body_{stem}(HarcTestContext& ctx, harc_rt::ThreadSlot* _slot, void* _harc_opaque_state) {{"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}auto& _harc_run_state = *static_cast<{state_type}*>(_harc_opaque_state);"
        )
        .ok();
        writeln!(out, "{INDENT}auto* dut = ctx.dut;").ok();
        writeln!(out, "{INDENT}auto& errors = ctx.errors;").ok();
        writeln!(out, "{INDENT}auto& _fatal = ctx.fatal;").ok();
        writeln!(out, "{INDENT}auto& cycle_count = ctx.cycle_count;").ok();
        writeln!(out, "{INDENT}auto& trace = ctx.trace;").ok();
        writeln!(out, "{INDENT}auto& log_ctx = ctx.log_ctx;").ok();
        writeln!(out, "{INDENT}auto& _checkers = ctx._checkers;").ok();
        writeln!(
            out,
            "{INDENT}auto& _post_eval_services = ctx._post_eval_services;"
        )
        .ok();
        writeln!(
            out,
            "{INDENT}auto& _auto_cov_reports = ctx._auto_cov_reports;"
        )
        .ok();
        writeln!(out, "{INDENT}auto& harc_rng = ctx.rng;").ok();
        writeln!(out, "{INDENT}auto& sched = ctx.scheduler;").ok();
        writeln!(out, "{INDENT}auto tick = [&]() {{ harc_tseq_tick(ctx); }};").ok();
        for actor in &tb.unbound_state_actors {
            writeln!(
                out,
                "{INDENT}auto& {} = _harc_run_state.{};",
                actor.storage, actor.storage
            )
            .ok();
        }
        for actor in &tb.target_tlm_actors {
            writeln!(
                out,
                "{INDENT}auto& {} = _harc_run_state.{};",
                actor.instance, actor.instance
            )
            .ok();
        }
        for binding in &tb.regblock_bindings {
            writeln!(
                out,
                "{INDENT}auto& {} = _harc_run_state.{};",
                binding.field, binding.field
            )
            .ok();
            if !binding.callbacks.is_empty() {
                writeln!(
                    out,
                    "{INDENT}auto& {}_cb_depth = _harc_run_state.{}_cb_depth;",
                    binding.field, binding.field
                )
                .ok();
            }
        }
        let runtime_cells_receiver = runtime_cells_member
            .as_ref()
            .map(|member| format!("_harc_run_state.{member}"));
        let runtime_cell_binding = runtime_cells_receiver.as_deref().map(|receiver| {
            super::expr::RuntimeCellRenderBinding {
                plan: &plan.runtime_cells,
                test,
                receiver,
            }
        });
        out.push_str(
            r#"    auto sim_logf_line = [&](FILE* f, const char* sev, const char* fmt, ...) {
        HARC_RT_LOG_FILE_ONLY_PRINTF(f, cycle_count, sev, fmt);
    };

    auto sim_log_line = [&](const char* sev, const char* fmt, ...) {
        va_list ap;
        va_start(ap, fmt);
        harc_rt::log::harc_log_vline(log_ctx.sim_log, &trace, cycle_count, sev, fmt, ap);
        va_end(ap);
    };

"#,
        );
        let testbench_receiver = testbench_member
            .as_ref()
            .map(|member| format!("_harc_run_state.{member}"));
        let component_bindings = tb
            .component_fields
            .iter()
            .map(|binding| super::expr::TestbenchComponentRenderBinding {
                field: binding.field.clone(),
                component: binding.component,
                receiver: format!("_harc_run_state.{}", binding.field),
            })
            .collect::<Vec<_>>();
        let transactor_state_bindings = tb
            .unbound_state_actors
            .iter()
            .map(|actor| super::expr::TestbenchTransactorStateRenderBinding {
                field: actor.field.clone(),
                transactor: actor.transactor,
                receiver: format!("_harc_run_state.{}", actor.storage),
            })
            .collect::<Vec<_>>();
        emit_common_covergroup_registrations(
            &mut out,
            prog,
            test,
            tb,
            "_harc_run_state",
            testbench_receiver.as_deref(),
            plan,
        )?;
        let mut actor_threads = Vec::new();
        for actor in &tb.target_tlm_actors {
            func::emit_target_actor(
                &mut out,
                prog,
                actor,
                &tb.bus_bindings,
                plan.mt,
                &mut actor_threads,
                Some("ctx"),
                1,
            )?;
        }
        for function in &body.test_hooks {
            func::emit_test_hook(
                &mut out,
                prog,
                prog.function(*function),
                plan.dut_type(),
                1,
                func::TestHookRenderBindings {
                    flow: func::FlowRenderBindings {
                        run_context: Some("ctx"),
                        dut_receiver: Some("ctx.dut"),
                        dut_access: Some(&plan.dut_access),
                        dut_lane_widths: None,
                        testbench_receiver: testbench_receiver.as_deref(),
                        testbench_components: Some(&component_bindings),
                        testbench_transactor_states: Some(&transactor_state_bindings),
                        bus_adapters: Some(super::expr::BusAdapterRenderBindings {
                            current: None,
                            callables: &plan.bus_adapters,
                        }),
                        clocks: Some(&test.clocks),
                        reserved: &[
                            "_harc_run_state",
                            "_harc_opaque_state",
                            "_harc_callback_state",
                            "_harc_callback_context",
                        ],
                        durable_callbacks: true,
                    },
                    runtime_cells: runtime_cell_binding,
                    common_contextual_tseqs: Some(&plan.contextual_tseqs),
                    durable_capture: true,
                },
            )?;
        }
        for edge in &tb.connects {
            super::emit_one_connect(&mut out, prog, "_harc_run_state", edge, Some("ctx"));
        }
        for binding in &tb.component_fields {
            let instance = format!("_harc_run_state.{}", binding.field);
            let bound_bus = tb
                .bound_bus_binding(ir::BoundBusOwner::Component(binding.component))
                .map_err(|detail| EmitError(format!("tbir: test `{}`: {detail}", test.name)))?;
            for edge in &binding.connects {
                if super::edge_is_enabled(prog, binding.component, binding.mode, edge) {
                    super::emit_one_connect(&mut out, prog, &instance, edge, Some("ctx"));
                }
            }
            super::emit_nested_connects(
                &mut out,
                prog,
                binding.component,
                &instance,
                binding.mode,
                Some("ctx"),
            );
            super::emit_on_handler_regs(
                &mut out,
                prog,
                binding.component,
                &instance,
                false,
                binding.mode,
                bound_bus,
                Some(&plan.bus_adapters),
                Some("ctx"),
            )?;
            super::emit_lifecycle_checkers(
                &mut out,
                prog,
                &plan.runtime_cells,
                binding.component,
                &instance,
                binding.mode,
                plan.dut_type(),
                Some(&plan.dut_access),
                &plan.vec_lane_widths,
                bound_bus,
                Some(&plan.bus_adapters),
                Some("ctx"),
            )?;
        }
        if !tb.periodic_services.is_empty() || !tb.cycle_services.is_empty() {
            let runtime_cell_binding = runtime_cell_binding.ok_or_else(|| {
                EmitError(format!(
                    "tbir: common test `{}` has lifecycle services without runtime-cell storage",
                    test.name
                ))
            })?;
            super::emit_tb_periodic_services(&mut out, tb, runtime_cell_binding, Some("ctx"))?;
            super::emit_tb_cycle_services(
                &mut out,
                prog,
                tb,
                plan.dut_type(),
                Some(&plan.dut_access),
                &plan.vec_lane_widths,
                runtime_cell_binding,
                Some("ctx"),
            )?;
        }
        func::emit_common_function(
            &mut out,
            prog,
            run,
            &prog.records,
            &tb.bus_bindings,
            &plan.vec_lane_widths,
            plan.randomize_snippets(),
            plan.dut_type(),
            &body.run_callback_captures,
            1,
            &plan.contextual_tseqs,
            runtime_cell_binding,
            func::FlowRenderBindings {
                run_context: Some("ctx"),
                dut_receiver: Some("ctx.dut"),
                dut_access: Some(&plan.dut_access),
                dut_lane_widths: None,
                testbench_receiver: testbench_receiver.as_deref(),
                testbench_components: Some(&component_bindings),
                testbench_transactor_states: Some(&transactor_state_bindings),
                bus_adapters: Some(super::expr::BusAdapterRenderBindings {
                    current: None,
                    callables: &plan.bus_adapters,
                }),
                clocks: Some(&test.clocks),
                reserved: &["_harc_run_state", "_harc_opaque_state"],
                durable_callbacks: true,
            },
            &outofline_lifecycle,
        )?;
        if let Some(check) = body.check {
            let check = prog.function(check);
            func::emit_common_function(
                &mut out,
                prog,
                check,
                &prog.records,
                &tb.bus_bindings,
                &plan.vec_lane_widths,
                plan.randomize_snippets(),
                plan.dut_type(),
                &body.check_callback_captures,
                1,
                &plan.contextual_tseqs,
                runtime_cell_binding,
                func::FlowRenderBindings {
                    run_context: Some("ctx"),
                    dut_receiver: Some("ctx.dut"),
                    dut_access: Some(&plan.dut_access),
                    dut_lane_widths: None,
                    testbench_receiver: testbench_receiver.as_deref(),
                    testbench_components: Some(&component_bindings),
                    testbench_transactor_states: Some(&transactor_state_bindings),
                    bus_adapters: Some(super::expr::BusAdapterRenderBindings {
                        current: None,
                        callables: &plan.bus_adapters,
                    }),
                    clocks: Some(&test.clocks),
                    reserved: &["_harc_run_state", "_harc_opaque_state"],
                    durable_callbacks: true,
                },
                &outofline_lifecycle,
            )?;
        }
        writeln!(out, "{INDENT}ctx.run_complete = true;").ok();
        writeln!(
            out,
            "{INDENT}co_await harc_rt::wait_until(_slot, [] {{ return false; }});"
        )
        .ok();
        writeln!(out, "{INDENT}co_return;").ok();
        writeln!(out, "}}").ok();
        writeln!(out).ok();
        writeln!(
            out,
            "static const HarcTestRunDescriptor harc_run_{stem} = {{"
        )
        .ok();
        writeln!(out, "{INDENT}\"{escaped_test_name}\",").ok();
        writeln!(out, "{INDENT}&harc_configure_clocks_{stem},").ok();
        writeln!(out, "{INDENT}&harc_create_state_{stem},").ok();
        writeln!(out, "{INDENT}&harc_body_{stem},").ok();
        writeln!(out, "{INDENT}&harc_report_state_{stem},").ok();
        writeln!(out, "{INDENT}&harc_destroy_state_{stem},").ok();
        writeln!(out, "}};").ok();
        writeln!(out).ok();
        writeln!(out, "int run_{}(int argc, char** argv) {{", test.name).ok();
        writeln!(
            out,
            "{INDENT}return harc_run_test(harc_run_{stem}, argc, argv);"
        )
        .ok();
        writeln!(out, "}}").ok();
        writeln!(out).ok();
        out.push_str(&common_artifacts::render_test_descriptor_with_required_abi(
            artifact_test,
        ));
    }
    Ok(out)
}

pub fn emit_common_interface(plan: &CommonCppPlan) -> Result<String, EmitError> {
    Ok(plan.publication()?.interface().to_string())
}

pub fn emit_common_runtime(plan: &CommonCppPlan) -> Result<String, EmitError> {
    plan.publication()?.runtime()
}

/// Render the capsule at `capsule_index` from this plan. The renderer accepts
/// no capsule object that could belong to another validated program.
///
/// ```compile_fail
/// use harc::codegen::tbir::common::{emit_common_capsule, CommonCppPlan};
///
/// fn render_foreign(plan: &CommonCppPlan, other: &CommonCppPlan) {
///     let foreign_capsule = &other.capsules()[0];
///     let _ = emit_common_capsule(plan, foreign_capsule);
/// }
/// ```
pub fn emit_common_capsule(
    plan: &CommonCppPlan,
    capsule_index: usize,
) -> Result<String, EmitError> {
    let capsule = plan.capsules.get(capsule_index).ok_or_else(|| {
        EmitError(format!(
            "tbir: common capsule index {capsule_index} is out of range for {} planned capsule(s)",
            plan.capsules.len()
        ))
    })?;
    plan.publication()?.capsule(capsule)
}

pub fn emit_common_capsules<F>(
    plan: &CommonCppPlan,
    jobs: usize,
    on_capsule: F,
) -> Result<Vec<usize>, EmitError>
where
    F: Fn(&CommonCapsulePlan, String, Duration) -> Result<(), EmitError> + Sync,
{
    let publication = plan.publication()?;
    emit_common_publication_capsules(&publication, jobs, on_capsule)
}

pub fn emit_common_publication_capsules<F>(
    publication: &CommonCppPublication<'_>,
    jobs: usize,
    on_capsule: F,
) -> Result<Vec<usize>, EmitError>
where
    F: Fn(&CommonCapsulePlan, String, Duration) -> Result<(), EmitError> + Sync,
{
    let plan = publication.plan();
    if jobs <= 1 || plan.capsules.len() <= 1 {
        let mut delivered = Vec::with_capacity(plan.capsules.len());
        for (index, capsule) in plan.capsules.iter().enumerate() {
            let started = Instant::now();
            let cpp = publication.capsule(capsule)?;
            on_capsule(capsule, cpp, started.elapsed())?;
            delivered.push(index);
        }
        return Ok(delivered);
    }

    let cursor = AtomicUsize::new(0);
    let fail_limit = AtomicUsize::new(usize::MAX);
    type PanicPayload = Box<dyn std::any::Any + Send + 'static>;
    enum CapsuleFailure {
        Error(EmitError),
        Panic(PanicPayload),
    }
    let first_failure: Mutex<Option<(usize, CapsuleFailure)>> = Mutex::new(None);
    let delivered: Mutex<Vec<usize>> = Mutex::new(Vec::with_capacity(plan.capsules.len()));

    let record_error = |index: usize, error: EmitError| {
        fail_limit.fetch_min(index, Ordering::Relaxed);
        let mut first = first_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if first.as_ref().is_none_or(|(old, _)| index < *old) {
            *first = Some((index, CapsuleFailure::Error(error)));
        }
    };
    let record_panic = |index: usize, payload: PanicPayload| {
        fail_limit.fetch_min(index, Ordering::Relaxed);
        let mut first = first_failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if first.as_ref().is_none_or(|(old, _)| index < *old) {
            *first = Some((index, CapsuleFailure::Panic(payload)));
        }
    };

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let cursor = &cursor;
            let fail_limit = &fail_limit;
            let record_error = &record_error;
            let record_panic = &record_panic;
            let delivered = &delivered;
            let on_capsule = &on_capsule;
            let spawned = std::thread::Builder::new()
                .stack_size(8 * 1024 * 1024)
                .spawn_scoped(scope, move || loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    if index >= plan.capsules.len() {
                        break;
                    }
                    if index > fail_limit.load(Ordering::Relaxed) {
                        continue;
                    }
                    let capsule = &plan.capsules[index];
                    let started = Instant::now();
                    let cpp = match publication.capsule(capsule) {
                        Ok(cpp) => cpp,
                        Err(error) => {
                            record_error(index, error);
                            continue;
                        }
                    };
                    if index > fail_limit.load(Ordering::Relaxed) {
                        continue;
                    }
                    let handed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        on_capsule(capsule, cpp, started.elapsed())
                    }));
                    match handed {
                        Ok(Ok(())) => delivered
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(index),
                        Ok(Err(error)) => record_error(index, error),
                        Err(payload) => record_panic(index, payload),
                    }
                });
            if let Err(error) = spawned {
                record_error(
                    usize::MAX,
                    EmitError(format!(
                        "could not spawn TB-IR common capsule emission worker: {error}"
                    )),
                );
                break;
            }
        }
    });

    if let Some((_, failure)) = first_failure
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        match failure {
            CapsuleFailure::Error(error) => return Err(error),
            CapsuleFailure::Panic(payload) => std::panic::resume_unwind(payload),
        }
    }
    let mut delivered = delivered
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    delivered.sort_unstable();
    Ok(delivered)
}
