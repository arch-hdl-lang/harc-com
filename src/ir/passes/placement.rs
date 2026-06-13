//! `placement` — per-block placement tier + timing class, capability-
//! checked against a `TargetProfile` (docs/tb-ir-design.md §"Multi-
//! target placement model", §"`placement`"; docs/tb-ir-plan.md
//! §"Passes").
//!
//! Tags every block of every coroutine-shaped function with where it
//! can execute on a placement-split target (Tier 0 co-located with the
//! DUT / Tier 1 near-processor / Tier 2 host service) and how timing-
//! sensitive its DUT interaction is (`CycleExact` / `TimingTolerant` /
//! `Unknown`), then checks every DUT access and every cycle-exact
//! region against the profile's capability model. Constructs the
//! target cannot execute surface as `PlacementDiag`s — compile-time
//! explanations, produced before any emission.
//!
//! Governing principle (design doc): "when transaction-level sync does
//! not change the nature of the test, use it — it is the speed path.
//! Per-cycle sync is reserved for regions where the TB/DUT interaction
//! requires it to work at all." The classifier is therefore
//! conservative: raw pin access anchored by a `WaitCycles` is
//! `CycleExact` unless proven otherwise; `wait until` regions and
//! transactor-call boundaries are `TimingTolerant` by construction
//! (the cycle-exact half of a transactor lives in its body, a Tier-0
//! candidate on split targets).
//!
//! MVP scope notes:
//! - Classification is block-local (one walk over stmts + terminator,
//!   O(IR size)); cross-block refinement (e.g. propagating a
//!   `CycleExact` anchor backward through suspend-free predecessors)
//!   is future work and the conservative local answer is `Unknown`.
//! - The tier heuristic is deliberately simple: pin-touching blocks
//!   are Tier-0 candidates (Tier 1 when the profile has no Tier 0),
//!   host-service-only blocks (logging / coverage reporting / fail
//!   diagnostics) are Tier 2, everything else is Tier 1. The
//!   load-bearing parts are the single-site triviality guarantee, the
//!   timing classifier, and the capability diagnostics.
//! - The design doc keys the annotation table with a `HashMap`; a
//!   `BTreeMap` is used instead so iteration — and the rendered dump —
//!   is byte-stable across runs (same override `lower_coroutine` made).

use super::super::{
    BlockId, Expr, FmtArgs, FunctionId, FunctionKind, PortAccess, Stmt, TbFunction, TbProgram,
    Terminator,
};
use crate::ir::CallTarget;
use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlacementTier {
    /// Runs in lockstep with DUT evaluation; pin-accurate every cycle.
    Tier0CoLocated,
    /// Transaction-level control flow (sequences, scoreboards, FSMs).
    Tier1NearProcessor,
    /// Host services: log formatting, file I/O, fatal, solving.
    Tier2HostService,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimingClass {
    /// Cycle-precise DUT interaction is semantically necessary.
    CycleExact,
    /// Order-sensitive but not cycle-count-sensitive; transaction-level
    /// sync is legal and preferred for speed.
    TimingTolerant,
    /// Neither proven; treated conservatively per target.
    Unknown,
}

/// Declarative description of a placement target. The *schema* is part
/// of the public IR contract; concrete profiles are backend-owned —
/// public harc ships only the named built-ins below (a file-based
/// profile format is deliberately deferred; freezing one now would
/// freeze a public format contract prematurely).
#[derive(Debug, Clone)]
pub struct TargetProfile {
    pub name: &'static str,
    /// Target has a Tier-0 execution site (generated BFM/monitor,
    /// simulation-kernel code) that can absorb cycle-exact regions.
    pub has_tier0: bool,
    /// Tier 1 advances in lockstep with the DUT clock. False on
    /// free-running firmware / batched-kernel targets, where a
    /// cycle-exact region cannot execute at Tier 1.
    pub tier1_cycle_locked: bool,
    /// `PortAccess` classes reachable on this target (architectural
    /// ports are reachable everywhere by design; probes and forces are
    /// declared resources a target may not realize).
    pub reach_port: bool,
    pub reach_probe: bool,
    pub reach_force: bool,
    /// Constraint-solve strategies available (consumed once
    /// `Terminator::Randomize` exists in the lowered subset; carried
    /// in the schema now because the profile is the public contract).
    pub solve_device_rejection: bool,
    pub solve_replay_table: bool,
    pub solve_host_sync: bool,
    /// Relative cost of a cross-tier synchronization.
    pub sync_cost: SyncCost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SyncCost {
    /// Same process, same loop — sync is a function call.
    Free,
    /// Same die / shared memory — cheap but not free.
    Cheap,
    /// Queue or network hop — batch aggressively.
    Expensive,
}

impl TargetProfile {
    /// The `cpp_tb` profile: one site, all tiers co-resident, Tier 1
    /// cycle-locked, every access class and solve strategy available.
    /// Under it the capability checks cannot fire — `diagnostics` is
    /// empty for every well-formed program (tested).
    pub fn single_site() -> Self {
        TargetProfile {
            name: "single-site",
            has_tier0: true,
            tier1_cycle_locked: true,
            reach_port: true,
            reach_probe: true,
            reach_force: true,
            solve_device_rejection: true,
            solve_replay_table: true,
            solve_host_sync: true,
            sync_cost: SyncCost::Free,
        }
    }

    /// A deliberately constrained demo profile for tests and
    /// diagnostics dumps: no Tier 0, free-running Tier 1, probes and
    /// forces unreachable, host-sync solving only. Models the worst
    /// case of an embedded/companion-CPU target.
    pub fn split_strict() -> Self {
        TargetProfile {
            name: "split-strict",
            has_tier0: false,
            tier1_cycle_locked: false,
            reach_port: true,
            reach_probe: false,
            reach_force: false,
            solve_device_rejection: false,
            solve_replay_table: false,
            solve_host_sync: true,
            sync_cost: SyncCost::Expensive,
        }
    }

    /// Resolve a `--profile` name (kebab or snake case).
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "single-site" | "single_site" => Some(Self::single_site()),
            "split-strict" | "split_strict" => Some(Self::split_strict()),
            _ => None,
        }
    }

    fn reaches(&self, access: PortAccess) -> bool {
        match access {
            PortAccess::Port => self.reach_port,
            PortAccess::Probe => self.reach_probe,
            PortAccess::Force => self.reach_force,
        }
    }
}

/// Pass output: the annotation table plus capability diagnostics.
/// `BTreeMap` (not the design doc's `HashMap`) for byte-stable
/// iteration.
#[derive(Debug, Clone, Default)]
pub struct PlacementTable {
    pub blocks: BTreeMap<(FunctionId, BlockId), (PlacementTier, TimingClass)>,
    pub diagnostics: Vec<PlacementDiag>,
}

/// One construct the profile cannot execute, with the compile-time
/// explanation the design doc requires ("fail … with an explanation,
/// before any emission").
#[derive(Debug, Clone)]
pub struct PlacementDiag {
    pub func: FunctionId,
    pub func_name: String,
    pub block: BlockId,
    pub message: String,
}

impl Display for PlacementDiag {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "fn{} {} b{}: {}",
            self.func.0, self.func_name, self.block.0, self.message
        )
    }
}

/// Annotate every block of every coroutine-shaped function
/// (`Run`/`Check`/`SamplerAuto`; `Helper` bodies are inlined or emit
/// as plain functions at their caller's site and carry no independent
/// placement). Read-only; one walk per block.
pub fn run(prog: &TbProgram, profile: &TargetProfile) -> PlacementTable {
    let mut table = PlacementTable::default();
    for func in &prog.functions {
        if func.kind == FunctionKind::Helper {
            continue;
        }
        for (bi, block) in func.blocks.iter().enumerate() {
            let bid = BlockId(bi as u32);
            let feat = block_features(block);
            let timing = classify_timing(&feat);
            let tier = classify_tier(&feat, profile);
            table.blocks.insert((func.id, bid), (tier, timing));
            capability_check(func, bid, &feat, timing, profile, &mut table.diagnostics);
        }
    }
    table
}

/// What one block does, summarized for classification.
#[derive(Default)]
struct BlockFeatures {
    /// Access classes touched by `DutRead`/`DutWrite` stmts and inline
    /// `Expr::Port` reads (wait predicates, fmt args, assert conds, …),
    /// deduped, in first-touch order.
    accesses: Vec<PortAccess>,
    /// Block contains a raw pin statement (`DutRead`/`DutWrite`).
    has_pin_stmt: bool,
    /// Block contains only host-service statements (logging, coverage
    /// reporting, fail diagnostics) — vacuously true for empty blocks.
    host_service_only: bool,
    /// Block calls through a `CallTarget::TransactorMethod`.
    has_transactor_call: bool,
    /// Terminator shape.
    suspends_cycles: bool,
    suspends_until: bool,
}

fn block_features(block: &super::super::BasicBlock) -> BlockFeatures {
    let mut accesses: Vec<PortAccess> = Vec::new();
    let mut transactor = false;
    let mut has_pin_stmt = false;
    let mut host_service_only = true;
    let mut suspends_cycles = false;
    let mut suspends_until = false;
    fn touch(accesses: &mut Vec<PortAccess>, a: PortAccess) {
        if !accesses.contains(&a) {
            accesses.push(a);
        }
    }
    for stmt in &block.stmts {
        match stmt {
            Stmt::DutWrite(port, value) => {
                has_pin_stmt = true;
                host_service_only = false;
                touch(&mut accesses, port.access);
                visit_expr(value, &mut accesses, &mut transactor);
            }
            Stmt::DutRead(_, port) => {
                has_pin_stmt = true;
                host_service_only = false;
                touch(&mut accesses, port.access);
            }
            Stmt::Assign(_, e) => {
                host_service_only = false;
                visit_expr(e, &mut accesses, &mut transactor);
            }
            Stmt::AssertCheck { cond, on_fail } => {
                host_service_only = false;
                visit_expr(cond, &mut accesses, &mut transactor);
                visit_fmt(on_fail, &mut accesses, &mut transactor);
            }
            Stmt::Log { args, .. } => {
                visit_fmt(args, &mut accesses, &mut transactor);
            }
            Stmt::FailDiag { guard, args } => {
                if let Some(g) = guard {
                    visit_expr(g, &mut accesses, &mut transactor);
                }
                visit_fmt(args, &mut accesses, &mut transactor);
            }
            Stmt::RecordInit(_, _) => {
                host_service_only = false;
            }
            Stmt::RecordFieldWrite { value, .. } => {
                host_service_only = false;
                visit_expr(value, &mut accesses, &mut transactor);
            }
            Stmt::TransactorCall { call, .. } => {
                host_service_only = false;
                visit_expr(call, &mut accesses, &mut transactor);
            }
            Stmt::TbFieldWrite { value, .. } => {
                // Host state on the _tb struct — no pin access of its
                // own; the value expression may carry inline reads.
                host_service_only = false;
                visit_expr(value, &mut accesses, &mut transactor);
            }
            Stmt::CovReport(_) => {}
            Stmt::ScoreboardOp { op, .. } => {
                // Host state on the scoreboard struct — no pin access of
                // its own; the value expression (push/scalar-write) may
                // carry inline reads.
                host_service_only = false;
                match op {
                    crate::ir::ScoreboardOp::QueuePush { value, .. }
                    | crate::ir::ScoreboardOp::ScalarWrite { value, .. } => {
                        visit_expr(value, &mut accesses, &mut transactor);
                    }
                    crate::ir::ScoreboardOp::QueuePop { .. } => {}
                }
            }
        }
    }
    match &block.terminator {
        Terminator::WaitCycles(n, _, _) => {
            suspends_cycles = true;
            visit_expr(n, &mut accesses, &mut transactor);
        }
        Terminator::WaitCyclesSync(n, _) => {
            suspends_cycles = true;
            visit_expr(n, &mut accesses, &mut transactor);
        }
        Terminator::WaitTimePs(..) => {
            // Wall-clock wait: advances simulated time like a counted
            // wait — same cycle-anchoring effect for classification.
            suspends_cycles = true;
        }
        Terminator::WaitUntil { preds, .. } => {
            suspends_until = true;
            for p in preds {
                visit_expr(&p.expr, &mut accesses, &mut transactor);
            }
        }
        Terminator::WaitUntilTimeout { preds, cycles, .. } => {
            suspends_until = true;
            for p in preds {
                visit_expr(&p.expr, &mut accesses, &mut transactor);
            }
            visit_expr(cycles, &mut accesses, &mut transactor);
        }
        Terminator::Branch(cond, _, _) => {
            visit_expr(cond, &mut accesses, &mut transactor);
        }
        Terminator::Fatal(args) => {
            visit_fmt(args, &mut accesses, &mut transactor);
        }
        Terminator::Jump(_) | Terminator::Return => {}
    }
    BlockFeatures {
        accesses,
        has_pin_stmt,
        host_service_only,
        has_transactor_call: transactor,
        suspends_cycles,
        suspends_until,
    }
}

fn visit_expr(e: &Expr, accesses: &mut Vec<PortAccess>, transactor: &mut bool) {
    match e {
        Expr::Port(p) => {
            if !accesses.contains(&p.access) {
                accesses.push(p.access);
            }
        }
        Expr::Binary(_, a, b) => {
            visit_expr(a, accesses, transactor);
            visit_expr(b, accesses, transactor);
        }
        Expr::Unary(_, a) => visit_expr(a, accesses, transactor),
        Expr::Ternary(c, t, e) => {
            visit_expr(c, accesses, transactor);
            visit_expr(t, accesses, transactor);
            visit_expr(e, accesses, transactor);
        }
        Expr::WidthCast { inner, .. } => visit_expr(inner, accesses, transactor),
        Expr::Call(target, args) => {
            if matches!(target, CallTarget::TransactorMethod { .. }) {
                *transactor = true;
            }
            for a in args {
                visit_expr(a, accesses, transactor);
            }
        }
        Expr::Literal { .. }
        | Expr::WideLiteral(_)
        | Expr::Local(_)
        | Expr::RecordField { .. }
        | Expr::TbField(_)
        | Expr::ScoreboardQuery { .. }
        | Expr::CovBin { .. } => {}
    }
}

fn visit_fmt(args: &FmtArgs, accesses: &mut Vec<PortAccess>, transactor: &mut bool) {
    for a in &args.args {
        visit_expr(&a.expr, accesses, transactor);
    }
}

fn classify_timing(feat: &BlockFeatures) -> TimingClass {
    if feat.has_pin_stmt && feat.suspends_cycles {
        // Raw pin access anchored by a cycle-counted wait: protocol
        // correctness may depend on the exact cycle relationship.
        // Conservative default per the design doc.
        return TimingClass::CycleExact;
    }
    if feat.suspends_until || feat.has_transactor_call {
        // "Write reg, wait until done, read result" works at any sync
        // granularity; a transactor call is timing-tolerant at the
        // call boundary by construction.
        return TimingClass::TimingTolerant;
    }
    if feat.has_pin_stmt {
        // Pin access with no local anchor — the anchor may live in a
        // neighboring block. Block-local analysis cannot prove either
        // way; conservative per-target treatment.
        return TimingClass::Unknown;
    }
    // No DUT interaction at all: timing-irrelevant, any granularity.
    TimingClass::TimingTolerant
}

fn classify_tier(feat: &BlockFeatures, profile: &TargetProfile) -> PlacementTier {
    if feat.has_pin_stmt || !feat.accesses.is_empty() {
        return if profile.has_tier0 {
            PlacementTier::Tier0CoLocated
        } else {
            PlacementTier::Tier1NearProcessor
        };
    }
    if feat.host_service_only {
        return PlacementTier::Tier2HostService;
    }
    PlacementTier::Tier1NearProcessor
}

fn capability_check(
    func: &TbFunction,
    block: BlockId,
    feat: &BlockFeatures,
    timing: TimingClass,
    profile: &TargetProfile,
    out: &mut Vec<PlacementDiag>,
) {
    for &access in &feat.accesses {
        if !profile.reaches(access) {
            let what = match access {
                PortAccess::Port => "an architectural port",
                PortAccess::Probe => "a probe signal",
                PortAccess::Force => "a force point",
            };
            out.push(PlacementDiag {
                func: func.id,
                func_name: func.name.clone(),
                block,
                message: format!(
                    "accesses {what}, but profile `{}` cannot reach {access:?} access \
                     on any tier (declare the resource on the target, or re-place \
                     this region)",
                    profile.name
                ),
            });
        }
    }
    if timing == TimingClass::CycleExact && !profile.tier1_cycle_locked && !profile.has_tier0 {
        out.push(PlacementDiag {
            func: func.id,
            func_name: func.name.clone(),
            block,
            message: format!(
                "cycle-exact DUT interaction, but profile `{}` has no cycle-locked \
                 execution site (Tier 1 is free-running and there is no Tier 0 to \
                 absorb it) — route the pin-level half through a transactor",
                profile.name
            ),
        });
    }
}

// ── Display (the `dump-ir --pass placement` suffix) ─────────────────

impl PlacementTable {
    pub fn display<'a>(
        &'a self,
        prog: &'a TbProgram,
        profile: &'a TargetProfile,
    ) -> PlacementTableDisplay<'a> {
        PlacementTableDisplay {
            table: self,
            prog,
            profile,
        }
    }
}

pub struct PlacementTableDisplay<'a> {
    table: &'a PlacementTable,
    prog: &'a TbProgram,
    profile: &'a TargetProfile,
}

fn tier_str(t: PlacementTier) -> &'static str {
    match t {
        PlacementTier::Tier0CoLocated => "tier0-co-located",
        PlacementTier::Tier1NearProcessor => "tier1-near-processor",
        PlacementTier::Tier2HostService => "tier2-host-service",
    }
}

fn timing_str(t: TimingClass) -> &'static str {
    match t {
        TimingClass::CycleExact => "cycle-exact",
        TimingClass::TimingTolerant => "timing-tolerant",
        TimingClass::Unknown => "unknown",
    }
}

impl Display for PlacementTableDisplay<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "placement [profile={}]", self.profile.name)?;
        let mut last_fn: Option<FunctionId> = None;
        for ((fid, bid), (tier, timing)) in &self.table.blocks {
            if last_fn != Some(*fid) {
                let func = self.prog.function(*fid);
                writeln!(f, "  fn{} {}", fid.0, func.name)?;
                last_fn = Some(*fid);
            }
            writeln!(f, "    b{}: {}, {}", bid.0, tier_str(*tier), timing_str(*timing))?;
        }
        if self.table.diagnostics.is_empty() {
            writeln!(f, "  diagnostics: none")?;
        } else {
            writeln!(f, "  diagnostics:")?;
            for d in &self.table.diagnostics {
                writeln!(f, "    {d}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BasicBlock, IrType, PortRef, PredSrc, TypedLocal, WaitMode};

    fn port(access: PortAccess) -> PortRef {
        PortRef {
            testbench_field: "dut".to_string(),
            port_path: vec!["p".to_string()],
            direction: None,
            width: None,
            access,
            lane: None,
        }
    }

    fn lit(v: u64) -> Expr {
        Expr::Literal {
            value: v,
            ty: IrType::Unknown,
        }
    }

    fn block(stmts: Vec<Stmt>, terminator: Terminator) -> BasicBlock {
        BasicBlock { stmts, terminator }
    }

    fn prog_with_run(blocks: Vec<BasicBlock>, locals: Vec<TypedLocal>) -> TbProgram {
        TbProgram {
            functions: vec![TbFunction {
                id: FunctionId(0),
                name: "run_T".to_string(),
                kind: FunctionKind::Run,
                params: vec![],
                locals,
                blocks,
                entry: BlockId(0),
                owner: None,
                ret: None,
            }],
            ..Default::default()
        }
    }

    fn entry_class(table: &PlacementTable) -> (PlacementTier, TimingClass) {
        table.blocks[&(FunctionId(0), BlockId(0))]
    }

    #[test]
    fn pin_access_anchored_by_wait_cycles_is_cycle_exact() {
        let prog = prog_with_run(
            vec![
                block(
                    vec![Stmt::DutWrite(port(PortAccess::Port), lit(1))],
                    Terminator::WaitCycles(lit(3), None, BlockId(1)),
                ),
                block(vec![], Terminator::Return),
            ],
            vec![],
        );
        let table = run(&prog, &TargetProfile::single_site());
        let (tier, timing) = entry_class(&table);
        assert_eq!(timing, TimingClass::CycleExact);
        assert_eq!(tier, PlacementTier::Tier0CoLocated);
        assert!(table.diagnostics.is_empty());
    }

    #[test]
    fn wait_until_region_is_timing_tolerant() {
        let prog = prog_with_run(
            vec![
                block(
                    vec![Stmt::DutWrite(port(PortAccess::Port), lit(1))],
                    Terminator::WaitUntil {
                        preds: vec![PredSrc {
                            expr: Expr::Port(port(PortAccess::Port)),
                            src_text: "dut.p == 1".to_string(),
                        }],
                        mode: WaitMode::Single,
                        succ: BlockId(1),
                    },
                ),
                block(vec![], Terminator::Return),
            ],
            vec![],
        );
        let table = run(&prog, &TargetProfile::single_site());
        assert_eq!(entry_class(&table).1, TimingClass::TimingTolerant);
    }

    #[test]
    fn pin_access_without_local_anchor_is_unknown() {
        let prog = prog_with_run(
            vec![
                block(
                    vec![Stmt::DutWrite(port(PortAccess::Port), lit(1))],
                    Terminator::Jump(BlockId(1)),
                ),
                block(vec![], Terminator::Return),
            ],
            vec![],
        );
        let table = run(&prog, &TargetProfile::single_site());
        assert_eq!(entry_class(&table).1, TimingClass::Unknown);
    }

    #[test]
    fn host_service_only_block_is_tier2() {
        let prog = prog_with_run(
            vec![block(
                vec![Stmt::Log {
                    level: crate::ir::LogLevel::Info,
                    args: FmtArgs {
                        fmt: "hello".to_string(),
                        args: vec![],
                    },
                }],
                Terminator::Return,
            )],
            vec![],
        );
        let table = run(&prog, &TargetProfile::single_site());
        assert_eq!(entry_class(&table).0, PlacementTier::Tier2HostService);
    }

    #[test]
    fn single_site_profile_never_diagnoses() {
        // Probe read + force write + cycle-exact anchor — the maximally
        // demanding block; single-site must still be diagnostic-free.
        let prog = prog_with_run(
            vec![
                block(
                    vec![
                        Stmt::DutRead(crate::ir::LocalId(0), port(PortAccess::Probe)),
                        Stmt::DutWrite(port(PortAccess::Force), lit(1)),
                    ],
                    Terminator::WaitCycles(lit(1), None, BlockId(1)),
                ),
                block(vec![], Terminator::Return),
            ],
            vec![TypedLocal {
                name: "t".to_string(),
                ty: IrType::Unknown,
            }],
        );
        let table = run(&prog, &TargetProfile::single_site());
        assert!(
            table.diagnostics.is_empty(),
            "single-site must never diagnose: {:?}",
            table.diagnostics
        );
    }

    #[test]
    fn split_strict_diagnoses_probe_force_and_cycle_exact() {
        let prog = prog_with_run(
            vec![
                block(
                    vec![
                        Stmt::DutRead(crate::ir::LocalId(0), port(PortAccess::Probe)),
                        Stmt::DutWrite(port(PortAccess::Force), lit(1)),
                    ],
                    Terminator::WaitCycles(lit(1), None, BlockId(1)),
                ),
                block(vec![], Terminator::Return),
            ],
            vec![TypedLocal {
                name: "t".to_string(),
                ty: IrType::Unknown,
            }],
        );
        let table = run(&prog, &TargetProfile::split_strict());
        let msgs: Vec<String> = table.diagnostics.iter().map(|d| d.to_string()).collect();
        assert_eq!(msgs.len(), 3, "probe + force + cycle-exact: {msgs:?}");
        assert!(msgs[0].contains("probe signal"), "{msgs:?}");
        assert!(msgs[1].contains("force point"), "{msgs:?}");
        assert!(msgs[2].contains("no cycle-locked execution site"), "{msgs:?}");
    }

    #[test]
    fn plain_port_access_is_reachable_under_split_strict() {
        let prog = prog_with_run(
            vec![
                block(
                    vec![Stmt::DutWrite(port(PortAccess::Port), lit(1))],
                    Terminator::WaitUntil {
                        preds: vec![PredSrc {
                            expr: Expr::Port(port(PortAccess::Port)),
                            src_text: "dut.p".to_string(),
                        }],
                        mode: WaitMode::Single,
                        succ: BlockId(1),
                    },
                ),
                block(vec![], Terminator::Return),
            ],
            vec![],
        );
        let table = run(&prog, &TargetProfile::split_strict());
        assert!(
            table.diagnostics.is_empty(),
            "timing-tolerant architectural-port region runs anywhere: {:?}",
            table.diagnostics
        );
    }

    #[test]
    fn helpers_carry_no_placement() {
        let mut prog = prog_with_run(vec![block(vec![], Terminator::Return)], vec![]);
        prog.functions.push(TbFunction {
            id: FunctionId(1),
            name: "helper".to_string(),
            kind: FunctionKind::Helper,
            params: vec![],
            locals: vec![],
            blocks: vec![block(vec![], Terminator::Return)],
            entry: BlockId(0),
            owner: None,
            ret: None,
        });
        let table = run(&prog, &TargetProfile::single_site());
        assert!(table.blocks.contains_key(&(FunctionId(0), BlockId(0))));
        assert!(!table.blocks.contains_key(&(FunctionId(1), BlockId(0))));
    }

    #[test]
    fn profile_by_name_resolves_both_spellings() {
        assert!(TargetProfile::by_name("single-site").is_some());
        assert!(TargetProfile::by_name("split_strict").is_some());
        assert!(TargetProfile::by_name("gpu").is_none());
    }
}
