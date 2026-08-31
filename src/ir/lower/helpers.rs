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
//!   retain the caller's exact typed DUT receiver), the body lowers into
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
    LowerDiagnosticRecorder, LowerError, SideTables, V1Status,
};
use crate::ast::{
    Block, BuiltinTy, CallArg, Expr as AstExpr, ExprKind, FunctionDecl, Item, SourceFile,
    Stmt as AstStmt, StmtKind, TypeArg, TypeExpr,
};
use crate::ir::scalar::{
    contextual_value_bits, scalar_value_evidence, signed_value_bits, ScalarValueEvidence,
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
    pub function: Option<FunctionId>,
}

#[derive(Default)]
pub(crate) struct HelperRegistry<'a> {
    by_name: HashMap<String, HelperEntry<'a>>,
}

impl<'a> HelperRegistry<'a> {
    /// Collect every file-level `function`, categorize pure vs impure,
    /// and reject recursion (direct or mutual).
    pub(crate) fn build(
        file: &'a SourceFile,
        diagnostics: &LowerDiagnosticRecorder,
    ) -> Result<Self, LowerError> {
        let mut decls: Vec<&'a FunctionDecl> = Vec::new();
        for (item_index, it) in file.items.iter().enumerate() {
            if let Item::Function(f) = it {
                if let Some((param, span)) = f.params.iter().find_map(|param| {
                    nested_string_type_span(param.ty.as_ref()).map(|span| (param, span))
                }) {
                    diagnostics.record(file.item_source(item_index), span);
                    return Err(unsupported(
                        &format!(
                            "function `{}` parameter `{}` whose type contains `String`",
                            f.name.name, param.name.name
                        ),
                        "String containers and aggregates are not supported; use exact top-level `String` for a callable parameter",
                    ));
                }
                if let Some(span) = nested_string_type_span(f.return_ty.as_ref()) {
                    diagnostics.record(file.item_source(item_index), span);
                    return Err(unsupported(
                        &format!("function `{}` return type containing `String`", f.name.name),
                        "String containers and aggregates are not supported; use exact top-level `String` for a callable return",
                    ));
                }
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
            by_name.insert(
                d.name.name.clone(),
                HelperEntry {
                    decl: d,
                    pure,
                    function: None,
                },
            );
        }
        let mut next_function = 0u32;
        for item in &file.items {
            let Item::Function(decl) = item else {
                continue;
            };
            let Some(entry) = by_name.get_mut(&decl.name.name) else {
                continue;
            };
            if entry.pure && std::ptr::eq(entry.decl, decl) {
                entry.function = Some(FunctionId(next_function));
                next_function += 1;
            }
        }
        Ok(HelperRegistry { by_name })
    }

    pub(crate) fn get(&self, name: &str) -> Option<&HelperEntry<'a>> {
        self.by_name.get(name)
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Whether a value-position call to `name` is CFG-inlined and therefore
    /// emits statements before yielding its result. Pure helpers stay as an
    /// `Expr::Call`; impure helpers materialize an ordered value prelude.
    pub(crate) fn emits_value_prelude(&self, name: &str) -> bool {
        self.by_name.get(name).is_some_and(|entry| !entry.pure)
    }

    pub(crate) fn pure_count(&self) -> usize {
        self.by_name
            .values()
            .filter(|entry| entry.function.is_some())
            .count()
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
        if is_string_tseq_type(p.ty.as_ref()) {
            b.record_error_span(p.span);
            return Err(LowerError::Invalid(format!(
                "helper `{}` parameter `{}` cannot use `TSeq<String>`; String sequences are not supported",
                decl.name.name, p.name.name
            )));
        }
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
    /// Return the exact recursive fixed-vector type of a pure helper call.
    /// Parentheses are transparent; impure helpers are excluded because
    /// their CFG-inlined result is not an ordinary composable call value.
    /// The receiving argument/return slot compares this type with its own
    /// expected type after lowering, preserving the existing mismatch error.
    pub(crate) fn fixed_vec_helper_call_type(&self, e: &AstExpr) -> Option<IrType> {
        let mut cur = e;
        loop {
            match &*cur.kind {
                ExprKind::Paren(inner) => cur = inner,
                ExprKind::Call { callee, .. } => {
                    let ExprKind::Ident(name) = &*callee.kind else {
                        return None;
                    };
                    let entry = self.helpers.get(&name.name)?;
                    if !entry.pure {
                        return None;
                    }
                    return entry
                        .decl
                        .return_ty
                        .as_ref()
                        .and_then(|ty| {
                            super::components::fixed_vec_ir_type_with_records(
                                ty,
                                &self.ctx.record_ids,
                            )
                        })
                        .filter(|ty| matches!(ty, IrType::FixedVec { .. }));
                }
                _ => return None,
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
            let arg_effects: Vec<bool> = arg_exprs
                .iter()
                .map(|expr| self.expr_has_effectful_value_prelude(expr))
                .collect();
            let mut lowered = Vec::with_capacity(arg_exprs.len());
            for (index, (p, e)) in decl
                .params
                .iter()
                .zip(arg_exprs.iter().copied())
                .enumerate()
            {
                let param_ty = pure_helper_signature_type(
                    name,
                    &format!("parameter `{}`", p.name.name),
                    p.ty.as_ref(),
                    &self.ctx.record_ids,
                )?;
                let mut v = if matches!(param_ty, IrType::FixedVec { .. }) {
                    let value = self.whole_vec_value_rhs(e)?.ok_or_else(|| {
                        LowerError::Invalid(format!(
                            "parameter `{}` of helper `{name}` requires a matching whole-vector value",
                            p.name.name
                        ))
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
                let hint = match self.check_param_slot(&v, p, &format!("helper `{name}`")) {
                    Ok(hint) => hint,
                    Err(error) => {
                        self.record_error_span(e.span);
                        return Err(error);
                    }
                };
                if arg_effects[index + 1..].iter().any(|effect| *effect) {
                    v = self.materialize_ordered_value_as(v, hint);
                }
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
                    function: entry.function.ok_or_else(|| {
                        LowerError::Invalid(format!(
                            "pure helper `{name}` is missing its canonical function identity"
                        ))
                    })?,
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
            Dut(String),
            Val(Expr),
        }
        let arg_effects: Vec<bool> = arg_exprs
            .iter()
            .map(|expr| self.expr_has_effectful_value_prelude(expr))
            .collect();
        let mut bound = Vec::with_capacity(arg_exprs.len());
        for (index, (p, e)) in decl.params.iter().zip(&arg_exprs).enumerate() {
            if let Some(receiver) = dut_ident_receiver(self, e) {
                bound.push(Bound::Dut(receiver));
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
                let param_ty = inlined_helper_signature_type(
                    name,
                    &format!("parameter `{}`", p.name.name),
                    p.ty.as_ref(),
                    &self.ctx.record_ids,
                )?;
                let mut v = if matches!(param_ty, IrType::FixedVec { .. }) {
                    let value = self.whole_vec_value_rhs(e)?.ok_or_else(|| {
                        LowerError::Invalid(format!(
                            "parameter `{}` of helper `{name}` requires a matching whole-vector value",
                            p.name.name
                        ))
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
                    self.lower_expr_no_ports(e)?
                };
                // The INLINED spelling is the one that mattered most:
                // without this, `poke(b)` on a `x: uint<8>` parameter
                // reached `verify_program`'s `TypeMismatch` and
                // surfaced as "internal error: TB-IR failed
                // verification after lowering" — a program error
                // answered through the compiler-bug channel, the third
                // place in this sweep that has happened.
                let hint = match self.check_param_slot(&v, p, &format!("helper `{name}`")) {
                    Ok(hint) => hint,
                    Err(error) => {
                        self.record_error_span(e.span);
                        return Err(error);
                    }
                };
                if arg_effects[index + 1..].iter().any(|effect| *effect) {
                    v = self.materialize_ordered_value_as(v, hint);
                }
                bound.push(Bound::Val(v));
            }
        }

        // Result slot, default-initialized so paths that fall off the
        // helper's end still define it.
        let dest = self.fresh_temp();
        let mut ret_ty = IrType::Unknown;
        if decl.return_ty.is_some() {
            ret_ty = inlined_helper_signature_type(
                name,
                "return type",
                decl.return_ty.as_ref(),
                &self.ctx.record_ids,
            )?;
            self.set_local_type(dest, ret_ty.clone());
        }
        self.push_return_default(dest, &ret_ty);
        let cont = self.new_block();

        let mut dut_aliases = HashMap::new();
        for (p, b) in decl.params.iter().zip(&bound) {
            if let Bound::Dut(receiver) = b {
                dut_aliases.insert(p.name.name.clone(), receiver.clone());
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
                self.set_local_type(
                    id,
                    inlined_helper_signature_type(
                        name,
                        &format!("parameter `{}`", p.name.name),
                        p.ty.as_ref(),
                        &self.ctx.record_ids,
                    )?,
                );
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
    ) -> Result<Option<IrType>, LowerError> {
        if is_string_tseq_type(p.ty.as_ref()) {
            return Err(LowerError::Invalid(format!(
                "parameter `{}` of {owner} cannot use `TSeq<String>`; String sequences are not supported",
                p.name.name
            )));
        }
        let want = slot_ir_type(p.ty.as_ref(), &self.ctx.record_ids);
        let slot = format!("parameter `{}` of {owner}", p.name.name);
        self.check_callable_argument(v, &want, &slot)
    }

    /// Check a source value against one callable parameter before argument
    /// ordering can replace the expression with a temporary. The returned
    /// type is a contextual type for such a temporary: it preserves a real
    /// mask/literal bound instead of re-inferring the wider input leaf.
    pub(crate) fn check_callable_argument(
        &self,
        value: &Expr,
        expected: &IrType,
        what: &str,
    ) -> Result<Option<IrType>, LowerError> {
        self.check_slot_ir(value, expected, what)?;
        if matches!(expected, IrType::FixedVec { .. } | IrType::RecordSeq(_)) {
            if let Some(actual @ (IrType::FixedVec { .. } | IrType::RecordSeq(_))) =
                self.expr_type(value)
            {
                if &actual != expected {
                    return Err(LowerError::Invalid(format!(
                        "{what} expects {expected:?}, got {actual:?}"
                    )));
                }
            }
        }
        let Some(evidence) = self.scalar_value_evidence(value) else {
            return Ok(None);
        };
        if matches!(
            expected,
            IrType::Unknown | IrType::UInt(None) | IrType::SInt(None)
        ) {
            return Ok(None);
        }

        if matches!(expected, IrType::Bool) {
            return match evidence {
                ScalarValueEvidence::Bool => Ok(Some(IrType::Bool)),
                ScalarValueEvidence::Integer {
                    signed: None,
                    exact: Some(0) | Some(1),
                    ..
                } => Ok(Some(IrType::Bool)),
                _ => Err(LowerError::Invalid(format!(
                    "{what} has type Bool, but the argument is not a boolean or a literal 0/1"
                ))),
            };
        }

        let (dest_width, dest_signed) = match expected {
            IrType::UInt(Some(width)) => (*width, false),
            IrType::SInt(Some(width)) => (*width, true),
            _ => return Ok(None),
        };
        let ScalarValueEvidence::Integer {
            width,
            signed,
            exact,
            ..
        } = evidence
        else {
            return Ok(Some(expected.clone()));
        };

        if let Some(src_signed) = signed {
            if dest_signed != src_signed {
                let src_width = width.unwrap_or(64);
                let (article, from, to) = if src_signed {
                    ("a", "signed", "unsigned")
                } else {
                    ("an", "unsigned", "signed")
                };
                return Err(LowerError::Invalid(format!(
                    "assignment of {article} {from} {src_width}-bit value to {what}, declared \
                     {to} {dest_width} bits. Signedness must match — relabel the value \
                     explicitly with `as {}<{dest_width}>`.",
                    if dest_signed { "sint" } else { "uint" }
                )));
            }
        } else if exact.is_some_and(|value| value < 0) && !dest_signed {
            let src_width = exact.map(signed_value_bits).unwrap_or(64);
            return Err(LowerError::Invalid(format!(
                "assignment of a signed {src_width}-bit value to {what}, declared unsigned \
                 {dest_width} bits. Signedness must match — relabel the value explicitly with \
                 `as uint<{dest_width}>`."
            )));
        }

        let src_width = match (signed, exact) {
            (None, Some(value)) => contextual_value_bits(value, dest_signed),
            _ => width,
        };
        if let Some(src_width) = src_width {
            if src_width > dest_width {
                return Err(LowerError::Invalid(format!(
                    "assignment of a {src_width}-bit value to {what}, declared {dest_width} bits, \
                     narrows. Widths must not shrink implicitly — use `.trunc<{dest_width}>()` to \
                     narrow explicitly, or widen the parameter declaration to {src_width} bits."
                )));
            }
        }
        Ok(Some(expected.clone()))
    }

    fn scalar_value_evidence(&self, value: &Expr) -> Option<ScalarValueEvidence> {
        scalar_value_evidence(value, &|value| self.host_state_scalar_type(value))
    }

    pub(crate) fn lower_checked_ordered_args(
        &mut self,
        exprs: &[&AstExpr],
        expected: &[IrType],
        slots: &[String],
        ports_allowed: bool,
    ) -> Result<Vec<Expr>, LowerError> {
        debug_assert_eq!(exprs.len(), expected.len());
        debug_assert_eq!(exprs.len(), slots.len());
        let effects: Vec<bool> = exprs
            .iter()
            .map(|expr| self.expr_has_effectful_value_prelude(expr))
            .collect();
        let mut lowered = Vec::with_capacity(exprs.len());
        for (index, expr) in exprs.iter().enumerate() {
            let mut value = if ports_allowed {
                self.lower_expr(expr)?
            } else {
                self.lower_expr_no_ports(expr)?
            };
            let hint = match self.check_callable_argument(&value, &expected[index], &slots[index]) {
                Ok(hint) => hint,
                Err(error) => {
                    self.record_error_span(expr.span);
                    return Err(error);
                }
            };
            if effects[index + 1..].iter().any(|effect| *effect) {
                value = self.materialize_ordered_value_as(value, hint);
            }
            lowered.push(value);
        }
        Ok(lowered)
    }

    /// Lower a call to an `extern function name(...) -> ret` (spec §9).
    /// The call remains an `Expr::Call(CallTarget::ExternFn, ...)` whose
    /// raw symbol binds to the user's `extern "C"` definition supplied by
    /// `--ref-src`. Extern functions cannot access DUT ports, so their
    /// arguments lower without port access; supported container arguments
    /// remain valid ABI values.
    pub(crate) fn lower_extern_fn_call(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<Expr, LowerError> {
        // `exprs.rs` dispatches here only after resolving the name in
        // `extern_fns`, so the signature lookup is an invariant.
        let (pnames, ptys, ret) = self.ctx.extern_fns[name].clone();
        super::reject_misplaced_named_args(
            args,
            &pnames,
            &format!("extern fn call `{name}(...)`"),
        )?;
        // Check arity before pairing arguments with declared slots. A surplus
        // argument has no parameter type against which it can be checked.
        if args.len() != pnames.len() {
            return Err(LowerError::Invalid(format!(
                "extern fn `{name}` takes {} argument(s), call passes {}",
                pnames.len(),
                args.len()
            )));
        }
        let arg_exprs: Vec<&crate::ast::Expr> = args
            .iter()
            .map(|a| match a {
                CallArg::Expr(e) | CallArg::Named { value: e, .. } => e,
            })
            .collect();
        let slots: Vec<String> = pnames
            .iter()
            .map(|pname| format!("parameter `{pname}` of extern fn `{name}`"))
            .collect();
        let lowered = self.lower_checked_ordered_args(&arg_exprs, &ptys, &slots, false)?;
        for (i, v) in lowered.iter().enumerate() {
            // Arity is checked above and `pnames`/`ptys` come from the same
            // declaration, so both indexes are total.
            let slot = &slots[i];
            // Validate against the declared type, including supported
            // sequence parameters. Record signatures are rejected while the
            // extern declaration table is built.
            if let Err(error) = self.check_slot_ir(v, &ptys[i], slot) {
                self.record_error_span(arg_exprs[i].span);
                return Err(error);
            }
        }
        Ok(Expr::Call(
            CallTarget::ExternFn {
                name: name.to_string(),
                params: ptys,
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

    /// Lower a call to the canonical callable for a testbench method.
    /// Arguments are evaluated in source order and the current DUT handle is
    /// represented explicitly for module-typed parameters.
    pub(crate) fn lower_tb_method_call(
        &mut self,
        name: &str,
        args: &[CallArg],
    ) -> Result<Expr, LowerError> {
        let Some(&function) = self.ctx.tb_method_functions.get(name) else {
            return Err(LowerError::Invalid(format!(
                "testbench method `{name}` has no canonical callable"
            )));
        };
        let decl = self
            .ctx
            .tb_methods
            .get(name)
            .expect("canonical testbench method has an AST declaration");
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
        let declared = decl
            .params
            .iter()
            .map(|param| param.name.name.clone())
            .collect::<Vec<_>>();
        super::reject_misplaced_named_args(
            args,
            &declared,
            &format!("testbench method call `{name}(...)"),
        )?;
        let arg_exprs = args
            .iter()
            .map(|arg| match arg {
                CallArg::Expr(expr) | CallArg::Named { value: expr, .. } => expr,
            })
            .collect::<Vec<_>>();
        let arg_effects = arg_exprs
            .iter()
            .map(|expr| self.expr_has_effectful_value_prelude(expr))
            .collect::<Vec<_>>();
        let mut lowered = Vec::with_capacity(arg_exprs.len());
        let mut dut_args = Vec::new();
        for (index, (param, expr)) in decl.params.iter().zip(&arg_exprs).enumerate() {
            let param_ty = testbench_method_signature_type(
                name,
                &format!("parameter `{}`", param.name.name),
                param.ty.as_ref(),
                &self.ctx.record_ids,
            )?;
            let module_param = matches!(param.ty, Some(TypeExpr::Named { .. }))
                && matches!(param_ty, IrType::Unknown);
            if module_param {
                let Some(receiver) = dut_ident_receiver(self, expr) else {
                    self.record_error_span(expr.span);
                    return Err(not_implemented(
                        &format!(
                            "testbench method parameter `{}` of module type with a non-DUT argument",
                            param.name.name
                        ),
                        "only the owning testbench DUT handle can enter a module parameter",
                        V1Status::EmitsUncompilable,
                    ));
                };
                dut_args.push(index);
                let receiver_local = self
                    .standalone_dut_aliases
                    .contains(&receiver)
                    .then(|| {
                        self.locals
                            .iter()
                            .position(|local| local.name == receiver)
                            .map(|local| Expr::Local(crate::ir::LocalId(local as u32)))
                    })
                    .flatten();
                lowered.push(receiver_local.unwrap_or(Expr::Literal {
                    value: 0,
                    ty: IrType::Unknown,
                }));
                continue;
            }
            let mut value = if matches!(param_ty, IrType::FixedVec { .. }) {
                let value = self.whole_vec_value_rhs(expr)?.ok_or_else(|| {
                    LowerError::Invalid(format!(
                        "parameter `{}` of testbench method `{name}` requires a whole fixed-vector value",
                        param.name.name
                    ))
                })?;
                if self.ir_whole_vec_type(&value).as_ref() != Some(&param_ty) {
                    return Err(LowerError::Invalid(format!(
                        "parameter `{}` of testbench method `{name}` expects {param_ty:?}, got {:?}",
                        param.name.name,
                        self.ir_whole_vec_type(&value).unwrap_or(IrType::Unknown)
                    )));
                }
                value
            } else {
                self.lower_expr_no_ports(expr)?
            };
            let hint =
                match self.check_param_slot(&value, param, &format!("testbench method `{name}`")) {
                    Ok(hint) => hint,
                    Err(error) => {
                        self.record_error_span(expr.span);
                        return Err(error);
                    }
                };
            if arg_effects[index + 1..].iter().any(|effect| *effect) {
                value = self.materialize_ordered_value_as(value, hint);
            }
            lowered.push(value);
        }
        let dest = if let Some(return_ty) = decl.return_ty.as_ref() {
            let local = self.fresh_temp();
            self.set_local_type(
                local,
                testbench_method_signature_type(
                    name,
                    "return type",
                    Some(return_ty),
                    &self.ctx.record_ids,
                )?,
            );
            Some(local)
        } else {
            None
        };
        self.push(Stmt::TestbenchCall {
            function,
            args: lowered,
            dut_args,
            dest,
        });
        Ok(dest.map(Expr::Local).unwrap_or(Expr::Literal {
            value: 0,
            ty: IrType::Unknown,
        }))
    }

    /// Lower a `return` inside a helper body. Returns false outside a helper
    /// context so the enclosing statement lowering can handle phase returns.
    pub(crate) fn lower_helper_return(
        &mut self,
        value: Option<&AstExpr>,
    ) -> Result<bool, LowerError> {
        if let Some(frame) = self.inline_frames.last() {
            let (dest, cont) = (frame.ret_dest, frame.ret_cont);
            if let Some(e) = value {
                let expected = self.local_type(dest).clone();
                if let Some(port) = self.as_port_ref(e)? {
                    if matches!(
                        expected,
                        IrType::Record(_)
                            | IrType::RecordSeq(_)
                            | IrType::Seq(_)
                            | IrType::FixedVec { .. }
                            | IrType::String
                            | IrType::Component(_)
                    ) {
                        self.record_error_span(e.span);
                        return Err(LowerError::Invalid(format!(
                            "helper return expects {expected:?}, but a DUT port is a scalar value"
                        )));
                    }
                    if let Err(error) =
                        self.check_slot_ir(&Expr::Port(port.clone()), &expected, "helper return")
                    {
                        self.record_error_span(e.span);
                        return Err(error);
                    }
                    self.push(Stmt::DutRead(dest, port));
                } else {
                    let ir = if let IrType::FixedVec { elem, len } = &expected {
                        self.whole_vec_copy_rhs((*len, (**elem).clone()), e)?
                            .ok_or_else(|| {
                            LowerError::Invalid(
                                "fixed-vector testbench method return requires a matching whole-vector value"
                                    .to_string(),
                            )
                        })?
                    } else {
                        self.lower_expr_no_ports(e)?
                    };
                    if matches!(expected, IrType::RecordSeq(_) | IrType::Seq(_)) {
                        let actual = self.expr_type(&ir).unwrap_or(IrType::Unknown);
                        if actual != expected {
                            return Err(LowerError::Invalid(format!(
                                "testbench/helper method return expects {expected:?}, got {actual:?}"
                            )));
                        }
                    }
                    if let Err(error) = self.check_slot_ir(&ir, &expected, "helper return") {
                        self.record_error_span(e.span);
                        return Err(error);
                    }
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
                    self.whole_vec_copy_rhs((*len, (**elem).clone()), e)?
                        .ok_or_else(|| {
                            LowerError::Invalid(
                                "fixed-vector method return requires a matching whole-vector value"
                                    .to_string(),
                            )
                        })?
                } else {
                    self.lower_expr_no_ports(e)?
                };
                if matches!(expected, IrType::RecordSeq(_) | IrType::Seq(_)) {
                    let actual = self.expr_type(&ir).unwrap_or(IrType::Unknown);
                    if actual != expected {
                        return Err(LowerError::Invalid(format!(
                            "method/helper return expects {expected:?}, got {actual:?}"
                        )));
                    }
                }
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
                }
                if let Err(error) = self.check_slot_ir(&ir, &expected, "helper return") {
                    self.record_error_span(e.span);
                    return Err(error);
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
            IrType::FixedVec { .. } | IrType::RecordSeq(_) | IrType::Seq(_) => {
                self.push(Stmt::AggregateInit(dest))
            }
            IrType::String => self.push(Stmt::Assign(dest, Expr::StringLiteral(String::new()))),
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

fn dut_ident_receiver(b: &FuncBuilder<'_>, e: &AstExpr) -> Option<String> {
    match &*e.kind {
        ExprKind::Ident(id) if b.lookup(&id.name).is_none() => b.resolved_dut_name(&id.name),
        ExprKind::Paren(inner) => dut_ident_receiver(b, inner),
        _ => None,
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
        BuiltinTy::String => IrType::String,
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
        if let Some(ty @ IrType::FixedVec { .. }) = super::components::fixed_vec_elem_ir_type(inner)
        {
            return Some(IrType::Seq(Box::new(ty)));
        }
    }
    Some(IrType::Unknown)
}

/// Resolve a `TSeq` used at a callable boundary. `tseq_ir_type` deliberately
/// leaves unsupported element spellings as `Unknown` for non-ABI callers;
/// callable signatures must reject that sentinel instead of emitting it as a
/// scalar value.
pub(crate) fn callable_tseq_ir_type(
    construct: impl FnOnce() -> String,
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<Option<IrType>, LowerError> {
    match tseq_ir_type(ty, record_ids) {
        Some(IrType::Unknown) => Err(unsupported(
            &construct(),
            "callable TSeq values support records, scalars, and scalar-leaf fixed vectors",
        )),
        other => Ok(other),
    }
}

pub(crate) fn is_string_tseq_type(ty: Option<&TypeExpr>) -> bool {
    let Some(TypeExpr::Builtin {
        name: BuiltinTy::TSeq,
        args,
        ..
    }) = ty
    else {
        return false;
    };
    match args.first() {
        Some(TypeArg::Type(TypeExpr::Builtin {
            name: BuiltinTy::String,
            ..
        })) => true,
        Some(TypeArg::Expr(expr)) => {
            matches!(&*expr.kind, ExprKind::Ident(ident) if ident.name == "String")
        }
        _ => false,
    }
}

pub(crate) fn type_contains_string(ty: &TypeExpr) -> bool {
    let args = match ty {
        TypeExpr::Builtin {
            name: BuiltinTy::String,
            ..
        } => return true,
        TypeExpr::Builtin { args, .. } | TypeExpr::Named { generics: args, .. } => args,
    };
    args.iter().any(|arg| match arg {
        TypeArg::Type(ty) => type_contains_string(ty),
        TypeArg::Expr(expr) | TypeArg::Named { value: expr, .. } => {
            matches!(&*expr.kind, ExprKind::Ident(ident) if ident.name == "String")
        }
    })
}

pub(crate) fn is_exact_string_type(ty: Option<&TypeExpr>) -> bool {
    matches!(
        ty,
        Some(TypeExpr::Builtin {
            name: BuiltinTy::String,
            args,
            ..
        }) if args.is_empty()
    )
}

pub(crate) fn is_nested_string_type(ty: Option<&TypeExpr>) -> bool {
    ty.is_some_and(type_contains_string) && !is_exact_string_type(ty)
}

pub(crate) fn nested_string_type_span(ty: Option<&TypeExpr>) -> Option<crate::lexer::Span> {
    let ty = ty.filter(|ty| type_contains_string(ty) && !is_exact_string_type(Some(ty)))?;
    Some(match ty {
        TypeExpr::Named { span, .. } | TypeExpr::Builtin { span, .. } => *span,
    })
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
    if let Some(fixed) =
        ty.and_then(|ty| super::components::fixed_vec_ir_type_with_records(ty, record_ids))
    {
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
    helper: &str,
    what: &str,
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<IrType, LowerError> {
    if let Some(seq) = callable_tseq_ir_type(
        || format!("{what} of helper `{helper}` has an unsupported TSeq element type"),
        ty,
        record_ids,
    )? {
        return Ok(seq);
    }
    if let Some(fixed @ IrType::FixedVec { .. }) =
        ty.and_then(|ty| super::components::fixed_vec_ir_type_with_records(ty, record_ids))
    {
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

fn inlined_helper_signature_type(
    helper: &str,
    what: &str,
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<IrType, LowerError> {
    if let Some(seq) = callable_tseq_ir_type(
        || format!("{what} of helper `{helper}` has an unsupported TSeq element type"),
        ty,
        record_ids,
    )? {
        return Ok(seq);
    }
    if let Some(fixed @ IrType::FixedVec { .. }) =
        ty.and_then(|ty| super::components::fixed_vec_ir_type_with_records(ty, record_ids))
    {
        return Ok(fixed);
    }
    Ok(ir_type_of_with_records(ty, record_ids))
}

fn testbench_method_signature_type(
    method: &str,
    what: &str,
    ty: Option<&TypeExpr>,
    record_ids: &HashMap<String, RecordId>,
) -> Result<IrType, LowerError> {
    if let Some(seq) = callable_tseq_ir_type(
        || format!("{what} of testbench method `{method}` has an unsupported TSeq element type"),
        ty,
        record_ids,
    )? {
        return Ok(seq);
    }
    if let Some(fixed @ IrType::FixedVec { .. }) =
        ty.and_then(|ty| super::components::fixed_vec_ir_type_with_records(ty, record_ids))
    {
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
        ExprKind::Int(_) | ExprKind::String(_) | ExprKind::Bool(_) => {}
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
