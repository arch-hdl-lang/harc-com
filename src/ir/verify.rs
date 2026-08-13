//! TB-IR structural verifier — design-doc invariants 1-8, 10, 15 plus
//! the port-position rule (an `Expr::Port` may appear only in wait
//! predicates, format-arg expressions, `DutRead`/`DutWrite` operands,
//! `AssertCheck` condition subtrees, and `FailDiag` guards — which
//! re-evaluate a wait predicate after the wait timed out) and the
//! transactor-call seam rule (below).
//!
//! **Transactor-call seam rule.** A `CallTarget::TransactorMethod`
//! call edge is deliberately never inlined at the IR level — the
//! sequence→transactor boundary is the placement cut every split
//! backend needs (design doc §CallTarget). The verifier pins the edge
//! to the one position backends expand: the ENTIRE right-hand side of
//! a `Stmt::Assign` in a `Run`/`Check` function, with a `bus_field`/
//! `method` pair that resolves against the owning testbench's
//! `bus_bindings` at the declared arity. Anywhere else — nested in an
//! expression, in a format arg or wait predicate, or inside a
//! `Helper`/`SamplerAuto` body (pure helpers must stay suspension-
//! free and placement-neutral) — is a lowering bug. The edge is also
//! the sanctioned exception to "no statement may suspend": its
//! suspension lives behind the call boundary, which placement
//! classifies as timing-tolerant by construction.
//!
//! Violations are programmer errors (lowering bugs or pass bugs), not
//! user errors — user errors are rejected earlier by the lowering pass.
//!
//! Two deliberate deviations from the doc's literal text:
//! - Invariant 5 ("exactly one terminator") and invariant 7 ("no
//!   suspending Stmt") hold by construction — `BasicBlock` has one
//!   `terminator` field and `Stmt` has no suspending variant — so no
//!   runtime check is needed.
//! - Invariant 8 permits empty blocks terminated by `Branch` or a
//!   suspension (`WaitCycles`/`WaitUntil`/`WaitUntilTimeout`) in
//!   addition to `Return`/`Jump`: loop headers are empty-by-design
//!   branch blocks (see the doc's own worked example 2, `b_header`),
//!   and a loop body whose first statement is `wait N cycles` lowers
//!   to an empty block whose terminator IS the content. Only an empty
//!   `Fatal` block remains flagged — the design synthesizes the fail
//!   action into that block's statements, so emptiness there means the
//!   synthesis dropped its body.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// Invariant 1: entry resolves to a block.
    BadEntry { func: FunctionId, entry: BlockId },
    /// Invariant 2: every block reachable from entry.
    UnreachableBlock { func: FunctionId, block: BlockId },
    /// Invariant 3: every LocalId resolves.
    BadLocal {
        func: FunctionId,
        block: BlockId,
        local: LocalId,
    },
    /// Invariant 4: defs dominate uses.
    LocalUseBeforeDef {
        func: FunctionId,
        block: BlockId,
        local: LocalId,
    },
    /// Invariant 6: terminator successors resolve.
    BadSuccessor {
        func: FunctionId,
        block: BlockId,
        succ: BlockId,
    },
    /// Invariant 8 (amended): empty `Fatal` block — the synthesized
    /// fail action went missing.
    EmptyBlock { func: FunctionId, block: BlockId },
    /// Invariant 10: covgroup references resolve.
    BadCovgroup {
        func: FunctionId,
        block: BlockId,
        covgroup: CovgroupId,
    },
    /// Invariant 15: Assign type matches the local's declared type
    /// (only checked when both sides are known).
    TypeMismatch {
        func: FunctionId,
        block: BlockId,
        local: LocalId,
        expected: IrType,
        actual: IrType,
    },
    /// WidthCast nodes carry language-level bit widths even when they are
    /// constructed by a compiler pass rather than source lowering.
    BadWidthCast {
        func: FunctionId,
        block: BlockId,
        width: u32,
        src_width: Option<u32>,
    },
    /// Record references resolve: the `RecordId` indexes the records
    /// table and record-typed locals carry the matching `IrType`.
    BadRecord {
        func: FunctionId,
        block: BlockId,
        record: RecordId,
    },
    /// A record field access names a field the schema does not have,
    /// or targets a local that is not record-typed.
    BadRecordField {
        func: FunctionId,
        block: BlockId,
        local: LocalId,
        field: String,
    },
    /// A `TbField`/`TbFieldWrite` names a scalar field the owning
    /// testbench schema does not declare (or the function has no
    /// owning testbench at all — helpers cannot touch TB state).
    BadTbField {
        func: FunctionId,
        block: BlockId,
        field: String,
    },
    /// Port-position rule: `Expr::Port` outside an allowed position.
    PortInDisallowedPosition {
        func: FunctionId,
        block: BlockId,
        context: &'static str,
    },
    /// Transactor-call seam rule (module docs). A
    /// `CallTarget::TransactorMethod` edge must resolve in exactly one
    /// namespace of the owning testbench, in its sanctioned position:
    /// bus binding → the entire `Assign` RHS of a Run/Check function;
    /// transactor field → the payload of a `Stmt::TransactorCall`.
    /// Violations: an edge nested in expression position (it can
    /// advance simulated time — never an expression value), a
    /// `Stmt::TransactorCall` payload that is not a call edge, or an
    /// edge that resolves in neither/the wrong namespace.
    BadTransactorCall {
        func: FunctionId,
        block: BlockId,
        detail: String,
    },
    /// A scoreboard op/query references a scoreboard id, testbench field,
    /// or scoreboard field that does not resolve (or names a queue where
    /// a scalar is expected, or vice versa).
    BadScoreboard {
        func: FunctionId,
        block: BlockId,
        detail: String,
    },
    /// Cross-IR: a test's run/check FunctionId or TestbenchId resolves.
    BadProgramRef { what: String },
    /// Invariant 9: a `Terminator::Randomize`'s `ConstraintRef` must
    /// index `TbProgram::constraint_sites`, and its `target` local must
    /// be record-typed (the solver writes record fields back into it).
    DanglingConstraintRef {
        func: FunctionId,
        block: BlockId,
        detail: String,
    },
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::BadEntry { func, entry } => {
                write!(f, "fn{}: entry b{} does not resolve", func.0, entry.0)
            }
            VerifyError::UnreachableBlock { func, block } => {
                write!(f, "fn{}: block b{} unreachable from entry", func.0, block.0)
            }
            VerifyError::BadLocal { func, block, local } => write!(
                f,
                "fn{}: b{} references undeclared local %{}",
                func.0, block.0, local.0
            ),
            VerifyError::LocalUseBeforeDef { func, block, local } => write!(
                f,
                "fn{}: b{} reads local %{} before any definition dominates it",
                func.0, block.0, local.0
            ),
            VerifyError::BadSuccessor { func, block, succ } => write!(
                f,
                "fn{}: b{} terminator targets missing block b{}",
                func.0, block.0, succ.0
            ),
            VerifyError::EmptyBlock { func, block } => write!(
                f,
                "fn{}: b{} is an empty Fatal block (synthesized fail action missing)",
                func.0, block.0
            ),
            VerifyError::BadCovgroup {
                func,
                block,
                covgroup,
            } => write!(
                f,
                "fn{}: b{} references missing covgroup cg{}",
                func.0, block.0, covgroup.0
            ),
            VerifyError::TypeMismatch {
                func,
                block,
                local,
                expected,
                actual,
            } => write!(
                f,
                "fn{}: b{} assigns {:?} into local %{} declared {:?}",
                func.0, block.0, actual, local.0, expected
            ),
            VerifyError::BadWidthCast {
                func,
                block,
                width,
                src_width,
            } => write!(
                f,
                "fn{}: b{} has invalid width cast destination {} and source {:?}",
                func.0, block.0, width, src_width
            ),
            VerifyError::BadRecord {
                func,
                block,
                record,
            } => write!(
                f,
                "fn{}: b{} references missing or mismatched record r{}",
                func.0, block.0, record.0
            ),
            VerifyError::BadRecordField {
                func,
                block,
                local,
                field,
            } => write!(
                f,
                "fn{}: b{} accesses field `{field}` on local %{} (not a record-typed local or no such field)",
                func.0, block.0, local.0
            ),
            VerifyError::BadTbField { func, block, field } => write!(
                f,
                "fn{}: b{} accesses testbench scalar field `{field}` that the owning \
                 testbench does not declare",
                func.0, block.0
            ),
            VerifyError::PortInDisallowedPosition {
                func,
                block,
                context,
            } => write!(
                f,
                "fn{}: b{} contains a DUT port read in a disallowed position ({context})",
                func.0, block.0
            ),
            VerifyError::BadTransactorCall {
                func,
                block,
                detail,
            } => write!(
                f,
                "fn{}: b{} transactor-call seam violation: {detail}",
                func.0, block.0
            ),
            VerifyError::BadScoreboard { func, block, detail } => write!(
                f,
                "fn{}: b{} scoreboard reference error: {detail}",
                func.0, block.0
            ),
            VerifyError::BadProgramRef { what } => write!(f, "program: {what}"),
            VerifyError::DanglingConstraintRef {
                func,
                block,
                detail,
            } => write!(
                f,
                "f{} b{}: dangling Randomize constraint ref ({detail})",
                func.0, block.0
            ),
        }
    }
}

pub fn verify_program(prog: &TbProgram) -> Result<(), Vec<VerifyError>> {
    let mut errs = Vec::new();
    for t in &prog.tests {
        if t.testbench.index() >= prog.testbenches.len() {
            errs.push(VerifyError::BadProgramRef {
                what: format!("test {} references missing tb{}", t.name, t.testbench.0),
            });
        }
        if t.run.index() >= prog.functions.len() {
            errs.push(VerifyError::BadProgramRef {
                what: format!("test {} references missing run fn{}", t.name, t.run.0),
            });
        }
        if let Some(c) = t.check {
            if c.index() >= prog.functions.len() {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("test {} references missing check fn{}", t.name, c.0),
                });
            }
        }
        // Cross-IR: every clock-qualified WaitCycles in the test's
        // functions must name a clock the test actually declares
        // (index in range AND name agreement — lowering resolves both
        // together, so disagreement means a pass corrupted the IR).
        // Codegen indexes the runtime clock vector with `index`
        // unchecked; this is the net that keeps that sound.
        for fid in [Some(t.run), t.check].into_iter().flatten() {
            let Some(func) = prog.functions.get(fid.index()) else {
                continue; // missing fn already reported above
            };
            for (bi, b) in func.blocks.iter().enumerate() {
                let Terminator::WaitCycles(_, Some(wc), _) = &b.terminator else {
                    continue;
                };
                match t.clocks.get(wc.index) {
                    Some(spec) if spec.name == wc.name => {}
                    Some(spec) => errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "test {}: fn{} b{bi} waits on clock `{}` at index {} but \
                             that slot is `{}`",
                            t.name, fid.0, wc.name, wc.index, spec.name
                        ),
                    }),
                    None => errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "test {}: fn{} b{bi} waits on clock `{}` at index {} but \
                             only {} clock(s) are declared",
                            t.name,
                            fid.0,
                            wc.name,
                            wc.index,
                            t.clocks.len()
                        ),
                    }),
                }
            }
        }
    }
    // Covergroup schemas: declared crosses must reference 2+ existing
    // points, all binned (lowering validates this; a pass that edits
    // schemas must not break it — emission indexes `points` directly).
    for (ci, cg) in prog.covgroups.iter().enumerate() {
        for cross in &cg.crosses {
            if cross.point_indices.len() < 2 {
                errs.push(VerifyError::BadProgramRef {
                    what: format!("cg{ci} cross has fewer than two points"),
                });
            }
            for &pi in &cross.point_indices {
                if pi >= cg.points.len() {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!("cg{ci} cross references missing point index {pi}"),
                    });
                } else if cg.points[pi].bins.is_empty() {
                    errs.push(VerifyError::BadProgramRef {
                        what: format!(
                            "cg{ci} cross references binless point `{}`",
                            cg.points[pi].name
                        ),
                    });
                }
            }
        }
    }
    // Transactor schemas: every method's FunctionId resolves to a
    // function tagged `TransactorBody` for that transactor, and every
    // testbench transactor field resolves. Emission indexes both
    // tables directly off these links.
    for (xi, x) in prog.transactors.iter().enumerate() {
        for m in &x.methods {
            match prog.functions.get(m.function.index()) {
                Some(f)
                    if f.kind
                        == (FunctionKind::TransactorBody {
                            transactor: TransactorId(xi as u32),
                        }) => {}
                Some(f) => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} method `{}` points at fn{} with kind {:?}",
                        m.name, m.function.0, f.kind
                    ),
                }),
                None => errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "transactor x{xi} method `{}` references missing fn{}",
                        m.name, m.function.0
                    ),
                }),
            }
        }
    }
    for (ti, tb) in prog.testbenches.iter().enumerate() {
        for (field, xid) in &tb.transactor_fields {
            if xid.index() >= prog.transactors.len() {
                errs.push(VerifyError::BadProgramRef {
                    what: format!(
                        "tb{ti} transactor field `{field}` references missing x{}",
                        xid.0
                    ),
                });
            }
        }
    }
    for (i, func) in prog.functions.iter().enumerate() {
        if func.id.index() != i {
            errs.push(VerifyError::BadProgramRef {
                what: format!("fn at index {i} carries id fn{}", func.id.0),
            });
        }
        if let Err(mut e) = verify_function(prog, func) {
            errs.append(&mut e);
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

pub fn verify_function(prog: &TbProgram, func: &TbFunction) -> Result<(), Vec<VerifyError>> {
    let mut errs = Vec::new();
    let nblocks = func.blocks.len();
    let fid = func.id;

    // Invariant 1.
    if func.entry.index() >= nblocks {
        errs.push(VerifyError::BadEntry {
            func: fid,
            entry: func.entry,
        });
        return Err(errs); // nothing else is meaningful
    }

    // Invariant 6 — successors resolve (checked before reachability so
    // the walk below can't index out of bounds).
    for (bi, b) in func.blocks.iter().enumerate() {
        for s in b.terminator.successors() {
            if s.index() >= nblocks {
                errs.push(VerifyError::BadSuccessor {
                    func: fid,
                    block: BlockId(bi as u32),
                    succ: s,
                });
            }
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }

    // Invariant 2 — reachability.
    let mut reachable = vec![false; nblocks];
    let mut work = vec![func.entry];
    while let Some(b) = work.pop() {
        if std::mem::replace(&mut reachable[b.index()], true) {
            continue;
        }
        work.extend(func.block(b).terminator.successors());
    }
    for (bi, r) in reachable.iter().enumerate() {
        if !r {
            errs.push(VerifyError::UnreachableBlock {
                func: fid,
                block: BlockId(bi as u32),
            });
        }
    }

    // Invariant 8 (amended — see module docs).
    for (bi, b) in func.blocks.iter().enumerate() {
        if b.stmts.is_empty() && matches!(b.terminator, Terminator::Fatal(_)) {
            errs.push(VerifyError::EmptyBlock {
                func: fid,
                block: BlockId(bi as u32),
            });
        }
    }

    // Invariants 3, 10, 15 + port positions, per block.
    for (bi, b) in func.blocks.iter().enumerate() {
        let bid = BlockId(bi as u32);
        let mut ck = Checker {
            prog,
            func,
            fid,
            bid,
            errs: &mut errs,
        };
        ck.check_block(b);
    }

    // Invariant 4 — forward dataflow: a local must be defined on every
    // path from entry before its first read. Params count as defined.
    check_def_before_use(func, fid, &reachable, &mut errs);

    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

struct Checker<'a> {
    prog: &'a TbProgram,
    func: &'a TbFunction,
    fid: FunctionId,
    bid: BlockId,
    errs: &'a mut Vec<VerifyError>,
}

impl Checker<'_> {
    fn check_block(&mut self, b: &BasicBlock) {
        for s in &b.stmts {
            match s {
                Stmt::Assign(l, e) => {
                    self.check_local(*l);
                    // Transactor-call seam rule: the one sanctioned
                    // position for a TransactorMethod call edge is the
                    // entire Assign RHS of a Run/Check function. Args
                    // are checked individually (no ports, no nesting);
                    // `check_expr` rejects the target everywhere else.
                    if let Expr::Call(CallTarget::TransactorMethod { bus_field, method }, args) = e
                    {
                        self.check_bus_call_edge(bus_field, method, args);
                        continue;
                    }
                    self.check_expr(e, false, "Assign value");
                    // Invariant 15.
                    if self.func.locals.get(l.index()).is_some() {
                        let expected = &self.func.local(*l).ty;
                        if let Some(actual) = expr_type(self.func, e) {
                            if *expected != IrType::Unknown
                                && actual != IrType::Unknown
                                && !assign_compatible(expected, &actual)
                            {
                                self.errs.push(VerifyError::TypeMismatch {
                                    func: self.fid,
                                    block: self.bid,
                                    local: *l,
                                    expected: expected.clone(),
                                    actual,
                                });
                            }
                        }
                    }
                }
                Stmt::DutWrite(_, e) => self.check_expr(e, true, "DutWrite value"),
                Stmt::DutRead(l, _) => self.check_local(*l),
                // `release dut.<probe>` carries no value and no local;
                // the PortRef's access class is validated at lowering.
                Stmt::ProbeRelease(_) => {}
                Stmt::RecordInit(l, r) => {
                    self.check_local(*l);
                    if r.index() >= self.prog.records.len()
                        || self
                            .func
                            .locals
                            .get(l.index())
                            .is_some_and(|tl| tl.ty != IrType::Record(*r))
                    {
                        self.errs.push(VerifyError::BadRecord {
                            func: self.fid,
                            block: self.bid,
                            record: *r,
                        });
                    }
                }
                Stmt::RecordFieldWrite {
                    local,
                    field,
                    path,
                    mid_indices,
                    index,
                    value,
                } => {
                    self.check_local(*local);
                    let mid_positions: Vec<usize> =
                        mid_indices.iter().map(|(p, _)| *p).collect();
                    self.check_record_field(*local, field, path, &mid_positions);
                    for (_, idx) in mid_indices {
                        self.check_expr(idx, false, "RecordFieldWrite mid index");
                    }
                    if let Some(idx) = index {
                        self.check_expr(idx, false, "RecordFieldWrite index");
                    }
                    self.check_expr(value, false, "RecordFieldWrite value");
                }
                Stmt::RecordWriteCb {
                    local,
                    field,
                    value,
                    ..
                } => {
                    self.check_local(*local);
                    self.check_record_field(*local, field, &[], &[]);
                    self.check_expr(value, false, "RecordWriteCb value");
                }
                Stmt::TbFieldWrite { field, value } => {
                    self.check_tb_field(field);
                    self.check_expr(value, false, "TbFieldWrite value");
                }
                Stmt::TransactorStateWrite { value, .. } => {
                    // Instance/field resolution is a lowering concern
                    // (the verifier has no transactor-binding context);
                    // just hold the value to the no-inline-port rule.
                    self.check_expr(value, false, "TransactorStateWrite value");
                }
                Stmt::TransactorStateRecordFieldWrite { value, .. } => {
                    self.check_expr(value, false, "TransactorStateRecordFieldWrite value");
                }
                Stmt::TransactorStateQueuePush { value, .. } => {
                    // Target-state queue host state — the pushed value
                    // follows the no-inline-port rule like any Assign value.
                    self.check_expr(value, false, "TransactorStateQueuePush value");
                }
                Stmt::TransactorStateQueuePop { dest, .. } => {
                    self.check_local(*dest);
                }
                Stmt::Log { args, .. } => self.check_fmt_args(args),
                Stmt::AssertCheck { cond, on_fail } => {
                    self.check_expr(cond, true, "AssertCheck cond");
                    self.check_fmt_args(on_fail);
                }
                Stmt::CovReport(inst) => self.check_covgroup(inst.covgroup),
                Stmt::TransactorCall { dest, call } => {
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                    self.check_transactor_call(call);
                }
                Stmt::TransactorSelfCall { dest, call } => {
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                    self.check_transactor_self_call(*dest, call);
                }
                Stmt::FailDiag { guard, args } => {
                    if let Some(g) = guard {
                        self.check_expr(g, true, "FailDiag guard");
                    }
                    self.check_fmt_args(args);
                }
                Stmt::ScoreboardOp {
                    sb,
                    field,
                    op,
                    nested_path,
                } => {
                    self.check_scoreboard(*sb, field, nested_path.is_some());
                    match op {
                        crate::ir::ScoreboardOp::QueuePush { queue, value } => {
                            self.check_scoreboard_queue(*sb, queue);
                            self.check_expr(value, false, "ScoreboardOp push value");
                        }
                        crate::ir::ScoreboardOp::QueuePop { queue, dest } => {
                            self.check_scoreboard_queue(*sb, queue);
                            self.check_local(*dest);
                        }
                        crate::ir::ScoreboardOp::ScalarWrite { scalar, value } => {
                            self.check_scoreboard_scalar(*sb, scalar);
                            self.check_expr(value, false, "ScoreboardOp scalar value");
                        }
                    }
                }
                Stmt::ComponentFieldWrite { value, .. } => {
                    // Component host state — the value follows the
                    // no-inline-port rule like any Assign value.
                    self.check_expr(value, false, "ComponentFieldWrite value");
                }
                Stmt::ComponentEmit { args, .. } => {
                    for a in args {
                        self.check_expr(a, false, "ComponentEmit arg");
                    }
                }
                Stmt::ComponentCall { args, dest, .. } => {
                    for a in args {
                        self.check_expr(a, false, "ComponentCall arg");
                    }
                    if let Some(d) = dest {
                        self.check_local(*d);
                    }
                }
                Stmt::SeqPush { seq, value } => {
                    self.check_local(*seq);
                    // The yielded value (a record `Local`) follows the
                    // no-inline-port rule like any host-state assignment.
                    self.check_expr(value, false, "SeqPush value");
                }
                Stmt::ComponentQueuePush { value, .. } => {
                    // Component-queue host state — the pushed value follows
                    // the no-inline-port rule like any Assign value.
                    self.check_expr(value, false, "ComponentQueuePush value");
                }
                Stmt::ComponentQueuePop { dest, .. } => {
                    self.check_local(*dest);
                }
                // Whole sub-component value copy — receiver/source resolved
                // at lowering against the component schema; nothing to
                // verify structurally (no local/port dependency).
                Stmt::ComponentSubAssign { .. } => {}
                Stmt::TlmFork(desc) => {
                    if let Some(d) = desc.dest {
                        self.check_local(d);
                    }
                    // A fork is a bus-bound TLM seam, same resolution rules
                    // as a blocking Assign-RHS edge (Run/Check only, binding
                    // resolves on the owner tb, method exists, arg arity +
                    // purity). The args are no-inline-port.
                    self.check_bus_call_edge(&desc.bus_field, &desc.method, &desc.args);
                }
                Stmt::TlmJoinAll(pending) => {
                    for p in pending {
                        if let Some(d) = p.dest {
                            self.check_local(d);
                        }
                        self.check_bus_call_edge(&p.bus_field, &p.method, &p.args);
                    }
                }
            }
        }
        match &b.terminator {
            Terminator::Branch(c, _, _) => self.check_expr(c, false, "Branch cond"),
            Terminator::WaitCycles(e, _, _) => self.check_expr(e, false, "WaitCycles count"),
            Terminator::WaitCyclesSync(e, _) => self.check_expr(e, false, "WaitCycles count"),
            Terminator::WaitTimePs(..) => {}
            Terminator::WaitUntil { preds, .. } => {
                for p in preds {
                    self.check_expr(&p.expr, true, "WaitUntil pred");
                }
            }
            Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                for p in preds {
                    self.check_expr(&p.expr, true, "WaitUntilTimeout pred");
                }
                self.check_expr(cycles, false, "WaitUntilTimeout cycles");
            }
            Terminator::Randomize {
                target,
                constraints,
                ..
            } => {
                self.check_local(*target);
                // Target must be record-typed: the solver writes the
                // record's fields back into it.
                if let Some(l) = self.func.locals.get(target.index()) {
                    if !matches!(l.ty, IrType::Record(_)) {
                        self.errs.push(VerifyError::DanglingConstraintRef {
                            func: self.fid,
                            block: self.bid,
                            detail: format!("target local `{}` is not record-typed", l.name),
                        });
                    }
                }
                // Invariant 9: the ConstraintRef resolves.
                if constraints.index() >= self.prog.constraint_sites.len() {
                    self.errs.push(VerifyError::DanglingConstraintRef {
                        func: self.fid,
                        block: self.bid,
                        detail: format!("c{} out of range", constraints.0),
                    });
                }
            }
            Terminator::Fatal(args) => self.check_fmt_args(args),
            Terminator::Jump(_) | Terminator::Return => {}
        }
    }

    fn check_fmt_args(&mut self, args: &FmtArgs) {
        for a in &args.args {
            self.check_expr(&a.expr, true, "format arg");
        }
    }

    fn check_local(&mut self, l: LocalId) {
        if l.index() >= self.func.locals.len() {
            self.errs.push(VerifyError::BadLocal {
                func: self.fid,
                block: self.bid,
                local: l,
            });
        }
    }

    /// The owning testbench must declare scalar field `field`.
    fn check_tb_field(&mut self, field: &str) {
        let ok = self
            .func
            .owner
            .and_then(|tb| self.prog.testbenches.get(tb.index()))
            .is_some_and(|tb| tb.scalar_fields.iter().any(|f| f.name == field));
        if !ok {
            self.errs.push(VerifyError::BadTbField {
                func: self.fid,
                block: self.bid,
                field: field.to_string(),
            });
        }
    }

    /// The scoreboard id must resolve and `field` must be a
    /// scoreboard-typed field of the owning testbench bound to it.
    fn check_scoreboard(&mut self, sb: crate::ir::ScoreboardId, field: &str, nested: bool) {
        if sb.index() >= self.prog.scoreboards.len() {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!("scoreboard id sb{} does not resolve", sb.0),
            });
            return;
        }
        // An env-nested data scoreboard (`top.sb`) is a sub-component of
        // the env local, not a testbench field — the binding check below
        // only applies to the `_tb.<field>` form. The sb id already
        // resolved above; that is sufficient for the nested form.
        if nested {
            return;
        }
        let bound = self
            .func
            .owner
            .and_then(|tb| self.prog.testbenches.get(tb.index()))
            .is_some_and(|tb| {
                tb.scoreboard_fields
                    .iter()
                    .any(|(f, id)| f == field && *id == sb)
            });
        if !bound {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!(
                    "field `{field}` is not bound to scoreboard sb{} on the owning testbench",
                    sb.0
                ),
            });
        }
    }

    fn check_scoreboard_scalar(&mut self, sb: crate::ir::ScoreboardId, scalar: &str) {
        let ok = self
            .prog
            .scoreboards
            .get(sb.index())
            .and_then(|s| s.field(scalar))
            .is_some_and(|f| matches!(f.kind, crate::ir::ScoreboardFieldKind::Scalar { .. }));
        if !ok {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!("scoreboard sb{} has no scalar field `{scalar}`", sb.0),
            });
        }
    }

    fn check_scoreboard_queue(&mut self, sb: crate::ir::ScoreboardId, queue: &str) {
        let ok = self
            .prog
            .scoreboards
            .get(sb.index())
            .and_then(|s| s.field(queue))
            .is_some_and(|f| matches!(f.kind, crate::ir::ScoreboardFieldKind::Queue { .. }));
        if !ok {
            self.errs.push(VerifyError::BadScoreboard {
                func: self.fid,
                block: self.bid,
                detail: format!("scoreboard sb{} has no queue field `{queue}`", sb.0),
            });
        }
    }

    /// `local` must be record-typed and its schema must declare `field`.
    /// `mid_positions` lists the segments (positions in `[field] ++ path`)
    /// that carry a `Vec<Record, N>` element selection.
    fn check_record_field(
        &mut self,
        local: LocalId,
        field: &str,
        path: &[String],
        mid_positions: &[usize],
    ) {
        // Resolve `field` then each `path` component against the nested
        // record schemas: a non-leaf component must reach a nested record
        // to descend into — a plain nested-record field (unindexed), or
        // one element of a `Vec<Record, N>` field (indexed); the leaf may
        // be any field but never carries a mid index. Fails on an unknown
        // field, a non-record intermediate, or an index/`Vec` mismatch.
        let ok = (|| -> Option<()> {
            let tl = self.func.locals.get(local.index())?;
            let mut rid = match tl.ty {
                IrType::Record(r) => r,
                _ => return None,
            };
            let segs: Vec<&str> = std::iter::once(field)
                .chain(path.iter().map(String::as_str))
                .collect();
            let last = segs.len() - 1;
            for (i, seg) in segs.iter().enumerate() {
                let fld = self.prog.records.get(rid.index())?.field(seg)?;
                let indexed = mid_positions.contains(&i);
                if i == last {
                    return (!indexed).then_some(());
                }
                match fld.ty {
                    IrType::Record(r) if fld.vec_len.is_none() == !indexed => rid = r,
                    _ => return None,
                }
            }
            Some(())
        })();
        if ok.is_none() {
            let mut dotted = field.to_string();
            for p in path {
                dotted.push('.');
                dotted.push_str(p);
            }
            self.errs.push(VerifyError::BadRecordField {
                func: self.fid,
                block: self.bid,
                local,
                field: dotted,
            });
        }
    }

    /// The `Stmt::TransactorCall` payload: must be a `TransactorMethod`
    /// call edge whose `bus_field`/`method` resolve through the owner
    /// testbench's transactor fields. Args follow the no-inline-ports
    /// rule (they are hoisted at lowering, like `Assign` values).
    fn check_transactor_call(&mut self, call: &Expr) {
        let (fid, bid) = (self.fid, self.bid);
        let bad = move |detail: String| VerifyError::BadTransactorCall {
            func: fid,
            block: bid,
            detail,
        };
        let Expr::Call(CallTarget::TransactorMethod { bus_field, method }, args) = call else {
            self.errs.push(bad(
                "payload is not a TransactorMethod call edge".to_string()
            ));
            return;
        };
        for a in args {
            self.check_expr(a, false, "TransactorCall arg");
        }
        let Some(owner) = self.func.owner else {
            self.errs.push(bad(format!(
                "`{bus_field}.{method}` called from a function with no owner testbench"
            )));
            return;
        };
        let Some(tb) = self.prog.testbenches.get(owner.index()) else {
            self.errs
                .push(bad(format!("owner tb{} does not resolve", owner.0)));
            return;
        };
        let Some((_, xid)) = tb.transactor_fields.iter().find(|(f, _)| f == bus_field) else {
            if tb.bus_bindings.iter().any(|b| &b.field == bus_field) {
                self.errs.push(bad(format!(
                    "`{bus_field}.{method}` names a bus binding but rides a \
                     Stmt::TransactorCall — bus-bound edges must be the entire \
                     RHS of an Assign"
                )));
            } else {
                self.errs.push(bad(format!(
                    "testbench `{}` has no transactor field `{bus_field}`",
                    tb.name
                )));
            }
            return;
        };
        let Some(schema) = self.prog.transactors.get(xid.index()) else {
            self.errs
                .push(bad(format!("transactor x{} does not resolve", xid.0)));
            return;
        };
        if schema.method(method).is_none() {
            self.errs.push(bad(format!(
                "transactor `{}` has no method `{method}`",
                schema.name
            )));
        }
    }

    fn check_covgroup(&mut self, c: CovgroupId) {
        if c.index() >= self.prog.covgroups.len() {
            self.errs.push(VerifyError::BadCovgroup {
                func: self.fid,
                block: self.bid,
                covgroup: c,
            });
        }
    }

    fn check_expr(&mut self, e: &Expr, ports_ok: bool, context: &'static str) {
        match e {
            Expr::Literal { .. } | Expr::WideLiteral(_) => {}
            // The global cycle counter — a framework value, no
            // local/port dependency to verify.
            Expr::CycleCount | Expr::ErrorCount => {}
            Expr::Local(l) => self.check_local(*l),
            Expr::TbField(field) => self.check_tb_field(field),
            // Transactor-instance state — host state, resolved at
            // lowering against the bound instance; nothing to verify
            // structurally here (no local/port dependency).
            Expr::TransactorState { .. } => {}
            Expr::TransactorStateRecordField { .. } => {}
            Expr::TransactorStateQueueQuery { .. } => {}
            Expr::Port(_) => {
                if !ports_ok {
                    self.errs.push(VerifyError::PortInDisallowedPosition {
                        func: self.fid,
                        block: self.bid,
                        context,
                    });
                }
            }
            Expr::Binary(_, a, b) => {
                self.check_expr(a, ports_ok, context);
                self.check_expr(b, ports_ok, context);
            }
            Expr::Unary(_, a) => self.check_expr(a, ports_ok, context),
            Expr::BitSlice { target, .. } => self.check_expr(target, ports_ok, context),
            Expr::Ternary(c, t, e2) => {
                self.check_expr(c, ports_ok, context);
                self.check_expr(t, ports_ok, context);
                self.check_expr(e2, ports_ok, context);
            }
            Expr::WidthCast {
                width,
                src_width,
                inner,
                ..
            } => {
                if *width == 0
                    || *width > crate::MAX_WIDTH_METHOD_BITS
                    || src_width.is_some_and(|w| w == 0 || w > crate::MAX_WIDTH_METHOD_BITS)
                {
                    self.errs.push(VerifyError::BadWidthCast {
                        func: self.fid,
                        block: self.bid,
                        width: *width,
                        src_width: *src_width,
                    });
                }
                self.check_expr(inner, ports_ok, context);
            }
            Expr::RecordField {
                local,
                field,
                path,
                mid_indices,
                index,
            } => {
                self.check_local(*local);
                let mid_positions: Vec<usize> = mid_indices.iter().map(|(p, _)| *p).collect();
                self.check_record_field(*local, field, path, &mid_positions);
                for (_, idx) in mid_indices {
                    self.check_expr(idx, ports_ok, context);
                }
                if let Some(idx) = index {
                    self.check_expr(idx, ports_ok, context);
                }
            }
            // Register-level frontdoor read in expression position. The
            // mirror is a record local; the register name must be one of
            // its fields. The helper read is a plain lambda call (not the
            // TLM seam), so it is a legitimate sub-expression value —
            // nothing in the seam rule forbids it here.
            Expr::RegRead { mirror, field, .. } => {
                self.check_local(*mirror);
                self.check_record_field(*mirror, field, &[], &[]);
            }
            Expr::CovBin { inst, .. } => self.check_covgroup(inst.covgroup),
            // A hook-param cover target carries the parameter NAME (no
            // resolvable local before the transactor pass); only its
            // optional index sub-expression needs checking.
            Expr::CovHookParam { index, .. } => {
                if let Some(i) = index {
                    self.check_expr(i, ports_ok, context);
                }
            }
            Expr::CovHookArg { .. } => {}
            // Component host state — resolved at lowering against the
            // component schema; no local/port dependency to verify here.
            Expr::ComponentField { .. } => {}
            // A by-value component passed as a method arg. A `Local` base
            // is a method-param local (verify it is defined); a
            // `SelfField`/`Path` base is resolved at lowering.
            Expr::ComponentValue { base } => {
                if let crate::ir::ComponentBase::Local(l) = base {
                    self.check_local(*l);
                }
            }
            // Component-queue size/empty read — host state resolved at
            // lowering against the component schema; nothing to verify.
            Expr::ComponentQueueQuery { .. } => {}
            // Idle predicate: the base/kind are resolved at lowering; only
            // the threshold sub-expression carries verifiable structure.
            Expr::ComponentIdle { n, .. } => self.check_expr(n, ports_ok, context),
            Expr::ScoreboardQuery {
                sb,
                field,
                query,
                nested_path,
            } => {
                self.check_scoreboard(*sb, field, nested_path.is_some());
                match query {
                    crate::ir::ScoreboardQuery::Scalar { scalar } => {
                        self.check_scoreboard_scalar(*sb, scalar)
                    }
                    crate::ir::ScoreboardQuery::QueueSize { queue }
                    | crate::ir::ScoreboardQuery::QueueEmpty { queue } => {
                        self.check_scoreboard_queue(*sb, queue)
                    }
                }
            }
            // Sequence length: the seq local must resolve. Host state —
            // no port/value dependency beyond the local.
            Expr::SeqLen(seq) => self.check_local(*seq),
            // Sequence element read (`seq[i]`): the seq local must resolve;
            // the index follows the same port rules as the surrounding
            // context.
            Expr::SeqIndex { seq, index } => {
                self.check_local(*seq);
                self.check_expr(index, ports_ok, context);
            }
            Expr::Call(target, args) => {
                // Seam rule: a call edge is never an expression VALUE.
                // It reaches the verifier only as the top-level Assign
                // RHS (bus) or the root payload of `Stmt::TransactorCall`
                // (transactor) — both consumed by `check_block` before
                // recursing. Reaching one here means it is nested or in
                // a disallowed statement position.
                if let CallTarget::TransactorMethod { bus_field, method } = target {
                    self.errs.push(VerifyError::BadTransactorCall {
                        func: self.fid,
                        block: self.bid,
                        detail: format!(
                            "`{bus_field}.{method}` call edge in a disallowed position \
                             ({context}) — must be the entire RHS of an Assign (bus) \
                             or the payload of a Stmt::TransactorCall (transactor)"
                        ),
                    });
                }
                if let CallTarget::TransactorSelfMethod { transactor, method } = target {
                    self.errs.push(VerifyError::BadTransactorCall {
                        func: self.fid,
                        block: self.bid,
                        detail: format!(
                            "`{transactor}.{method}` sibling call in a disallowed position \
                             ({context}) — lowering must hoist it into a \
                             Stmt::TransactorSelfCall"
                        ),
                    });
                }
                for a in args {
                    self.check_expr(a, ports_ok, context);
                }
            }
        }
    }

    /// Validate one sibling method call inside a DUT-poking transactor
    /// method body. These calls are synchronous lambda calls, not
    /// testbench-field call edges, so they are only legal in a
    /// `TransactorBody` and resolve against that body's transactor
    /// schema.
    fn check_transactor_self_call(&mut self, dest: Option<LocalId>, call: &Expr) {
        let (fid, bid) = (self.fid, self.bid);
        let bad = move |detail: String| VerifyError::BadTransactorCall {
            func: fid,
            block: bid,
            detail,
        };
        let Expr::Call(CallTarget::TransactorSelfMethod { transactor, method }, args) = call else {
            self.errs
                .push(bad("payload is not a TransactorSelfMethod call".to_string()));
            return;
        };
        for a in args {
            self.check_expr(a, false, "TransactorSelfCall arg");
        }
        let FunctionKind::TransactorBody { transactor: xid } = self.func.kind else {
            self.errs.push(bad(format!(
                "`{transactor}.{method}` sibling call outside a transactor method body"
            )));
            return;
        };
        let Some(schema) = self.prog.transactors.get(xid.index()) else {
            self.errs
                .push(bad(format!("transactor t{} does not resolve", xid.0)));
            return;
        };
        if schema.name != *transactor {
            self.errs.push(bad(format!(
                "sibling call names transactor `{transactor}` from `{}` body",
                schema.name
            )));
            return;
        }
        let Some(m) = schema.method(method) else {
            self.errs.push(bad(format!(
                "transactor `{}` has no sibling method `{method}`",
                schema.name
            )));
            return;
        };
        if args.len() != m.n_params {
            self.errs.push(bad(format!(
                "transactor method `{}.{method}` takes {} argument(s), call passes {}",
                schema.name,
                m.n_params,
                args.len()
            )));
        }
        if dest.is_some() && !m.has_ret {
            self.errs.push(bad(format!(
                "void transactor method `{}.{method}` captured into a destination",
                schema.name
            )));
        }
    }

    fn bad_transactor(&mut self, detail: String) {
        self.errs.push(VerifyError::BadTransactorCall {
            func: self.fid,
            block: self.bid,
            detail,
        });
    }

    /// Validate one sanctioned bus-bound `TransactorMethod` call edge
    /// (Assign-RHS position): function kind, bus-binding resolution on
    /// the owning testbench, method existence, arity, and argument
    /// purity (no ports, no nesting). Transactor-field edges never take
    /// this position — they ride `Stmt::TransactorCall` and are checked
    /// by `check_transactor_call`.
    fn check_bus_call_edge(&mut self, bus_field: &str, method: &str, args: &[Expr]) {
        // A `TransactorBody` function may carry a downstream blocking
        // bus-call edge when it is a bound-to target responder
        // re-issuing a TLM call (nested forwarding). The responder body
        // is lowered standalone (no owner testbench), so the binding's
        // wire names cannot be resolved here — emission resolves the edge
        // against the binding testbench's `bus_bindings` (raising an
        // EmitError if the downstream binding is absent). Only argument
        // purity is checked here (below); the Run/Check resolution arm is
        // skipped for the owner-less responder case.
        if matches!(self.func.kind, FunctionKind::TransactorBody { .. })
            && self.func.owner.is_none()
        {
            // Downstream forwarding edge — defer wire resolution to emit.
        } else if !matches!(self.func.kind, FunctionKind::Run | FunctionKind::Check) {
            self.bad_transactor(format!(
                "`{bus_field}.{method}` call edge in a {:?}-kind function \
                 (allowed only in Run/Check bodies or a bound-to responder \
                 forwarding a downstream call)",
                self.func.kind
            ));
        } else {
            let owner_tb = self
                .func
                .owner
                .and_then(|tb| self.prog.testbenches.get(tb.index()));
            let binding =
                owner_tb.and_then(|tb| tb.bus_bindings.iter().find(|b| b.field == bus_field));
            let diag = match binding {
                None if owner_tb
                    .is_some_and(|tb| tb.transactor_fields.iter().any(|(f, _)| f == bus_field)) =>
                {
                    Some(format!(
                        "`{bus_field}.{method}` names a transactor field but rides an \
                         Assign RHS — transactor-bound edges must be a \
                         Stmt::TransactorCall payload"
                    ))
                }
                None => Some(format!(
                    "`{bus_field}.{method}` does not resolve: owning testbench has no \
                     bus binding `{bus_field}`"
                )),
                Some(b) => match b.methods.iter().find(|m| m.name == method) {
                    None => Some(format!(
                        "bus `{}` (binding `{bus_field}`) has no tlm_method `{method}`",
                        b.bus
                    )),
                    Some(m) if m.args.len() != args.len() => Some(format!(
                        "`{bus_field}.{method}` arity mismatch: schema declares {} \
                         arg(s), call carries {}",
                        m.args.len(),
                        args.len()
                    )),
                    Some(_) => None,
                },
            };
            if let Some(what) = diag {
                self.bad_transactor(what);
            }
        }
        for a in args {
            self.check_expr(a, false, "TransactorMethod arg");
        }
    }
}

/// Best-effort expression typing for invariant 15. Returns `None` when
/// the expression's type cannot be locally determined.
fn expr_type(func: &TbFunction, e: &Expr) -> Option<IrType> {
    match e {
        Expr::Literal { ty, .. } => Some(ty.clone()),
        Expr::WideLiteral(words) => Some(IrType::UInt(Some(wide_literal_bits(words)))),
        Expr::Local(l) => func.locals.get(l.index()).map(|t| t.ty.clone()),
        Expr::BitSlice { hi, lo, .. } => Some(IrType::UInt(Some(hi - lo + 1))),
        Expr::WidthCast { kind, width, .. } => Some(match kind {
            crate::ir::WidthCastKind::Sext => IrType::SInt(Some(*width)),
            _ => IrType::UInt(Some(*width)),
        }),
        _ => None,
    }
}

fn assign_compatible(expected: &IrType, actual: &IrType) -> bool {
    if expected == actual {
        return true;
    }
    // A widthless scalar (`UInt(None)` / `SInt(None)`) is signedness
    // metadata on a 64-bit value with no declared width — file-scope
    // const / enum-variant substitution emits these (#525). For width
    // compatibility it is the same wildcard `Unknown` was before the
    // substitution carried signedness: assignable into (and from) any
    // scalar local, exactly the pre-#525 accepted set.
    let widthless =
        |t: &IrType| matches!(t, IrType::UInt(None) | IrType::SInt(None));
    if widthless(expected) || widthless(actual) {
        return true;
    }
    match (expected, actual) {
        (IrType::UInt(Some(ew)), IrType::UInt(Some(aw)))
        | (IrType::SInt(Some(ew)), IrType::SInt(Some(aw))) => aw <= ew,
        (IrType::UInt(Some(ew)), IrType::Bool) | (IrType::SInt(Some(ew)), IrType::Bool) => *ew >= 1,
        _ => false,
    }
}

fn wide_literal_bits(words: &[u32]) -> u32 {
    let Some((idx, word)) = words.iter().enumerate().rev().find(|(_, w)| **w != 0) else {
        return 1;
    };
    (idx as u32) * 32 + (32 - word.leading_zeros())
}

fn bit_words(nbits: usize) -> usize {
    nbits.div_ceil(64)
}

fn full_bits(nbits: usize) -> Vec<u64> {
    let words = bit_words(nbits);
    let mut bits = vec![!0u64; words];
    let rem = nbits % 64;
    if rem != 0 {
        if let Some(last) = bits.last_mut() {
            *last = (1u64 << rem) - 1;
        }
    }
    bits
}

fn zero_bits(nbits: usize) -> Vec<u64> {
    vec![0u64; bit_words(nbits)]
}

fn bit_get(bits: &[u64], idx: usize) -> bool {
    bits.get(idx / 64)
        .is_some_and(|word| (word & (1u64 << (idx % 64))) != 0)
}

fn bit_set(bits: &mut [u64], idx: usize) {
    if let Some(word) = bits.get_mut(idx / 64) {
        *word |= 1u64 << (idx % 64);
    }
}

fn bit_or_assign(dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d |= *s;
    }
}

fn bit_and_assign(dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        *d &= *s;
    }
}

/// Invariant 4 — iterative forward dataflow over "definitely defined"
/// local sets. `defined_in[b]` = intersection of predecessors' outs;
/// a read inside the block must be covered by the running defined set.
fn check_def_before_use(
    func: &TbFunction,
    fid: FunctionId,
    reachable: &[bool],
    errs: &mut Vec<VerifyError>,
) {
    let nlocals = func.locals.len();
    let nblocks = func.blocks.len();
    if nblocks == 0 {
        return;
    }
    let full = full_bits(nlocals);
    // Params count as defined at entry: by convention the first
    // `params.len()` locals mirror the function's parameters.
    let mut entry_in = zero_bits(nlocals);
    for i in 0..func.params.len().min(nlocals) {
        bit_set(&mut entry_in, i);
    }
    // A `RecordSeq` accumulator (the tseq `ret` slot) is always
    // default-constructed by the backend at function top — `declare_locals`
    // emits `std::vector<Record> r{};` — so it is live from entry. Mark it
    // defined so the `yield`/`SeqPush` accumulator read never trips
    // use-before-def.
    for (i, l) in func.locals.iter().enumerate() {
        if matches!(l.ty, IrType::RecordSeq(_) | IrType::Seq(_)) {
            bit_set(&mut entry_in, i);
        }
    }
    let mut ins: Vec<Vec<u64>> = vec![full.clone(); nblocks];
    ins[func.entry.index()] = entry_in.clone();

    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nblocks];
    for (bi, b) in func.blocks.iter().enumerate() {
        for s in b.terminator.successors() {
            preds[s.index()].push(bi);
        }
    }

    let mut gens = vec![zero_bits(nlocals); nblocks];
    for (bi, b) in func.blocks.iter().enumerate() {
        for s in &b.stmts {
            match s {
                Stmt::Assign(l, _) | Stmt::DutRead(l, _) | Stmt::RecordInit(l, _) => {
                    bit_set(&mut gens[bi], l.index());
                }
                Stmt::TransactorCall { dest: Some(l), .. } => {
                    bit_set(&mut gens[bi], l.index());
                }
                Stmt::TransactorSelfCall { dest: Some(l), .. } => {
                    bit_set(&mut gens[bi], l.index());
                }
                Stmt::ScoreboardOp {
                    op: crate::ir::ScoreboardOp::QueuePop { dest: l, .. },
                    ..
                }
                | Stmt::ComponentCall { dest: Some(l), .. }
                | Stmt::ComponentQueuePop { dest: l, .. }
                | Stmt::TransactorStateQueuePop { dest: l, .. } => {
                    bit_set(&mut gens[bi], l.index());
                }
                _ => {}
            }
        }
    }

    // Fixpoint.
    loop {
        let mut changed = false;
        for bi in 0..nblocks {
            if !reachable[bi] {
                continue;
            }
            let new_in = if bi == func.entry.index() {
                entry_in.clone()
            } else {
                let mut acc = full.clone();
                let mut out = zero_bits(nlocals);
                let mut any = false;
                for &p in &preds[bi] {
                    if !reachable[p] {
                        continue;
                    }
                    any = true;
                    out.clone_from(&ins[p]);
                    bit_or_assign(&mut out, &gens[p]);
                    bit_and_assign(&mut acc, &out);
                }
                if !any {
                    zero_bits(nlocals)
                } else {
                    acc
                }
            };
            if new_in != ins[bi] {
                ins[bi] = new_in;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Walk each reachable block statement-by-statement, reporting reads
    // not covered by the running defined set.
    for (bi, b) in func.blocks.iter().enumerate() {
        if !reachable[bi] {
            continue;
        }
        let bid = BlockId(bi as u32);
        let mut defined = ins[bi].clone();
        let check_e = |e: &Expr, defined: &[u64], errs: &mut Vec<VerifyError>| {
            for_each_local(e, &mut |l| {
                if l.index() < nlocals && !bit_get(defined, l.index()) {
                    errs.push(VerifyError::LocalUseBeforeDef {
                        func: fid,
                        block: bid,
                        local: l,
                    });
                }
            });
        };
        for s in &b.stmts {
            match s {
                Stmt::Assign(l, e) => {
                    check_e(e, &defined, errs);
                    bit_set(&mut defined, l.index());
                }
                Stmt::DutRead(l, _) | Stmt::RecordInit(l, _) => {
                    bit_set(&mut defined, l.index());
                }
                Stmt::RecordFieldWrite { local, value, .. }
                | Stmt::RecordWriteCb { local, value, .. } => {
                    // Writing a field READS the record local (it must
                    // be initialized first — RecordInit defines it).
                    if local.index() < nlocals && !bit_get(&defined, local.index()) {
                        errs.push(VerifyError::LocalUseBeforeDef {
                            func: fid,
                            block: bid,
                            local: *local,
                        });
                    }
                    check_e(value, &defined, errs);
                }
                Stmt::TbFieldWrite { value, .. } => check_e(value, &defined, errs),
                Stmt::TransactorStateWrite { value, .. } => check_e(value, &defined, errs),
                Stmt::TransactorStateRecordFieldWrite { value, .. } => {
                    check_e(value, &defined, errs)
                }
                Stmt::TransactorStateQueuePush { value, .. } => check_e(value, &defined, errs),
                Stmt::TransactorStateQueuePop { dest, .. } => {
                    // Pop defines the destination local.
                    bit_set(&mut defined, dest.index());
                }
                Stmt::DutWrite(_, e) => check_e(e, &defined, errs),
                Stmt::TransactorCall { dest, call } => {
                    check_e(call, &defined, errs);
                    if let Some(l) = dest {
                        bit_set(&mut defined, l.index());
                    }
                }
                Stmt::TransactorSelfCall { dest, call } => {
                    check_e(call, &defined, errs);
                    if let Some(l) = dest {
                        bit_set(&mut defined, l.index());
                    }
                }
                Stmt::Log { args, .. } => {
                    for a in &args.args {
                        check_e(&a.expr, &defined, errs);
                    }
                }
                Stmt::AssertCheck { cond, on_fail } => {
                    check_e(cond, &defined, errs);
                    for a in &on_fail.args {
                        check_e(&a.expr, &defined, errs);
                    }
                }
                Stmt::CovReport(_) => {}
                Stmt::ProbeRelease(_) => {}
                Stmt::FailDiag { guard, args } => {
                    if let Some(g) = guard {
                        check_e(g, &defined, errs);
                    }
                    for a in &args.args {
                        check_e(&a.expr, &defined, errs);
                    }
                }
                Stmt::ScoreboardOp { op, .. } => match op {
                    crate::ir::ScoreboardOp::QueuePush { value, .. } => {
                        check_e(value, &defined, errs)
                    }
                    crate::ir::ScoreboardOp::ScalarWrite { value, .. } => {
                        check_e(value, &defined, errs)
                    }
                    crate::ir::ScoreboardOp::QueuePop { dest, .. } => {
                        bit_set(&mut defined, dest.index());
                    }
                },
                Stmt::ComponentFieldWrite { value, .. } => check_e(value, &defined, errs),
                Stmt::ComponentEmit { args, .. } => {
                    for a in args {
                        check_e(a, &defined, errs);
                    }
                }
                Stmt::ComponentCall { args, dest, .. } => {
                    for a in args {
                        check_e(a, &defined, errs);
                    }
                    if let Some(l) = dest {
                        bit_set(&mut defined, l.index());
                    }
                }
                Stmt::SeqPush { seq, value } => {
                    // `yield t` reads both the accumulator (defined at the
                    // tseq function entry) and the yielded value.
                    if seq.index() < nlocals && !bit_get(&defined, seq.index()) {
                        errs.push(VerifyError::LocalUseBeforeDef {
                            func: fid,
                            block: bid,
                            local: *seq,
                        });
                    }
                    check_e(value, &defined, errs);
                }
                Stmt::ComponentQueuePush { value, .. } => check_e(value, &defined, errs),
                Stmt::ComponentQueuePop { dest, .. } => {
                    // Pop defines the destination local.
                    bit_set(&mut defined, dest.index());
                }
                // Whole sub-component copy — no local def/use (both ends
                // are component values, not test locals).
                Stmt::ComponentSubAssign { .. } => {}
                Stmt::TlmFork(desc) => {
                    // Args read at the fork site; the dest is defined here
                    // (v1 declares + zero-inits `T x = {};` at the fork,
                    // so reads between fork and join_all see a defined
                    // local), and re-assigned at the matching join_all.
                    for a in &desc.args {
                        check_e(a, &defined, errs);
                    }
                    if let Some(l) = desc.dest {
                        bit_set(&mut defined, l.index());
                    }
                }
                Stmt::TlmJoinAll(pending) => {
                    for p in pending {
                        if let Some(l) = p.dest {
                            bit_set(&mut defined, l.index());
                        }
                    }
                }
            }
        }
        match &b.terminator {
            Terminator::Branch(c, _, _) => check_e(c, &defined, errs),
            Terminator::WaitCycles(e, _, _) => check_e(e, &defined, errs),
            Terminator::WaitCyclesSync(e, _) => check_e(e, &defined, errs),
            Terminator::WaitTimePs(..) => {}
            Terminator::WaitUntil { preds, .. } => {
                for p in preds {
                    check_e(&p.expr, &defined, errs);
                }
            }
            Terminator::WaitUntilTimeout { preds, cycles, .. } => {
                for p in preds {
                    check_e(&p.expr, &defined, errs);
                }
                check_e(cycles, &defined, errs);
            }
            Terminator::Fatal(args) => {
                for a in &args.args {
                    check_e(&a.expr, &defined, errs);
                }
            }
            Terminator::Randomize { target, .. } => {
                // The solver writes the record fields back into `target`;
                // it is a def, not a use (the record local was already
                // defined at its `let` RecordInit site).
                bit_set(&mut defined, target.index());
            }
            Terminator::Jump(_) | Terminator::Return => {}
        }
    }
}

fn for_each_local(e: &Expr, f: &mut impl FnMut(LocalId)) {
    match e {
        Expr::Literal { .. }
        | Expr::WideLiteral(_)
        | Expr::CycleCount
        | Expr::ErrorCount
        | Expr::Port(_)
        | Expr::TbField(_)
        | Expr::TransactorState { .. }
        | Expr::TransactorStateRecordField { .. }
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::ComponentField { .. }
        | Expr::ScoreboardQuery { .. }
        | Expr::ComponentQueueQuery { .. }
        | Expr::CovHookArg { .. } => {}
        Expr::ComponentValue { base } => {
            if let crate::ir::ComponentBase::Local(l) = base {
                f(*l);
            }
        }
        Expr::Local(l) => f(*l),
        Expr::RecordField {
            local,
            mid_indices,
            index,
            ..
        } => {
            f(*local);
            for (_, idx) in mid_indices {
                for_each_local(idx, f);
            }
            if let Some(idx) = index {
                for_each_local(idx, f);
            }
        }
        // The mirror record local is both used (read) and written (the
        // inline assignment-expression predict), but it was defined at
        // its `let` RecordInit site upstream — record it as a use.
        Expr::RegRead { mirror, .. } => f(*mirror),
        Expr::Binary(_, a, b) => {
            for_each_local(a, f);
            for_each_local(b, f);
        }
        Expr::Unary(_, a) => for_each_local(a, f),
        Expr::BitSlice { target, .. } => for_each_local(target, f),
        Expr::Ternary(c, t, e) => {
            for_each_local(c, f);
            for_each_local(t, f);
            for_each_local(e, f);
        }
        Expr::WidthCast { inner, .. } => for_each_local(inner, f),
        Expr::ComponentIdle { n, .. } => for_each_local(n, f),
        Expr::CovBin { .. } => {}
        Expr::CovHookParam { index, .. } => {
            if let Some(i) = index {
                for_each_local(i, f);
            }
        }
        Expr::SeqLen(l) => f(*l),
        Expr::SeqIndex { seq, index } => {
            f(*seq);
            for_each_local(index, f);
        }
        Expr::Call(_, args) => {
            for a in args {
                for_each_local(a, f);
            }
        }
    }
}
