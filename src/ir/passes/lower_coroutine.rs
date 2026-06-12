//! `lower_coroutine` — CFG → tagged FSM (docs/tb-ir-design.md
//! §"`lower_coroutine`", docs/tb-ir-plan.md §"Passes").
//!
//! Gives every *resume point* of a coroutine-shaped `TbFunction` an
//! explicit FSM state ID and computes the transition table between
//! those states, so FSM-shaped backends (per-instance device FSMs,
//! firmware task schedulers) can emit `switch (state)` dispatch
//! directly instead of re-deriving control flow from the CFG.
//!
//! Resume points are the entry block plus every successor of a
//! suspending terminator (`WaitCycles`, `WaitUntil`, and both edges of
//! `WaitUntilTimeout`). Non-suspending control flow between resume
//! points (`Jump`/`Branch` chains) is collapsed into the transition
//! entries: each transition carries the conjunction of branch
//! conditions taken on its path (the condition summary), the suspend
//! trigger that fires it, and the destination state.
//!
//! The design doc's normalized form — "every terminator either jumps
//! within the same state or suspends and lists its resume successor by
//! `BlockId`" — already holds by construction in the MVP IR (suspends
//! are `Terminator`s and name their successors), so this pass has
//! nothing to rewrite: it is read-only and returns `CoroutineMetadata`
//! as a side-table keyed by function. `cpp_tb`/tbir keep consuming the
//! CFG directly and ignore the metadata.
//!
//! Tagged kinds: `Run`, `Check`, and `SamplerAuto` (the coroutine-
//! shaped functions). `Helper` functions are skipped — impure helpers
//! are CFG-inlined at lowering time and pure helpers emit as plain
//! call-by-value functions, never as coroutines.

use super::super::{
    BlockId, Expr, FunctionId, FunctionKind, PredSrc, TbFunction, TbProgram, Terminator,
    WaitClock, WaitMode,
};
use crate::ir::display::{expr_str, mode_str};
use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};

/// Index into a function's `state_enum` vec. State `s0` is always the
/// entry (reset) state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StateId(pub u32);

/// Side-table output of the pass (design-doc field names). Both maps
/// are `BTreeMap` keyed by `FunctionId` and the per-function vecs are
/// in deterministic block order, so iteration — and the rendered dump
/// — is byte-stable across runs.
#[derive(Debug, Clone, Default)]
pub struct CoroutineMetadata {
    /// Per function: `state_enum[f][i]` is the resume block of state
    /// `s_i`. Entry first, remaining resume points in block order.
    pub state_enum: BTreeMap<FunctionId, Vec<BlockId>>,
    /// Per function: transitions grouped by source state (in state
    /// order), then-edge before else-edge within a state.
    pub transition_table: BTreeMap<FunctionId, Vec<Transition>>,
}

/// One edge of the tagged FSM: from `from`, when `trigger` fires and
/// the collapsed path's branch conditions (`guard`) held, control
/// resumes at `to` (`None` = the function terminated).
#[derive(Debug, Clone)]
pub struct Transition {
    pub from: StateId,
    /// Condition summary: conjunction of the branch conditions taken
    /// on the collapsed `Jump`/`Branch` path from the resume block to
    /// the trigger terminator. Empty = unconditional.
    pub guard: Vec<GuardTerm>,
    pub trigger: Trigger,
    /// `None` for `Done`/`Fatal` triggers.
    pub to: Option<StateId>,
}

/// One conjunct of a condition summary: a `Branch` condition and which
/// edge the path took (`taken == false` means the condition was false).
#[derive(Debug, Clone)]
pub struct GuardTerm {
    pub cond: Expr,
    pub taken: bool,
}

/// The suspend (or termination) event that fires a transition.
#[derive(Debug, Clone)]
pub enum Trigger {
    /// `WaitCycles(n)` elapsed. The clock qualifier (None = primary
    /// clock) only affects which clock's rising edges are counted —
    /// the FSM shape is identical, so the pass treats both forms the
    /// same and the qualifier surfaces only in the rendered trigger.
    CyclesElapsed(Expr, Option<WaitClock>),
    /// `WaitUntil` / `WaitUntilTimeout` predicate(s) became true.
    PredsHold { preds: Vec<PredSrc>, mode: WaitMode },
    /// `WaitUntilTimeout` cycle budget expired first.
    Timeout { cycles: Expr },
    /// `Return` reached — coroutine finished.
    Done,
    /// `Fatal` reached — simulation aborts.
    Fatal,
}

/// Structured pass failure — never a panic (mirrors
/// `lower::unsupported` style: name the construct, point at the fix).
#[derive(Debug, Clone)]
pub enum LowerCoroutineError {
    /// A `Jump`/`Branch` cycle with no suspending terminator on it:
    /// the zero-time loop can't be collapsed into finitely many
    /// transition entries.
    UnsuspendedLoop {
        func: FunctionId,
        name: String,
        block: BlockId,
    },
}

impl Display for LowerCoroutineError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            LowerCoroutineError::UnsuspendedLoop { func, name, block } => write!(
                f,
                "lower_coroutine does not support loops without a suspension point: \
                 fn{} {} has a control-flow cycle through b{} with no `wait` on it \
                 (add a `wait` inside the loop, or drop `--pass lower-coroutine`)",
                func.0, name, block.0
            ),
        }
    }
}

impl std::error::Error for LowerCoroutineError {}

/// Tag every coroutine-shaped function (`Run`/`Check`/`SamplerAuto`)
/// in `prog`. Read-only — see the module docs for why no rewrite is
/// needed on the MVP IR.
///
/// Precondition: `prog` is `verify`-clean (block ids in range,
/// all blocks reachable) — like all passes, this runs after
/// `verify::verify_program`. Transition count is the number of
/// distinct branch paths between suspends (exponential only in the
/// length of suspend-free `Branch` chains, which TB-scale functions
/// keep small).
pub fn run(prog: &TbProgram) -> Result<CoroutineMetadata, LowerCoroutineError> {
    let mut meta = CoroutineMetadata::default();
    for func in &prog.functions {
        if !matches!(
            func.kind,
            FunctionKind::Run | FunctionKind::Check | FunctionKind::SamplerAuto { .. }
        ) {
            continue;
        }
        let (states, transitions) = tag_function(func)?;
        meta.state_enum.insert(func.id, states);
        meta.transition_table.insert(func.id, transitions);
    }
    Ok(meta)
}

/// Resume successors of a suspending terminator (empty for the
/// non-suspending kinds).
fn resume_successors(t: &Terminator) -> Vec<BlockId> {
    match t {
        Terminator::WaitCycles(_, _, succ) | Terminator::WaitUntil { succ, .. } => vec![*succ],
        Terminator::WaitUntilTimeout {
            on_fire,
            on_timeout,
            ..
        } => vec![*on_fire, *on_timeout],
        Terminator::Jump(_)
        | Terminator::Branch(..)
        | Terminator::Return
        | Terminator::Fatal(_) => vec![],
    }
}

fn tag_function(func: &TbFunction) -> Result<(Vec<BlockId>, Vec<Transition>), LowerCoroutineError> {
    let nblocks = func.blocks.len();

    // Resume points: entry + every suspend successor.
    let mut is_resume = vec![false; nblocks];
    is_resume[func.entry.index()] = true;
    for b in &func.blocks {
        for s in resume_successors(&b.terminator) {
            is_resume[s.index()] = true;
        }
    }

    // State enum: entry first (s0 = reset state), then the remaining
    // resume points in block order — deterministic by construction.
    let mut states = vec![func.entry];
    for (bi, marked) in is_resume.iter().enumerate() {
        let bid = BlockId(bi as u32);
        if *marked && bid != func.entry {
            states.push(bid);
        }
    }
    let mut state_of: Vec<Option<StateId>> = vec![None; nblocks];
    for (si, b) in states.iter().enumerate() {
        state_of[b.index()] = Some(StateId(si as u32));
    }

    // Per state, collapse the Jump/Branch chain from its resume block
    // down to each reachable suspend/termination, accumulating the
    // branch-condition summary along the way.
    let mut transitions = Vec::new();
    for (si, &resume_block) in states.iter().enumerate() {
        collapse(
            func,
            StateId(si as u32),
            resume_block,
            &mut Vec::new(),
            &mut Vec::new(),
            &state_of,
            &mut transitions,
        )?;
    }
    Ok((states, transitions))
}

/// DFS from `block` through non-suspending terminators; emits one
/// `Transition` per suspend/termination edge reached. `path` is the
/// current DFS path (cycle detection); `guard` the running condition
/// summary. Then-edge explored before else-edge for stable output
/// order.
fn collapse(
    func: &TbFunction,
    from: StateId,
    block: BlockId,
    guard: &mut Vec<GuardTerm>,
    path: &mut Vec<BlockId>,
    state_of: &[Option<StateId>],
    out: &mut Vec<Transition>,
) -> Result<(), LowerCoroutineError> {
    if path.contains(&block) {
        return Err(LowerCoroutineError::UnsuspendedLoop {
            func: func.id,
            name: func.name.clone(),
            block,
        });
    }
    path.push(block);
    // Resolves the (by-construction) state of a suspend successor.
    let state_at = |b: BlockId| state_of[b.index()];
    match &func.block(block).terminator {
        Terminator::Jump(t) => collapse(func, from, *t, guard, path, state_of, out)?,
        Terminator::Branch(cond, then_b, else_b) => {
            guard.push(GuardTerm {
                cond: cond.clone(),
                taken: true,
            });
            collapse(func, from, *then_b, guard, path, state_of, out)?;
            guard.pop();
            guard.push(GuardTerm {
                cond: cond.clone(),
                taken: false,
            });
            collapse(func, from, *else_b, guard, path, state_of, out)?;
            guard.pop();
        }
        Terminator::WaitCycles(cycles, clock, succ) => out.push(Transition {
            from,
            guard: guard.clone(),
            trigger: Trigger::CyclesElapsed(cycles.clone(), clock.clone()),
            to: state_at(*succ),
        }),
        Terminator::WaitUntil { preds, mode, succ } => out.push(Transition {
            from,
            guard: guard.clone(),
            trigger: Trigger::PredsHold {
                preds: preds.clone(),
                mode: *mode,
            },
            to: state_at(*succ),
        }),
        Terminator::WaitUntilTimeout {
            preds,
            mode,
            cycles,
            on_fire,
            on_timeout,
        } => {
            out.push(Transition {
                from,
                guard: guard.clone(),
                trigger: Trigger::PredsHold {
                    preds: preds.clone(),
                    mode: *mode,
                },
                to: state_at(*on_fire),
            });
            out.push(Transition {
                from,
                guard: guard.clone(),
                trigger: Trigger::Timeout {
                    cycles: cycles.clone(),
                },
                to: state_at(*on_timeout),
            });
        }
        Terminator::Return => out.push(Transition {
            from,
            guard: guard.clone(),
            trigger: Trigger::Done,
            to: None,
        }),
        Terminator::Fatal(_) => out.push(Transition {
            from,
            guard: guard.clone(),
            trigger: Trigger::Fatal,
            to: None,
        }),
    }
    path.pop();
    Ok(())
}

// ── Display (the `dump-ir --pass lower-coroutine` suffix) ───────────

impl CoroutineMetadata {
    /// Render against `prog` (expressions need local names). Output is
    /// deterministic: BTreeMap key order + per-function vec order.
    pub fn display<'a>(&'a self, prog: &'a TbProgram) -> CoroutineMetadataDisplay<'a> {
        CoroutineMetadataDisplay { meta: self, prog }
    }
}

pub struct CoroutineMetadataDisplay<'a> {
    meta: &'a CoroutineMetadata,
    prog: &'a TbProgram,
}

impl Display for CoroutineMetadataDisplay<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (fid, states) in &self.meta.state_enum {
            let func = self.prog.function(*fid);
            writeln!(f, "coroutine-fsm fn{} {}", fid.0, func.name)?;
            writeln!(f, "  states:")?;
            for (si, b) in states.iter().enumerate() {
                let entry = if *b == func.entry { " (entry)" } else { "" };
                writeln!(f, "    s{si} = b{}{entry}", b.0)?;
            }
            writeln!(f, "  transitions:")?;
            let empty = Vec::new();
            let transitions = self.meta.transition_table.get(fid).unwrap_or(&empty);
            for t in transitions {
                let to = match t.to {
                    Some(s) => format!("s{}", s.0),
                    None => "end".to_string(),
                };
                write!(f, "    s{} -> {} on {}", t.from.0, to, trigger_str(func, &t.trigger))?;
                if !t.guard.is_empty() {
                    let terms: Vec<String> = t
                        .guard
                        .iter()
                        .map(|g| {
                            let c = expr_str(func, &g.cond);
                            if g.taken { c } else { format!("!{c}") }
                        })
                        .collect();
                    write!(f, " if {}", terms.join(" && "))?;
                }
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

fn trigger_str(func: &TbFunction, t: &Trigger) -> String {
    match t {
        Trigger::CyclesElapsed(e, clock) => match clock {
            Some(c) => format!("wait_cycles({} on {})", expr_str(func, e), c.name),
            None => format!("wait_cycles({})", expr_str(func, e)),
        },
        Trigger::PredsHold { preds, mode } => {
            let ps: Vec<String> = preds.iter().map(|p| expr_str(func, &p.expr)).collect();
            format!("preds({}) [{}]", ps.join(", "), mode_str(mode))
        }
        Trigger::Timeout { cycles } => format!("timeout({})", expr_str(func, cycles)),
        Trigger::Done => "return".to_string(),
        Trigger::Fatal => "fatal".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BasicBlock, IrType, TypedLocal};

    fn lit(v: u64) -> Expr {
        Expr::Literal {
            value: v,
            ty: IrType::Unknown,
        }
    }

    fn local_cond(i: u32) -> Expr {
        Expr::Local(crate::ir::LocalId(i))
    }

    fn block(stmts: Vec<crate::ir::Stmt>, terminator: Terminator) -> BasicBlock {
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

    fn flag_local(name: &str) -> TypedLocal {
        TypedLocal {
            name: name.to_string(),
            ty: IrType::Bool,
        }
    }

    #[test]
    fn straight_line_wait_cycles() {
        // b0: WaitCycles(2) -> b1; b1: Return.
        let prog = prog_with_run(
            vec![
                block(vec![], Terminator::WaitCycles(lit(2), None, BlockId(1))),
                block(vec![], Terminator::Return),
            ],
            vec![],
        );
        let meta = run(&prog).expect("tags");
        let states = &meta.state_enum[&FunctionId(0)];
        assert_eq!(states, &vec![BlockId(0), BlockId(1)]);
        let ts = &meta.transition_table[&FunctionId(0)];
        assert_eq!(ts.len(), 2);
        assert_eq!(ts[0].from, StateId(0));
        assert_eq!(ts[0].to, Some(StateId(1)));
        assert!(matches!(ts[0].trigger, Trigger::CyclesElapsed(..)));
        assert!(ts[0].guard.is_empty());
        assert_eq!(ts[1].from, StateId(1));
        assert_eq!(ts[1].to, None);
        assert!(matches!(ts[1].trigger, Trigger::Done));
    }

    #[test]
    fn branch_paths_carry_condition_summary() {
        // b0: Branch(%c, b1, b2); b1/b2: WaitCycles -> b3; b3: Return.
        // Both suspend edges land in the same resume state, with
        // opposite guard polarities, then-edge first.
        let prog = prog_with_run(
            vec![
                block(
                    vec![],
                    Terminator::Branch(local_cond(0), BlockId(1), BlockId(2)),
                ),
                block(vec![], Terminator::WaitCycles(lit(1), None, BlockId(3))),
                block(vec![], Terminator::WaitCycles(lit(5), None, BlockId(3))),
                block(vec![], Terminator::Return),
            ],
            vec![flag_local("c")],
        );
        let meta = run(&prog).expect("tags");
        assert_eq!(
            meta.state_enum[&FunctionId(0)],
            vec![BlockId(0), BlockId(3)]
        );
        let ts = &meta.transition_table[&FunctionId(0)];
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].guard.len(), 1);
        assert!(ts[0].guard[0].taken, "then-edge explored first");
        assert_eq!(ts[0].to, Some(StateId(1)));
        assert_eq!(ts[1].guard.len(), 1);
        assert!(!ts[1].guard[0].taken);
        assert_eq!(ts[1].to, Some(StateId(1)));
        assert!(matches!(ts[2].trigger, Trigger::Done));
    }

    #[test]
    fn loop_with_wait_collapses_through_header() {
        // b0: Jump(b1 header); b1: Branch(%c, b2 body, b4 exit);
        // b2: WaitCycles -> b3 latch; b3: Jump(b1); b4: Return.
        // Resume points: b0 (entry) and b3 (wait successor); both
        // states re-walk the header, so each gets a loop edge and an
        // exit edge.
        let prog = prog_with_run(
            vec![
                block(vec![], Terminator::Jump(BlockId(1))),
                block(
                    vec![],
                    Terminator::Branch(local_cond(0), BlockId(2), BlockId(4)),
                ),
                block(vec![], Terminator::WaitCycles(lit(1), None, BlockId(3))),
                block(vec![], Terminator::Jump(BlockId(1))),
                block(vec![], Terminator::Return),
            ],
            vec![flag_local("c")],
        );
        let meta = run(&prog).expect("tags");
        assert_eq!(
            meta.state_enum[&FunctionId(0)],
            vec![BlockId(0), BlockId(3)]
        );
        let ts = &meta.transition_table[&FunctionId(0)];
        assert_eq!(ts.len(), 4);
        // s0: loop edge then exit edge.
        assert_eq!((ts[0].from, ts[0].to), (StateId(0), Some(StateId(1))));
        assert!(ts[0].guard[0].taken);
        assert!(matches!(ts[1].trigger, Trigger::Done));
        assert!(!ts[1].guard[0].taken);
        // s1: same shape — the latch re-walks the header.
        assert_eq!((ts[2].from, ts[2].to), (StateId(1), Some(StateId(1))));
        assert!(matches!(ts[3].trigger, Trigger::Done));
        assert_eq!(ts[3].from, StateId(1));
    }

    #[test]
    fn wait_until_timeout_yields_fire_and_timeout_edges() {
        // b0: WaitUntilTimeout { fire: b1, timeout: b2 };
        // b1: Return; b2: Jump(b1).
        let prog = prog_with_run(
            vec![
                block(
                    vec![],
                    Terminator::WaitUntilTimeout {
                        preds: vec![PredSrc {
                            expr: local_cond(0),
                            src_text: "c".to_string(),
                        }],
                        mode: WaitMode::Single,
                        cycles: lit(100),
                        on_fire: BlockId(1),
                        on_timeout: BlockId(2),
                    },
                ),
                block(vec![], Terminator::Return),
                block(vec![], Terminator::Jump(BlockId(1))),
            ],
            vec![flag_local("c")],
        );
        let meta = run(&prog).expect("tags");
        assert_eq!(
            meta.state_enum[&FunctionId(0)],
            vec![BlockId(0), BlockId(1), BlockId(2)]
        );
        let ts = &meta.transition_table[&FunctionId(0)];
        assert_eq!(ts.len(), 4);
        assert!(matches!(&ts[0].trigger, Trigger::PredsHold { preds, mode }
            if preds.len() == 1 && *mode == WaitMode::Single));
        assert_eq!(ts[0].to, Some(StateId(1)));
        assert!(matches!(ts[1].trigger, Trigger::Timeout { .. }));
        assert_eq!(ts[1].to, Some(StateId(2)));
        // s1 returns; s2 (timeout handler) falls through to s1's
        // Return — a done edge of its own.
        assert!(matches!(ts[2].trigger, Trigger::Done));
        assert_eq!(ts[2].from, StateId(1));
        assert!(matches!(ts[3].trigger, Trigger::Done));
        assert_eq!(ts[3].from, StateId(2));
    }

    #[test]
    fn unsuspended_loop_is_a_structured_error() {
        // b0: Jump(b1); b1: Jump(b0) — zero-time spin, not taggable.
        let prog = prog_with_run(
            vec![
                block(vec![], Terminator::Jump(BlockId(1))),
                block(vec![], Terminator::Jump(BlockId(0))),
            ],
            vec![],
        );
        let err = run(&prog).expect_err("must reject");
        let LowerCoroutineError::UnsuspendedLoop { func, block, .. } = &err;
        assert_eq!(*func, FunctionId(0));
        assert_eq!(*block, BlockId(0));
        let msg = err.to_string();
        assert!(msg.contains("loops without a suspension point"), "{msg}");
        assert!(msg.contains("b0"), "names the block: {msg}");
        assert!(msg.contains("run_T"), "names the function: {msg}");
    }

    #[test]
    fn helper_functions_are_skipped() {
        let mut prog = prog_with_run(vec![block(vec![], Terminator::Return)], vec![]);
        prog.functions.push(TbFunction {
            id: FunctionId(1),
            name: "double_it".to_string(),
            kind: FunctionKind::Helper,
            params: vec![],
            locals: vec![],
            blocks: vec![block(vec![], Terminator::Return)],
            entry: BlockId(0),
            owner: None,
            ret: None,
        });
        let meta = run(&prog).expect("tags");
        assert!(meta.state_enum.contains_key(&FunctionId(0)));
        assert!(!meta.state_enum.contains_key(&FunctionId(1)));
        assert!(!meta.transition_table.contains_key(&FunctionId(1)));
    }

    #[test]
    fn fatal_terminator_yields_fatal_edge() {
        // b0: WaitCycles -> b1; b1: Fatal — terminal, no target state.
        let prog = prog_with_run(
            vec![
                block(vec![], Terminator::WaitCycles(lit(1), None, BlockId(1))),
                block(
                    vec![crate::ir::Stmt::Log {
                        level: crate::ir::LogLevel::Fatal,
                        args: crate::ir::FmtArgs {
                            fmt: "boom".to_string(),
                            args: vec![],
                        },
                    }],
                    Terminator::Fatal(crate::ir::FmtArgs {
                        fmt: "boom".to_string(),
                        args: vec![],
                    }),
                ),
            ],
            vec![],
        );
        let meta = run(&prog).expect("tags");
        let ts = &meta.transition_table[&FunctionId(0)];
        assert!(matches!(ts[1].trigger, Trigger::Fatal));
        assert_eq!(ts[1].to, None);
    }
}
