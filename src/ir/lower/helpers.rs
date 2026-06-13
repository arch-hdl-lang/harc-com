//! Helper-function support: categorization + CFG inlining.
//!
//! File-level `function f(...)` declarations come in two flavors:
//!
//! - **Pure** helpers — scalar computation only (literals, params,
//!   locals, arithmetic, `if`/`for`/`while`/`repeat`/`loop`, `return`).
//!   These lower once into a standalone `TbFunction` (kind `Helper`)
//!   and call sites stay `Expr::Call(CallTarget::Helper, ...)`, emitted
//!   as a plain C++ function call.
//! - **Impure** helpers — anything that touches the DUT, suspends
//!   (`wait`), or needs the runtime log context (`log`/`assert`/`fail`),
//!   plus anything outside the pure scalar subset. These are
//!   **CFG-inlined at each call site**: params become fresh caller
//!   locals assigned from the lowered arguments (DUT-typed params
//!   become aliases of the caller's DUT field), the body lowers into
//!   the caller's blocks, and `return e` becomes
//!   `Assign(dest, e); Jump(continuation)`.
//!
//! The categorization is deliberately conservative: an expression or
//! statement form the scanner does not recognize marks the helper
//! impure, deferring the precise `Unsupported` error to call-site
//! lowering — so an *uncalled* impure helper with exotic contents
//! never errors (v1 likewise emits every helper lambda and lets the
//! C++ compiler ignore the uncalled ones). Helpers classified pure
//! lower eagerly, so the rare scan-recognized-but-unlowerable form
//! (e.g. a Verilog-sized literal) errors even when uncalled — still an
//! explicit `Unsupported`, never a mis-lower.
//!
//! Recursion (direct or mutual) is rejected up front via a DFS over
//! the helper-to-helper call graph, and again at inline time via the
//! frame stack (belt and suspenders for call edges the conservative
//! scanner might miss inside unrecognized constructs).

use super::{FuncBuilder, InlineFrame, LowerCtx, LowerError, unsupported};
use crate::ast::{
    Block, BuiltinTy, CallArg, Expr as AstExpr, ExprKind, FunctionDecl, Item, SourceFile,
    Stmt as AstStmt, StmtKind, TypeArg, TypeExpr,
};
use crate::ir::{
    CallTarget, Expr, FunctionId, FunctionKind, IrType, Stmt, TbFunction, Terminator, TypedParam,
};
use std::collections::{HashMap, HashSet};

pub(crate) struct HelperEntry<'a> {
    pub decl: &'a FunctionDecl,
    pub pure: bool,
}

#[derive(Default)]
pub(crate) struct HelperRegistry<'a> {
    by_name: HashMap<String, HelperEntry<'a>>,
}

impl<'a> HelperRegistry<'a> {
    /// Collect every file-level `function`, categorize pure vs impure,
    /// and reject recursion (direct or mutual).
    pub(crate) fn build(file: &'a SourceFile) -> Result<Self, LowerError> {
        let mut decls: Vec<&'a FunctionDecl> = Vec::new();
        for it in &file.items {
            if let Item::Function(f) = it {
                decls.push(f);
            }
        }
        let names: HashSet<&str> = decls.iter().map(|d| d.name.name.as_str()).collect();

        // Per-helper conservative scan.
        let mut scans: HashMap<&str, Scan> = HashMap::new();
        for d in &decls {
            scans.insert(d.name.name.as_str(), scan_decl(d));
        }

        // Recursion check: DFS over helper-only call edges.
        check_acyclic(&decls, &scans)?;

        // Impurity fixpoint: a helper is impure if its own body is, or
        // if it calls an impure helper (an inlined body cannot live
        // inside a file-scope C++ function), or if it calls a name
        // that is not a declared helper (defer the error to use site).
        let mut impure: HashSet<&str> = scans
            .iter()
            .filter(|(_, s)| s.impure)
            .map(|(n, _)| *n)
            .collect();
        loop {
            let mut changed = false;
            for (n, s) in &scans {
                if impure.contains(n) {
                    continue;
                }
                if s.callees
                    .iter()
                    .any(|c| !names.contains(c.as_str()) || impure.contains(c.as_str()))
                {
                    impure.insert(n);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let mut by_name = HashMap::new();
        for d in decls {
            let pure = !impure.contains(d.name.name.as_str());
            by_name.insert(d.name.name.clone(), HelperEntry { decl: d, pure });
        }
        Ok(HelperRegistry { by_name })
    }

    pub(crate) fn get(&self, name: &str) -> Option<&HelperEntry<'a>> {
        self.by_name.get(name)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }
}

/// Lower one pure helper into a standalone `TbFunction` (kind Helper).
/// Locals `0..params.len()` mirror the params (verifier convention).
pub(crate) fn lower_pure_helper<'a>(
    id: FunctionId,
    decl: &FunctionDecl,
    helpers: &'a HelperRegistry<'a>,
    ctx: &'a LowerCtx,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers);
    b.in_pure_helper = true;
    let mut params = Vec::with_capacity(decl.params.len());
    for p in &decl.params {
        let ty = ir_type_of(p.ty.as_ref());
        let local = b.declare(&p.name.name);
        b.set_local_type(local, ty.clone());
        params.push(TypedParam {
            name: p.name.name.clone(),
            ty,
        });
    }
    if decl.return_ty.is_some() {
        let ret = b.declare("__ret");
        b.helper_ret = Some(ret);
    }
    b.lower_block_stmts(&decl.body)?;
    if !b.is_terminated() {
        b.terminate(Terminator::Return);
    }
    let mut f = b.finish(id, decl.name.name.clone(), FunctionKind::Helper, None)?;
    f.params = params;
    Ok(f)
}

impl FuncBuilder<'_> {
    /// Lower a call to a declared helper. Pure helpers stay
    /// `Expr::Call`; impure helpers are CFG-inlined here and the call
    /// evaluates to the inlined return-value local.
    pub(crate) fn lower_helper_call(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<Expr, LowerError> {
        let entry = self
            .helpers
            .get(name)
            .expect("caller checked registry membership");
        let decl = entry.decl;
        if args.len() != decl.params.len() {
            return Err(LowerError::Invalid(format!(
                "helper `{name}` takes {} argument(s), call passes {}",
                decl.params.len(),
                args.len()
            )));
        }
        let mut arg_exprs = Vec::with_capacity(args.len());
        for a in args {
            match a {
                CallArg::Expr(e) => arg_exprs.push(e),
                CallArg::Named { .. } => {
                    return Err(unsupported(
                        &format!("named arguments in helper call `{name}(...)`"),
                        "",
                    ));
                }
            }
        }

        if entry.pure {
            let mut lowered = Vec::with_capacity(arg_exprs.len());
            for e in arg_exprs {
                lowered.push(self.lower_expr(e)?);
            }
            return Ok(Expr::Call(CallTarget::Helper(name.to_string()), lowered));
        }

        // ── CFG inline ──────────────────────────────────────────────
        if self.in_fmt_args {
            return Err(unsupported(
                &format!("DUT/sync-touching helper call `{name}(...)` inside a message"),
                "log/fail messages evaluate lazily; hoist the call into a `let` first",
            ));
        }
        if self.inline_frames.iter().any(|f| f.name == name) {
            return Err(unsupported(
                "recursive helper functions",
                format!("`{name}` is already being inlined"),
            ));
        }

        // Evaluate arguments in the caller's scope, before the helper's
        // params shadow anything.
        enum Bound {
            Dut,
            Val(Expr),
        }
        let mut bound = Vec::with_capacity(arg_exprs.len());
        for (p, e) in decl.params.iter().zip(&arg_exprs) {
            if is_dut_ident(self, e) {
                bound.push(Bound::Dut);
            } else if matches!(p.ty, Some(TypeExpr::Named { .. })) {
                return Err(unsupported(
                    &format!(
                        "helper parameter `{}` of module type with a non-DUT argument",
                        p.name.name
                    ),
                    "",
                ));
            } else {
                bound.push(Bound::Val(self.lower_expr_no_ports(e)?));
            }
        }

        // Result slot, default-initialized so paths that fall off the
        // helper's end still define it.
        let dest = self.fresh_temp();
        self.push(Stmt::Assign(
            dest,
            Expr::Literal {
                value: 0,
                ty: IrType::Unknown,
            },
        ));
        let cont = self.new_block();

        let mut dut_aliases = HashSet::new();
        for (p, b) in decl.params.iter().zip(&bound) {
            if matches!(b, Bound::Dut) {
                dut_aliases.insert(p.name.name.clone());
            }
        }
        self.inline_frames.push(InlineFrame {
            name: name.to_string(),
            dut_aliases,
            ret_dest: dest,
            ret_cont: cont,
            scope_floor: self.scope_depth(),
            loop_floor: self.loop_stack.len(),
        });
        self.push_scope();
        for (p, b) in decl.params.iter().zip(bound) {
            if let Bound::Val(e) = b {
                let id = self.declare(&p.name.name);
                self.push(Stmt::Assign(id, e));
            }
        }

        self.lower_block_stmts(&decl.body)?;
        if !self.is_terminated() {
            self.terminate(Terminator::Jump(cont));
        }

        self.pop_scope();
        self.inline_frames.pop();
        self.start_block(cont);
        Ok(Expr::Local(dest))
    }

    /// `return` lowering for helper contexts. Returns `false` when not
    /// in a helper context (caller handles run/check semantics).
    pub(crate) fn lower_helper_return(
        &mut self,
        value: Option<&AstExpr>,
    ) -> Result<bool, LowerError> {
        if let Some(frame) = self.inline_frames.last() {
            let (dest, cont) = (frame.ret_dest, frame.ret_cont);
            if let Some(e) = value {
                if let Some(port) = self.as_port_ref(e)? {
                    self.push(Stmt::DutRead(dest, port));
                } else {
                    let ir = self.lower_expr_no_ports(e)?;
                    self.push(Stmt::Assign(dest, ir));
                }
            }
            self.terminate(Terminator::Jump(cont));
            return Ok(true);
        }
        if let Some(ret) = self.helper_ret {
            if let Some(e) = value {
                let ir = self.lower_expr_no_ports(e)?;
                self.push(Stmt::Assign(ret, ir));
            }
            self.terminate(Terminator::Return);
            return Ok(true);
        }
        Ok(false)
    }
}

fn is_dut_ident(b: &FuncBuilder<'_>, e: &AstExpr) -> bool {
    match &*e.kind {
        ExprKind::Ident(id) => b.is_dut_name(&id.name),
        ExprKind::Paren(inner) => is_dut_ident(b, inner),
        _ => false,
    }
}

pub(crate) fn ir_type_of(ty: Option<&TypeExpr>) -> IrType {
    let Some(TypeExpr::Builtin { name, args, .. }) = ty else {
        return IrType::Unknown;
    };
    let width = || match args.first() {
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse::<u32>().ok(),
            _ => None,
        },
        _ => None,
    };
    match name {
        BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => IrType::UInt(width()),
        BuiltinTy::SInt | BuiltinTy::SIntCap => IrType::SInt(width()),
        BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => IrType::Bool,
        _ => IrType::Unknown,
    }
}

// ── Conservative purity / call-graph scan ───────────────────────────

struct Scan {
    impure: bool,
    callees: Vec<String>,
}

fn scan_decl(d: &FunctionDecl) -> Scan {
    let mut s = Scan {
        impure: false,
        callees: Vec::new(),
    };
    // A param of module (Named) type is a DUT handle.
    if d.params
        .iter()
        .any(|p| matches!(p.ty, Some(TypeExpr::Named { .. })))
    {
        s.impure = true;
    }
    scan_block(&d.body, &mut s);
    s
}

fn scan_block(b: &Block, s: &mut Scan) {
    for st in &b.stmts {
        scan_stmt(st, s);
    }
}

fn scan_stmt(st: &AstStmt, s: &mut Scan) {
    match &st.kind {
        StmtKind::Let(l) => {
            if !l.probes.is_empty() || l.bind {
                s.impure = true;
            }
            if let Some(v) = &l.value {
                scan_expr(v, s);
            }
        }
        StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
            scan_expr(target, s);
            scan_expr(value, s);
        }
        StmtKind::If(i) => {
            scan_expr(&i.cond, s);
            scan_block(&i.then_block, s);
            for (c, b) in &i.elsifs {
                scan_expr(c, s);
                scan_block(b, s);
            }
            if let Some(eb) = &i.else_block {
                scan_block(eb, s);
            }
        }
        StmtKind::For(f) => {
            scan_expr(&f.iter, s);
            scan_block(&f.body, s);
        }
        StmtKind::Repeat(r) => {
            scan_expr(&r.count, s);
            scan_block(&r.body, s);
        }
        StmtKind::While { cond, body, .. } => {
            scan_expr(cond, s);
            scan_block(body, s);
        }
        StmtKind::Loop(b) => scan_block(b, s),
        StmtKind::Break { .. } | StmtKind::Continue { .. } => {}
        StmtKind::Return(v) => {
            if let Some(e) = v {
                scan_expr(e, s);
            }
        }
        // Everything else needs the DUT, the scheduler, or the runtime
        // log context (`wait`, `log`, `assert`, `fail`, ...), or is
        // outside the recognized subset entirely.
        _ => s.impure = true,
    }
}

fn scan_expr(e: &AstExpr, s: &mut Scan) {
    match &*e.kind {
        ExprKind::Int(_) | ExprKind::Bool(_) => {}
        ExprKind::Ident(id) => {
            // Lexically captured DUT reference (v1 lambdas capture
            // `dut` by reference; the inlined form resolves it via the
            // caller's context).
            if id.name == "dut" {
                s.impure = true;
            }
        }
        ExprKind::Paren(inner) => scan_expr(inner, s),
        ExprKind::Unary { expr, .. } => scan_expr(expr, s),
        ExprKind::Binary { op, lhs, rhs } => {
            // Temporal / membership operators are outside the lowered
            // expression subset; classify impure so the precise error
            // surfaces at a call site (or not at all if never called).
            use crate::ast::BinaryOp as B;
            if matches!(
                op,
                B::PipeImplies
                    | B::PipeImpliesNext
                    | B::Throughout
                    | B::Within
                    | B::Intersect
                    | B::In
                    | B::Inside
            ) {
                s.impure = true;
            }
            scan_expr(lhs, s);
            scan_expr(rhs, s);
        }
        // Literal `lo .. hi` ranges appear as `for` iterables and are
        // lowered; open ranges are not.
        ExprKind::RangeLit {
            lo: Some(lo),
            hi: Some(hi),
        } => {
            scan_expr(lo, s);
            scan_expr(hi, s);
        }
        ExprKind::Call { callee, args } => {
            match &*callee.kind {
                ExprKind::Ident(id) => s.callees.push(id.name.clone()),
                _ => s.impure = true,
            }
            for a in args {
                match a {
                    CallArg::Expr(e) => scan_expr(e, s),
                    CallArg::Named { value, .. } => {
                        s.impure = true;
                        scan_expr(value, s);
                    }
                }
            }
        }
        // Field accesses are potential DUT port accesses (`d.rd_data`
        // through a DUT-typed param); anything else unrecognized is
        // conservatively impure — call-site lowering reports precisely.
        _ => s.impure = true,
    }
}

/// DFS cycle check over helper-to-helper call edges. Rejects direct
/// and mutual recursion with the offending cycle path in the message.
fn check_acyclic<'a>(
    decls: &[&'a FunctionDecl],
    scans: &'a HashMap<&'a str, Scan>,
) -> Result<(), LowerError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }

    fn dfs<'a>(
        n: &'a str,
        scans: &'a HashMap<&'a str, Scan>,
        color: &mut HashMap<&'a str, Color>,
        path: &mut Vec<&'a str>,
    ) -> Result<(), LowerError> {
        color.insert(n, Color::Grey);
        path.push(n);
        if let Some(s) = scans.get(n) {
            for c in &s.callees {
                let Some((key, _)) = scans.get_key_value(c.as_str()) else {
                    continue; // not a helper; handled at call sites
                };
                let key: &'a str = *key;
                match color[key] {
                    Color::Grey => {
                        let start = path.iter().position(|p| *p == key).unwrap_or(0);
                        let mut cycle: Vec<&str> = path[start..].to_vec();
                        cycle.push(key);
                        return Err(unsupported(
                            "recursive helper functions",
                            format!("cycle: {}", cycle.join(" -> ")),
                        ));
                    }
                    Color::White => dfs(key, scans, color, path)?,
                    Color::Black => {}
                }
            }
        }
        path.pop();
        color.insert(n, Color::Black);
        Ok(())
    }

    let mut color: HashMap<&str, Color> = scans.keys().map(|n| (*n, Color::White)).collect();
    // Deterministic order: declaration order, not HashMap order.
    for d in decls {
        let n = d.name.name.as_str();
        if color[n] == Color::White {
            dfs(n, scans, &mut color, &mut Vec::new())?;
        }
    }
    Ok(())
}
