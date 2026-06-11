//! TB-IR structural verifier — design-doc invariants 1-8, 10, 15 plus
//! the port-position rule (an `Expr::Port` may appear only in wait
//! predicates, format-arg expressions, `DutRead`/`DutWrite` operands,
//! and `AssertCheck` condition subtrees).
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
    /// Port-position rule: `Expr::Port` outside an allowed position.
    PortInDisallowedPosition {
        func: FunctionId,
        block: BlockId,
        context: &'static str,
    },
    /// Cross-IR: a test's run/check FunctionId or TestbenchId resolves.
    BadProgramRef { what: String },
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
            VerifyError::PortInDisallowedPosition {
                func,
                block,
                context,
            } => write!(
                f,
                "fn{}: b{} contains a DUT port read in a disallowed position ({context})",
                func.0, block.0
            ),
            VerifyError::BadProgramRef { what } => write!(f, "program: {what}"),
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
    if errs.is_empty() { Ok(()) } else { Err(errs) }
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

    if errs.is_empty() { Ok(()) } else { Err(errs) }
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
                    self.check_expr(e, false, "Assign value");
                    // Invariant 15.
                    if self.func.locals.get(l.index()).is_some() {
                        let expected = &self.func.local(*l).ty;
                        if let Some(actual) = expr_type(self.func, e) {
                            if *expected != IrType::Unknown
                                && actual != IrType::Unknown
                                && *expected != actual
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
                Stmt::Log { args, .. } => self.check_fmt_args(args),
                Stmt::AssertCheck { cond, on_fail } => {
                    self.check_expr(cond, true, "AssertCheck cond");
                    self.check_fmt_args(on_fail);
                }
                Stmt::CovReport(inst) => self.check_covgroup(inst.covgroup),
            }
        }
        match &b.terminator {
            Terminator::Branch(c, _, _) => self.check_expr(c, false, "Branch cond"),
            Terminator::WaitCycles(e, _) => self.check_expr(e, false, "WaitCycles count"),
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
            Expr::Literal { .. } => {}
            Expr::Local(l) => self.check_local(*l),
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
            Expr::CovBin { inst, .. } => self.check_covgroup(inst.covgroup),
            Expr::Call(_, args) => {
                for a in args {
                    self.check_expr(a, ports_ok, context);
                }
            }
        }
    }
}

/// Best-effort expression typing for invariant 15. Returns `None` when
/// the expression's type cannot be locally determined.
fn expr_type(func: &TbFunction, e: &Expr) -> Option<IrType> {
    match e {
        Expr::Literal { ty, .. } => Some(ty.clone()),
        Expr::Local(l) => func.locals.get(l.index()).map(|t| t.ty.clone()),
        _ => None,
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
    // Bitsets as Vec<bool> — function sizes are tiny.
    let full = vec![true; nlocals];
    let mut ins: Vec<Vec<bool>> = vec![full.clone(); nblocks];
    ins[func.entry.index()] = vec![false; nlocals];

    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); nblocks];
    for (bi, b) in func.blocks.iter().enumerate() {
        for s in b.terminator.successors() {
            preds[s.index()].push(bi);
        }
    }

    let gen_kill = |b: &BasicBlock, start: &[bool]| -> Vec<bool> {
        let mut d = start.to_vec();
        for s in &b.stmts {
            match s {
                Stmt::Assign(l, _) | Stmt::DutRead(l, _) => {
                    if l.index() < d.len() {
                        d[l.index()] = true;
                    }
                }
                _ => {}
            }
        }
        d
    };

    // Fixpoint.
    loop {
        let mut changed = false;
        for bi in 0..nblocks {
            if !reachable[bi] {
                continue;
            }
            let new_in = if bi == func.entry.index() {
                vec![false; nlocals]
            } else {
                let mut acc = full.clone();
                let mut any = false;
                for &p in &preds[bi] {
                    if !reachable[p] {
                        continue;
                    }
                    any = true;
                    let out = gen_kill(&func.blocks[p], &ins[p]);
                    for i in 0..nlocals {
                        acc[i] = acc[i] && out[i];
                    }
                }
                if !any { vec![false; nlocals] } else { acc }
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
        let check_e = |e: &Expr, defined: &[bool], errs: &mut Vec<VerifyError>| {
            for_each_local(e, &mut |l| {
                if l.index() < defined.len() && !defined[l.index()] {
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
                    if l.index() < defined.len() {
                        defined[l.index()] = true;
                    }
                }
                Stmt::DutRead(l, _) => {
                    if l.index() < defined.len() {
                        defined[l.index()] = true;
                    }
                }
                Stmt::DutWrite(_, e) => check_e(e, &defined, errs),
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
            }
        }
        match &b.terminator {
            Terminator::Branch(c, _, _) => check_e(c, &defined, errs),
            Terminator::WaitCycles(e, _) => check_e(e, &defined, errs),
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
            Terminator::Jump(_) | Terminator::Return => {}
        }
    }
}

fn for_each_local(e: &Expr, f: &mut impl FnMut(LocalId)) {
    match e {
        Expr::Literal { .. } | Expr::Port(_) => {}
        Expr::Local(l) => f(*l),
        Expr::Binary(_, a, b) => {
            for_each_local(a, f);
            for_each_local(b, f);
        }
        Expr::Unary(_, a) => for_each_local(a, f),
        Expr::CovBin { .. } => {}
        Expr::Call(_, args) => {
            for a in args {
                for_each_local(a, f);
            }
        }
    }
}
