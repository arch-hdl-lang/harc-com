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

use super::{
    exprs::width_cast_kind, not_implemented, unsupported, FuncBuilder, InlineFrame, LowerCtx,
    LowerError, SideTables, V1Status,
};
use crate::ast::{
    Block, BuiltinTy, CallArg, Expr as AstExpr, ExprKind, FunctionDecl, Item, SourceFile,
    Stmt as AstStmt, StmtKind, TypeArg, TypeExpr,
};
use crate::ir::{
    CallTarget, Expr, FunctionId, FunctionKind, IrType, RecordId, Stmt, TbFunction, Terminator,
    TypedParam,
};
use std::cell::RefCell;
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

        // Impurity fixpoint: a helper is impure if its own body is, or
        // if it calls an impure helper (an inlined body cannot live
        // inside a file-scope C++ function), or if it calls a name
        // that is not a declared helper (defer the error to use site).
        //
        // Runs BEFORE the recursion check, because whether a cycle is
        // admissible depends on purity: a PURE helper emits as a
        // file-scope C++ function with a prototype ahead of every body,
        // so it can call itself. An impure one is CFG-inlined at each
        // call site, and inlining a cycle does not terminate.
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

        // Recursion check: DFS over helper-only call edges, rejecting a
        // cycle only when it passes through an impure (inlined) helper.
        check_acyclic(&decls, &scans, &impure)?;

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
    side_tables: &'a RefCell<SideTables>,
) -> Result<TbFunction, LowerError> {
    let mut b = FuncBuilder::new(ctx, helpers, side_tables);
    b.pure_helper_abi = true;
    let mut params = Vec::with_capacity(decl.params.len());
    for p in &decl.params {
        let ty = pure_helper_signature_type(
            &decl.name.name,
            &format!("parameter `{}`", p.name.name),
            p.ty.as_ref(),
            &ctx.record_ids,
        )?;
        let local = b.declare(&p.name.name);
        b.set_local_type(local, ty.clone());
        params.push(TypedParam {
            name: p.name.name.clone(),
            ty,
        });
    }
    if decl.return_ty.is_some() {
        let ret_ty = pure_helper_signature_type(
            &decl.name.name,
            "return type",
            decl.return_ty.as_ref(),
            &ctx.record_ids,
        )?;
        if matches!(ret_ty, IrType::Record(_)) {
            return Err(not_implemented(
                &format!("record return from pure helper `{}`", decl.name.name),
                "v1 types a named return as a Verilated module pointer and then returns a \
                 record value, so its generated C++ does not compile",
                V1Status::EmitsUncompilable,
            ));
        }
        let ret = b.declare("__ret");
        b.set_local_type(ret, ret_ty);
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
    /// Is `e` a call to a helper whose declared return type is a fixed
    /// vector of EXACTLY `expected`'s shape? (harc#745 Finding 2.)
    ///
    /// This is the one non-path whole-vector RHS v1 composes into
    /// well-typed C++ — the callee returns a `std::array` that drops
    /// straight into another `std::array` slot of the same shape — so it
    /// is a TB-IR gap (`unsupported`, v1 is the honest fallback), NOT a
    /// program error.
    ///
    /// The shape MUST match `expected`. A composed call whose element
    /// type or length differs (`echo2() -> Vec<_, 2>` into a `Vec<_, 4>`
    /// slot) is a mismatch v1 also fails to compile, so it stays
    /// `Invalid` rather than being pointed at a v1 that cannot help — the
    /// same honesty rule that keeps a scalar `return 1` `Invalid`.
    /// (`fixed_vec_ir_type_with_records` also decodes scalars, so a
    /// scalar-returning helper call never satisfies the `FixedVec`
    /// equality either.)
    fn is_fixed_vec_helper_call(&self, e: &AstExpr, expected: &IrType) -> bool {
        if !matches!(expected, IrType::FixedVec { .. }) {
            return false;
        }
        let mut cur = e;
        loop {
            match &*cur.kind {
                ExprKind::Paren(inner) => cur = inner,
                ExprKind::Call { callee, .. } => {
                    let ExprKind::Ident(name) = &*callee.kind else {
                        return false;
                    };
                    return self.helpers.get(&name.name).is_some_and(|entry| {
                        entry
                            .decl
                            .return_ty
                            .as_ref()
                            .and_then(|ty| {
                                super::components::fixed_vec_ir_type_with_records(
                                    ty,
                                    &self.ctx.record_ids,
                                )
                            })
                            .is_some_and(|ty| &ty == expected)
                    });
                }
                _ => return false,
            }
        }
    }

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
        // v1 drops argument names and binds strictly by position, so a
        // name written in its own position is inert (measured:
        // `hlp(a = 111, b = 222)` emits `hlp(111, 222)`, byte-identical
        // to the positional form) and one written elsewhere silently
        // SWAPS the values (`hlp(b = 222, a = 111)` emits
        // `hlp(222, 111)`). Refusing the whole construct refused the
        // inert form too; the shared guard splits the three cases and
        // reads the parameter names off the declaration, so nothing is
        // invented.
        let declared: Vec<String> = decl.params.iter().map(|p| p.name.name.clone()).collect();
        super::reject_misplaced_named_args(args, &declared, &format!("helper call `{name}(...)`"))?;
        let arg_exprs: Vec<&crate::ast::Expr> = args
            .iter()
            .map(|a| match a {
                CallArg::Expr(e) | CallArg::Named { value: e, .. } => e,
            })
            .collect();

        if entry.pure {
            let mut lowered = Vec::with_capacity(arg_exprs.len());
            for (p, e) in decl.params.iter().zip(arg_exprs) {
                let param_ty = pure_helper_signature_type(
                    name,
                    &format!("parameter `{}`", p.name.name),
                    p.ty.as_ref(),
                    &self.ctx.record_ids,
                )?;
                let v = if matches!(param_ty, IrType::FixedVec { .. }) {
                    // `whole_vec_value_rhs` returns `None` for a NON-PATH
                    // argument. A fixed-vector-returning helper call there
                    // (`f(echo_vec(v))`) is one v1 composes into well-typed
                    // C++, so it is a TB-IR gap (`unsupported`, v1 is the
                    // honest fallback), not the `Invalid` program error that
                    // would violate the tbir-mvp rule "`Invalid` runs on NO
                    // backend" (harc#745 Finding 2). Any other non-path — a
                    // scalar, an arithmetic expression — is a genuine
                    // mismatch v1 also refuses, and stays `Invalid`.
                    let value = self.whole_vec_value_rhs(e)?.ok_or_else(|| {
                        if self.is_fixed_vec_helper_call(e, &param_ty) {
                            unsupported(
                                &format!(
                                    "a composed fixed-vector value for parameter `{}` of helper `{name}`",
                                    p.name.name
                                ),
                                "this position needs a named whole-vector path (a testbench field \
                                 or element chain); a fixed-vector call result is not yet hoisted \
                                 into one",
                            )
                        } else {
                            LowerError::Invalid(format!(
                                "parameter `{}` of helper `{name}` requires a matching whole-vector value",
                                p.name.name
                            ))
                        }
                    })?;
                    if self.ir_whole_vec_type(&value).as_ref() != Some(&param_ty) {
                        return Err(LowerError::Invalid(format!(
                            "parameter `{}` of helper `{name}` expects {param_ty:?}, got {:?}",
                            p.name.name,
                            self.ir_whole_vec_type(&value).unwrap_or(IrType::Unknown)
                        )));
                    }
                    value
                } else {
                    self.lower_expr(e)?
                };
                if self.record_id_of_expr(&v).is_some() {
                    return Err(not_implemented(
                        &format!(
                            "record argument passed to scalar-ABI parameter `{}` of pure helper `{name}`",
                            p.name.name
                        ),
                        "pure-helper parameters use the scalar C++ ABI; v1 also emits a \
                         non-record parameter and fails to compile the record argument",
                        V1Status::EmitsUncompilable,
                    ));
                }
                self.check_param_slot(&v, p, &format!("helper `{name}`"))?;
                lowered.push(v);
            }
            let ret = pure_helper_signature_type(
                name,
                "return type",
                decl.return_ty.as_ref(),
                &self.ctx.record_ids,
            )?;
            return Ok(Expr::Call(
                CallTarget::Helper {
                    name: name.to_string(),
                    ret,
                },
                lowered,
            ));
        }

        // ── CFG inline ──────────────────────────────────────────────
        if self.in_fmt_args {
            return Err(unsupported(
                &format!("DUT/sync-touching helper call `{name}(...)` inside a message"),
                "log/fail messages evaluate lazily; hoist the call into a `let` first",
            ));
        }
        if self.inline_frames.iter().any(|f| f.name == name) {
            // Only reachable for an INLINED (impure) helper — a pure one
            // is called, not inlined, and its recursion is admissible.
            // v1 emits every helper as an `auto` lambda, and a lambda
            // that names itself inside its own initializer does not
            // compile, so there is no v1 escape hatch here.
            return Err(not_implemented(
                "recursive DUT/sync-touching helper functions",
                format!(
                    "`{name}` is already being inlined; a PURE recursive helper lowers fine \
                     (it emits as a real C++ function)"
                ),
                V1Status::EmitsUncompilable,
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
            } else if matches!(p.ty, Some(TypeExpr::Named { .. }))
                && !matches!(ir_type_of_param(p.ty.as_ref(), self.ctx), IrType::Record(_))
            {
                // v1 emits the call against a lambda typed on the
                // MODULE — `auto touch = [&](VTop* d) -> uint64_t` —
                // and passes whatever was written: `touch(m)` with `m`
                // a component gives "no match for call to
                // `<lambda(VTop*)>` (Model&)". Measured with
                // `-std=gnu++20`, the standard `src/main.rs` builds
                // with; the DUT-argument spelling compiles clean, so
                // the arm is specifically about the other one.
                return Err(not_implemented(
                    &format!(
                        "helper parameter `{}` of module type with a non-DUT argument",
                        p.name.name
                    ),
                    "v1 types the emitted lambda on the module and passes the argument \
                     through, so g++ rejects the call",
                    V1Status::EmitsUncompilable,
                ));
            } else {
                let v = self.lower_expr_no_ports(e)?;
                // The INLINED spelling is the one that mattered most:
                // without this, `poke(b)` on a `x: uint<8>` parameter
                // reached `verify_program`'s `TypeMismatch` and
                // surfaced as "internal error: TB-IR failed
                // verification after lowering" — a program error
                // answered through the compiler-bug channel, the third
                // place in this sweep that has happened.
                self.check_param_slot(&v, p, &format!("helper `{name}`"))?;
                bound.push(Bound::Val(v));
            }
        }

        // Result slot, default-initialized so paths that fall off the
        // helper's end still define it.
        let dest = self.fresh_temp();
        let mut ret_ty = IrType::Unknown;
        if decl.return_ty.is_some() {
            ret_ty = ir_type_of_with_records(decl.return_ty.as_ref(), &self.ctx.record_ids);
            self.set_local_type(dest, ret_ty.clone());
        }
        self.push_return_default(dest, &ret_ty);
        let cont = self.new_block();

        let mut dut_aliases = HashSet::new();
        for (p, b) in decl.params.iter().zip(&bound) {
            if matches!(b, Bound::Dut) {
                dut_aliases.insert(p.name.name.clone());
            }
        }
        self.inline_frames.push(InlineFrame {
            name: name.to_string(),
            is_testbench_method: false,
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
                self.set_local_type(id, ir_type_of_param(p.ty.as_ref(), self.ctx));
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

    /// One declared parameter's slot check, shared by the helper and
    /// testbench-method arms. Resolves through `slot_ir_type`, which
    /// answers for the two shapes a schema's `param_tys` cannot: a
    /// `TSeq<T>` (described as the sequence it is, rather than as "a
    /// non-record value") and an `int`/`time` scalar.
    pub(crate) fn check_param_slot(
        &self,
        v: &Expr,
        p: &crate::ast::Param,
        owner: &str,
    ) -> Result<(), LowerError> {
        let want = slot_ir_type(p.ty.as_ref(), &self.ctx.record_ids);
        let what = format!("parameter `{}` of {owner}", p.name.name);
        self.check_slot_ir(v, &want, &what)?;
        if matches!(want, IrType::FixedVec { .. }) {
            if let Some(actual @ IrType::FixedVec { .. }) = self.expr_type(v) {
                if actual != want {
                    return Err(LowerError::Invalid(format!(
                        "{what} expects {want:?}, got {actual:?}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Lower a call to an `extern function name(...) -> ret` (spec §9).
    /// Mirrors v1: arguments lower as plain scalar values and the call
    /// stays an `Expr::Call(CallTarget::ExternFn, ...)`, emitted with the
    /// RAW symbol name so it binds to the user's `extern "C"` definition
    /// linked via `--ref-src`. An extern fn is PURE — it never
    /// CFG-inlines and (unlike an impure helper) never takes a DUT
    /// handle, so arguments lower without port access.
    ///
    /// Not "scalar", which this comment used to say and which the loop
    /// below used to enforce: `c_type_for` renders `TSeq<T>` as
    /// `const std::vector<T>&`, and a sequence argument compiles under
    /// both backends. Purity is the property the lowering depends on;
    /// scalar-ness was an assumption sitting next to it.
    pub(crate) fn lower_extern_fn_call(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<Expr, LowerError> {
        // Same measurement as the helper arm above:
        // `ref_add(b = 222, a = 111)` emits `ref_add(222, 111)` under
        // v1, and the in-order form emits the positional one unchanged.
        // One lookup, not two, and no `None` path on either: the sole
        // caller (`exprs.rs`) dispatches here only when the name is in
        // `extern_fns`. The previous shape had three separate
        // expressions of that impossible `None` — an `if let` that
        // silently skipped the named-argument check, an
        // `unwrap_or_default`, and a `contains_key` guard — and the
        // commit that removed two of them said in its own message that
        // the lookup always hits.
        let (pnames, ptys, ret) = self.ctx.extern_fns[name].clone();
        super::reject_misplaced_named_args(
            args,
            &pnames,
            &format!("extern fn call `{name}(...)`"),
        )?;
        // Arity, before the slot loop — the same order
        // `check_component_call_args` uses, and for the reason its doc
        // records: without it the `zip`/`get` silently stops at the
        // shorter side. Measured with a compiling control (`f(1)` on a
        // one-parameter fn): `f(1, 2)` gives "too many arguments to
        // function `uint64_t f(uint64_t)`" and `g(1)` on a two-parameter
        // fn "too few arguments", from both backends.
        //
        // This also retires a `None` arm that checked a surplus argument
        // against a fabricated scalar slot — `f(1, b)` reported that
        // "an argument of extern fn `f` takes a non-record value",
        // describing parameter #2 of a one-parameter function. A slot
        // that does not exist has no type to disagree with.
        if args.len() != pnames.len() {
            return Err(LowerError::Invalid(format!(
                "extern fn `{name}` takes {} argument(s), call passes {}",
                pnames.len(),
                args.len()
            )));
        }
        let mut lowered = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            match a {
                CallArg::Expr(e) | CallArg::Named { value: e, .. } => {
                    let v = self.lower_expr_no_ports(e)?;
                    // Arity is checked above and `pnames`/`ptys` come
                    // from the same `d.params`, so both indexes are
                    // total. The `None` labels these used to carry
                    // described a parameter that does not exist — the
                    // very diagnostic the arity check exists to replace.
                    let slot = format!("parameter `{}` of extern fn `{name}`", pnames[i]);
                    // Against the DECLARED type, not against "scalar".
                    // A comment here twice asserted that every extern-fn
                    // parameter is a scalar — citing this module's own
                    // header — and the loop hard-coded the slot to match.
                    // `TSeq<T>` is the counterexample: `cpp_tb`'s
                    // `c_type_for` renders it `const std::vector<T>&`,
                    // and `extern function ref_sum(xs: TSeq<Beat>)`
                    // called with a sequence compiles under v1 and under
                    // tbir at the merge base. Hard-coding the slot made
                    // the DEFAULT backend reject it — the exact class of
                    // false `Invalid` this family exists to remove,
                    // reintroduced by the check meant to prevent it.
                    //
                    // The record case never reaches here: it is refused
                    // at the declaration, where both backends already
                    // break.
                    self.check_slot_ir(&v, &ptys[i], &slot)?;
                    lowered.push(v)
                }
            }
        }
        Ok(Expr::Call(
            CallTarget::ExternFn {
                name: name.to_string(),
                ret,
            },
            lowered,
        ))
    }

    /// `Some(method)` when `callee` is a call target of the form
    /// `_tb.<m>` for a testbench helper method (`function`/`hookable`
    /// declared in the bound testbench).
    pub(crate) fn tb_method_call_name(&self, callee: &AstExpr) -> Option<String> {
        let tb_field = self.ctx.tb_field.as_deref()?;
        let ExprKind::Field { target, name } = &*callee.kind else {
            return None;
        };
        let ExprKind::Ident(root) = &*target.kind else {
            return None;
        };
        (root.name == tb_field && self.ctx.tb_methods.contains_key(&name.name))
            .then(|| name.name.clone())
    }

    /// CFG-inline a testbench helper method call (`_tb.reset()`,
    /// `_tb.bump(5)`). Same shape as the impure-helper inline: params
    /// become fresh caller locals, the body lowers into the caller's
    /// blocks under an `InlineFrame` (so the body sees only its own
    /// scopes — plus the DUT, which a testbench method touches through
    /// the shared `dut` field, resolved by `is_dut_name` exactly as in
    /// the test body). v1 emits these as `[&]`-capturing lambdas whose
    /// `wait`s tick the same scheduler, so the inlined CFG's cycle
    /// behavior is identical.
    pub(crate) fn lower_tb_method_call(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<Expr, LowerError> {
        let decl = self
            .ctx
            .tb_methods
            .get(name)
            .expect("caller checked tb_methods membership");
        if args.len() != decl.params.len() {
            return Err(LowerError::Invalid(format!(
                "testbench method `{name}` takes {} argument(s), call passes {}",
                decl.params.len(),
                args.len()
            )));
        }
        if self.in_fmt_args {
            return Err(unsupported(
                &format!("testbench method call `{name}(...)` inside a message"),
                "log/fail messages evaluate lazily; hoist the call into a `let` first",
            ));
        }
        let frame_name = format!("_tb.{name}");
        if self.inline_frames.iter().any(|f| f.name == frame_name) {
            // Testbench methods are always inlined (they capture the
            // shared `_tb` host state), so a cycle cannot terminate.
            // v1 emits them as `auto` lambdas, which cannot name
            // themselves in their own initializer — no v1 escape hatch.
            return Err(not_implemented(
                "recursive testbench methods",
                format!("`{name}` is already being inlined"),
                V1Status::EmitsUncompilable,
            ));
        }

        // Same measurement as the helper and extern-fn arms:
        // `hlp(b = 222, a = 111)` emits `Tb_hlp(_tb, 222, 111)` under
        // v1 — silently swapped — while the in-order form emits the
        // positional one unchanged.
        let declared: Vec<String> = decl.params.iter().map(|p| p.name.name.clone()).collect();
        super::reject_misplaced_named_args(
            args,
            &declared,
            &format!("testbench method call `{name}(...)`"),
        )?;
        let arg_exprs: Vec<&crate::ast::Expr> = args
            .iter()
            .map(|a| match a {
                CallArg::Expr(e) | CallArg::Named { value: e, .. } => e,
            })
            .collect();
        enum Bound {
            Dut,
            Val(Expr),
        }
        let mut bound = Vec::with_capacity(arg_exprs.len());
        for (p, e) in decl.params.iter().zip(&arg_exprs) {
            if is_dut_ident(self, e) {
                bound.push(Bound::Dut);
            } else if matches!(p.ty, Some(TypeExpr::Named { .. }))
                && !matches!(ir_type_of_param(p.ty.as_ref(), self.ctx), IrType::Record(_))
            {
                // The testbench-method sibling of the helper arm
                // above, measured on its own rather than inferred from
                // it: v1 emits `Tb_peek(_tb, _tb.m)` against
                // `[&](Tb& self, VTop* d)` and g++ gives "no match for
                // call to `<lambda(Tb&, VTop*)>` (Tb&, Model&)".
                return Err(not_implemented(
                    &format!(
                        "testbench method parameter `{}` of module type with a non-DUT argument",
                        p.name.name
                    ),
                    "v1 types the emitted lambda on the module and passes the argument \
                     through, so g++ rejects the call",
                    V1Status::EmitsUncompilable,
                ));
            } else {
                let param_ty = testbench_method_signature_type(
                    name,
                    &format!("parameter `{}`", p.name.name),
                    p.ty.as_ref(),
                    &self.ctx.record_ids,
                )?;
                let v = if matches!(param_ty, IrType::FixedVec { .. }) {
                    // Same split as the helper-parameter sibling above
                    // (harc#745 Finding 2): a composed fixed-vector value
                    // such as `m(echo_vec(v))` is a tbir gap v1 compiles
                    // (`unsupported`); any other non-path is a genuine
                    // mismatch v1 also refuses (`Invalid`).
                    let value = self.whole_vec_value_rhs(e)?.ok_or_else(|| {
                        if self.is_fixed_vec_helper_call(e, &param_ty) {
                            unsupported(
                                &format!(
                                    "a composed fixed-vector value for parameter `{}` of testbench method `{name}`",
                                    p.name.name
                                ),
                                "this position needs a named whole-vector path (a testbench field \
                                 or element chain); a fixed-vector call result is not yet hoisted \
                                 into one",
                            )
                        } else {
                            LowerError::Invalid(format!(
                                "parameter `{}` of testbench method `{name}` requires a whole fixed-vector value",
                                p.name.name
                            ))
                        }
                    })?;
                    if self.ir_whole_vec_type(&value).as_ref() != Some(&param_ty) {
                        return Err(LowerError::Invalid(format!(
                            "parameter `{}` of testbench method `{name}` expects {param_ty:?}, got {:?}",
                            p.name.name,
                            self.ir_whole_vec_type(&value).unwrap_or(IrType::Unknown)
                        )));
                    }
                    value
                } else {
                    self.lower_expr_no_ports(e)?
                };
                // The testbench-method spelling of the slot rule.
                // `ir_type_of_param` is already in hand two lines up, so
                // this needed no new type table — the note that deferred
                // it said otherwise and was wrong. Measured: `tf(1)`
                // into a `t: Beat` parameter lowered and emitted, and
                // both backends answer "no match for call to"; `tf(o)`
                // on a DIFFERENT record reached the VERIFIER's
                // `TypeMismatch` and surfaced as "internal error: TB-IR
                // failed verification after lowering" — a program error
                // answered through the compiler-bug channel.
                self.check_param_slot(&v, p, &format!("testbench method `{name}`"))?;
                bound.push(Bound::Val(v));
            }
        }

        let dest = self.fresh_temp();
        let mut ret_ty = IrType::Unknown;
        if decl.return_ty.is_some() {
            ret_ty = testbench_method_signature_type(
                name,
                "return type",
                decl.return_ty.as_ref(),
                &self.ctx.record_ids,
            )?;
            self.set_local_type(dest, ret_ty.clone());
        }
        self.push_return_default(dest, &ret_ty);
        let cont = self.new_block();

        let mut dut_aliases = HashSet::new();
        for (p, b) in decl.params.iter().zip(&bound) {
            if matches!(b, Bound::Dut) {
                dut_aliases.insert(p.name.name.clone());
            }
        }
        self.inline_frames.push(InlineFrame {
            name: frame_name,
            is_testbench_method: true,
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
                self.set_local_type(
                    id,
                    testbench_method_signature_type(
                        name,
                        &format!("parameter `{}`", p.name.name),
                        p.ty.as_ref(),
                        &self.ctx.record_ids,
                    )?,
                );
                self.push(Stmt::Assign(id, e));
            }
        }

        let body = decl.body.clone();
        self.lower_block_stmts(&body)?;
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
                    let expected = self.local_type(dest).clone();
                    let ir = if let IrType::FixedVec { elem, len } = &expected {
                        let shape = crate::codegen::cpp_tb::ir_vec_elem_class(elem)
                            .map(|class| (*len, class));
                        match shape {
                            Some(shape) => self.whole_vec_copy_rhs(shape, e)?,
                            None => None,
                        }
                        .ok_or_else(|| {
                            // `whole_vec_copy_rhs` declines a composed
                            // fixed-vector call (`return echo_vec(v)`), a
                            // path of the wrong shape, and a scalar alike.
                            // Only the first is one v1 compiles (the return
                            // type is a `std::array`), so it is a TB-IR gap
                            // (`unsupported`, v1 is the honest fallback);
                            // classifying it as `Invalid` broke the
                            // tbir-mvp rule that `Invalid` runs on NO
                            // backend (harc#745 Finding 2). Every other
                            // shape stays a program error (`Invalid`).
                            if self.is_fixed_vec_helper_call(e, &expected) {
                                unsupported(
                                    "a composed fixed-vector testbench method return",
                                    "the return value must be a named whole-vector path (a \
                                     testbench field or element chain); a fixed-vector call \
                                     result is not yet hoisted into one",
                                )
                            } else {
                                LowerError::Invalid(
                                    "fixed-vector testbench method return requires a matching whole-vector value"
                                        .to_string(),
                                )
                            }
                        })?
                    } else {
                        self.lower_expr_no_ports(e)?
                    };
                    self.push(Stmt::Assign(dest, ir));
                }
            }
            self.terminate(Terminator::Jump(cont));
            return Ok(true);
        }
        if let Some(ret) = self.helper_ret {
            if let Some(e) = value {
                let expected = self.local_type(ret).clone();
                let ir = if let IrType::FixedVec { elem, len } = &expected {
                    let shape = crate::codegen::cpp_tb::ir_vec_elem_class(elem)
                        .map(|class| (*len, class));
                    let value = match shape {
                        Some(shape) => self.whole_vec_copy_rhs(shape, e)?,
                        None => None,
                    };
                    value.ok_or_else(|| {
                        // Same split as the testbench-method return a few
                        // lines up (harc#745 Finding 2): a composed
                        // fixed-vector helper-call return is a tbir gap v1
                        // compiles (`unsupported`); a mismatched-shape path
                        // or a scalar is a real program error (`Invalid`).
                        if self.is_fixed_vec_helper_call(e, &expected) {
                            unsupported(
                                "a composed fixed-vector helper return",
                                "the return value must be a named whole-vector path (a testbench \
                                 field or element chain); a fixed-vector call result is not yet \
                                 hoisted into one",
                            )
                        } else {
                            LowerError::Invalid(
                                "fixed-vector method return requires a matching whole-vector value"
                                    .to_string(),
                            )
                        }
                    })?
                } else {
                    self.lower_expr_no_ports(e)?
                };
                if self.pure_helper_abi {
                    if self.record_id_of_expr(&ir).is_some() {
                        let construct = if matches!(expected, IrType::FixedVec { .. }) {
                            "record value returned from a fixed-vector-valued pure helper"
                        } else {
                            "record value returned from a scalar-valued pure helper"
                        };
                        return Err(not_implemented(
                            construct,
                            "the pure-helper C++ return ABI follows the declaration; v1 also \
                             emits that return type and fails to compile the bare record value",
                            V1Status::EmitsUncompilable,
                        ));
                    }
                    self.check_slot_ir(&ir, &expected, "pure-helper return")?;
                }
                self.push(Stmt::Assign(ret, ir));
            }
            self.terminate(Terminator::Return);
            return Ok(true);
        }
        Ok(false)
    }

    fn push_return_default(&mut self, dest: crate::ir::LocalId, ty: &IrType) {
        match ty {
            IrType::Record(rid) => self.push(Stmt::RecordInit(dest, *rid)),
            IrType::FixedVec { .. } => self.push(Stmt::AggregateInit(dest)),
            _ => self.push(Stmt::Assign(
                dest,
                Expr::Literal {
                    value: 0,
                    ty: IrType::Unknown,
                },
            )),
        }
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

/// `TSeq<T>` as an `IrType` — `RecordSeq(r)` for a declared record
/// element, `Seq(scalar)` for a scalar one, `Unknown` for an element
/// this compiler cannot name. `None` when `ty` is not a `TSeq` at all.
///
/// Shared by the component-method schema (which types the sequence a
/// `hookable`/`function` parameter or return declares) and by the slot
/// check that decides whether an argument may enter such a parameter,
/// so the two cannot disagree about what `TSeq<Beat>` means.
pub(crate) fn tseq_ir_type(
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> Option<IrType> {
    let Some(TypeExpr::Builtin {
        name: BuiltinTy::TSeq,
        args,
        ..
    }) = ty
    else {
        return None;
    };
    let elem_name: Option<&str> = match args.first() {
        Some(TypeArg::Type(TypeExpr::Named { name, .. })) => {
            name.segments.last().map(|s| s.name.as_str())
        }
        Some(TypeArg::Expr(e)) => match &*e.kind {
            ExprKind::Ident(id) => Some(id.name.as_str()),
            _ => None,
        },
        _ => None,
    };
    if let Some(rid) = elem_name.and_then(|s| record_ids.get(s)) {
        return Some(IrType::RecordSeq(*rid));
    }
    if let Some(TypeArg::Type(inner)) = args.first() {
        if let ty @ (IrType::UInt(_) | IrType::SInt(_) | IrType::Bool) = ir_type_of(Some(inner)) {
            return Some(IrType::Seq(Box::new(ty)));
        }
    }
    Some(IrType::Unknown)
}

/// The declared type of a SLOT, for the argument checks — as opposed to
/// `ir_type_of_with_records`, which also types the parameter's local and
/// therefore the emitted C++.
///
/// They differ on two things. `TSeq<T>` resolves here to
/// `RecordSeq`/`Seq` (the short-circuit on the first line) where
/// `ir_type_of_with_records` falls through to `Unknown`. And `int` (and
/// `time`) are scalars that `ir_type_of` deliberately leaves `Unknown`,
/// because they carry no width and local typing wants that absence
/// preserved — a slot check does not care about width, it only asks
/// record / sequence / scalar, so it can answer where local typing must
/// not.
///
/// Use this wherever the declared `TypeExpr` is in hand. Where only a
/// schema's `param_tys` is available the check reads that instead, and
/// an `int` parameter there stays unchecked: those tables exist to type
/// locals, and widening them would change what the backend emits.
pub(crate) fn slot_ir_type(
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> IrType {
    if let Some(seq) = tseq_ir_type(ty, record_ids) {
        return seq;
    }
    if let Some(fixed) = ty.and_then(|ty| {
        super::components::fixed_vec_ir_type_with_records(ty, record_ids)
    }) {
        return fixed;
    }
    if let Some(TypeExpr::Builtin { name, .. }) = ty {
        match name {
            BuiltinTy::Int => return IrType::SInt(None),
            BuiltinTy::Time => return IrType::UInt(None),
            _ => {}
        }
    }
    ir_type_of_with_records(ty, record_ids)
}

/// Resolve the by-value ABI shared by standalone pure-helper declarations
/// and calls. Scalar and declared-record behavior is unchanged; a
/// fixed vector uses the recursive aggregate `std::array` carrier.
fn pure_helper_signature_type(
    _helper: &str,
    _what: &str,
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<IrType, LowerError> {
    if let Some(fixed @ IrType::FixedVec { .. }) = ty.and_then(|ty| {
        super::components::fixed_vec_ir_type_with_records(ty, record_ids)
    }) {
        return Ok(fixed);
    }
    Ok(ir_type_of_with_records(ty, record_ids))
}

pub(crate) fn ir_type_of_with_records(
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> IrType {
    if let Some(TypeExpr::Named { name, .. }) = ty {
        if let Some(simple) = name.segments.last().map(|s| s.name.as_str()) {
            if let Some(&rid) = record_ids.get(simple) {
                return IrType::Record(rid);
            }
        }
    }
    ir_type_of(ty)
}

fn ir_type_of_param(ty: Option<&TypeExpr>, ctx: &super::LowerCtx) -> IrType {
    ir_type_of_with_records(ty, &ctx.record_ids)
}

fn testbench_method_signature_type(
    _method: &str,
    _what: &str,
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<IrType, LowerError> {
    if let Some(fixed @ IrType::FixedVec { .. }) = ty.and_then(|ty| {
        super::components::fixed_vec_ir_type_with_records(ty, record_ids)
    }) {
        return Ok(fixed);
    }
    Ok(ir_type_of_with_records(ty, record_ids))
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
    // A param of module (Named) type is a DUT handle. Record-typed params
    // intentionally remain on the inlined path too: v1 cannot distinguish
    // those names and emits them as Verilated module pointers.
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
        // Ternaries are pure scalar selection — lowered to
        // `Expr::Ternary`, emitted as the C++ `?:` even in file-scope
        // pure-helper functions.
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            scan_expr(cond, s);
            scan_expr(then_branch, s);
            scan_expr(else_branch, s);
        }
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
                ExprKind::Field { target, name } if width_cast_kind(&name.name).is_some() => {
                    scan_expr(target, s);
                }
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
        ExprKind::Cast { expr, .. } => scan_expr(expr, s),
        // Field accesses are potential DUT port accesses (`d.rd_data`
        // through a DUT-typed param); anything else unrecognized is
        // conservatively impure — call-site lowering reports precisely.
        _ => s.impure = true,
    }
}

/// DFS cycle check over helper-to-helper call edges. Rejects direct
/// and mutual recursion with the offending cycle path in the message.
/// Reject helper-call cycles that pass through an impure helper.
///
/// A PURE helper lowers to a file-scope C++ function whose prototype is
/// emitted ahead of every body (`emit_helper_prototype`), so a recursive
/// call resolves and the program terminates on its own base case — the
/// cycle is admissible and left alone. (v1 cannot do this: it emits
/// every helper as an `auto` lambda, and a lambda that names itself in
/// its own initializer does not compile.)
///
/// An IMPURE helper is CFG-inlined at each call site, and inlining a
/// cycle does not terminate, so those are rejected here — with the
/// cycle path in the message.
fn check_acyclic<'a>(
    decls: &[&'a FunctionDecl],
    scans: &'a HashMap<&'a str, Scan>,
    impure: &HashSet<&str>,
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
        impure: &HashSet<&str>,
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
                        // A cycle among PURE helpers is fine — they emit
                        // as real recursive C++ functions. Only an
                        // inlined member makes it non-terminating.
                        if cycle.iter().any(|m| impure.contains(*m)) {
                            return Err(not_implemented(
                                "a recursive helper cycle through a DUT/sync-touching helper",
                                format!(
                                    "cycle: {}; such a helper is inlined at each call site, \
                                     so the inlining would not terminate — a PURE recursive \
                                     helper lowers fine",
                                    cycle.join(" -> ")
                                ),
                                V1Status::EmitsUncompilable,
                            ));
                        }
                    }
                    Color::White => dfs(key, scans, impure, color, path)?,
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
            dfs(n, scans, impure, &mut color, &mut Vec::new())?;
        }
    }
    Ok(())
}
