//! Native C++ width and signedness inference, validation, and rendering.
//!
//! These helpers keep the legacy emitter aligned with TBIR for width methods,
//! shift signedness, narrowing diagnostics, and wrapping arithmetic.

use super::*;

impl Emitter {
    /// Intrinsic width-method lowering: `.trunc<N>()`, `.zext<N>()`,
    /// `.sext<N>()`, `.resize<N>()`. Ported from arch-com's
    /// `cpp_method_call` (src/sim_codegen/mod.rs:2688). The parser
    /// emits these as `Call { callee: Field{recv, name},
    /// args: [width_const] }`. We dispatch on `name`, evaluate the
    /// width as a compile-time constant, and emit the C++ narrow or
    /// extend. Returns `true` when the call matched and was emitted;
    /// `false` otherwise (caller falls through to normal call
    /// emission).
    ///
    /// Behavior summary (matches arch-com's semantics):
    /// - `.trunc<N>()`  — narrow to N bits: `((uintN_t)((expr) & MASK))`
    ///                    where MASK = (1 << N) - 1. Errors if N >=
    ///                    source width when source width is known
    ///                    (would widen, wrong direction).
    /// - `.zext<N>()`   — zero-extend to N bits: `((uintN_t)(expr))`.
    /// - `.sext<N>()`   — sign-extend to N bits: shift-fill trick.
    /// - `.resize<N>()` — direction-agnostic: narrow if N < source
    ///                    width, zero-extend otherwise.
    pub(super) fn try_emit_width_method(&mut self, callee: &Expr, args: &[CallArg]) -> bool {
        let ExprKind::Field { target, name } = &*callee.kind else {
            return false;
        };
        let kind = match name.name.as_str() {
            "trunc" => "trunc",
            "zext" => "zext",
            "sext" => "sext",
            "resize" => "resize",
            _ => return false,
        };
        let width_expr = match args.first() {
            Some(CallArg::Expr(e)) => e,
            _ => {
                self.errors
                    .push(format!("`.{kind}<N>()` requires a constant width argument",));
                return true;
            }
        };
        let width = match eval_const_width(width_expr) {
            Some(w) => w,
            None => {
                self.errors.push(format!(
                    "`.{kind}<N>()` requires a constant integer width \
                     (saw a non-const expression); span at parse-time \
                     should pin the offending token",
                ));
                return true;
            }
        };
        if width == 0 {
            self.errors.push(format!(
                "`.{kind}<{width}>()`: width must be greater than zero",
            ));
            return true;
        }
        if width > crate::MAX_WIDTH_METHOD_BITS {
            self.errors.push(format!(
                "`.{kind}<{width}>()`: destination width exceeds the {}-bit language limit",
                crate::MAX_WIDTH_METHOD_BITS
            ));
            return true;
        }
        // Best-effort source-width inference for the type-direction check.
        // We can detect width when the receiver is a literal-typed `let`
        // or an explicit `as uint<W>` / `as sint<W>` cast. Otherwise the
        // width is unknown and we skip the type-direction check.
        let source_width = self.infer_expr_width_best_effort(target);
        if let Some(sw) = source_width {
            match kind {
                "trunc" if width >= sw => {
                    self.errors.push(format!(
                        "`.trunc<{width}>()` on a {sw}-bit value: width \
                         must be strictly less than the source width \
                         (otherwise it's a no-op or wrong-direction). \
                         Use `.zext<{width}>()` to widen, or remove the \
                         cast if you meant a no-op.",
                    ));
                    return true;
                }
                "zext" | "sext" if width < sw => {
                    self.errors.push(format!(
                        "`.{kind}<{width}>()` on a {sw}-bit value: width \
                         must be ≥ the source width (otherwise it \
                         narrows, wrong direction). Use `.trunc<{width}>()` \
                         to narrow.",
                    ));
                    return true;
                }
                _ => {}
            }
        }
        // The direction checks above want the raw inferred width; a zero
        // width is not usable emission metadata (it would reach the sext
        // shift-fill as `64 - 0`, UB), so drop to the unknown-source
        // shapes. Matches TB-IR's `lower_width_method`.
        let source_width = source_width.filter(|w| *w > 0);
        // Emit. Pattern follows arch-com's cast_to_bits / shift-fill
        // for sext. Sub-64 narrowing (`trunc`, `resize` down) narrows to
        // `uint64_t` *before* the mask: a `HarcWide<N>` receiver converts
        // implicitly to both `uint64_t` and `_harc_u128`, so masking it
        // directly is an ambiguous `operator&`. Narrowing first is
        // value-identical for the `uint64_t` / `_harc_u128` receivers,
        // since `width < 64` keeps the kept bits inside the low word.
        let c_unsigned = cpp_uint_for_width(Some(width));
        // `sext` with nothing to extend (source already ≥ the destination,
        // or unknown) is still a *signed* relabel: the result feeds signed
        // comparison, division, and shifts. Casting it through `uint64_t`
        // silently made `s.sext<64>() > 0` true for a negative `sint<64>`
        // — TB-IR emits `((int64_t)(...))` here and the two backends
        // returned opposite verdicts for the same source.
        let c_sext_plain = if width <= 64 {
            "int64_t".to_string()
        } else {
            c_unsigned.clone()
        };
        // Narrow to `uint64_t` before that signed relabel: a `HarcWide<N>`
        // receiver converts implicitly to both `uint64_t` and `_harc_u128`,
        // so a bare `(int64_t)` on one is an ambiguous conversion. The
        // scalar receivers reinterpret the same low 64 bits either way.
        let (sext_narrow, sext_narrow_close) = if width <= 64 {
            ("(uint64_t)(", ")")
        } else {
            ("", "")
        };
        match kind {
            "trunc" => {
                if width > 128 {
                    let words = width.div_ceil(32);
                    write!(self.out, "harc_rt::harc_wide_trunc<{words}>(").ok();
                    self.emit_expr(target);
                    write!(self.out, ", {width})").ok();
                } else if width > 64 {
                    write!(self.out, "harc_rt::harc_trunc_u128((_harc_u128)(").ok();
                    self.emit_expr(target);
                    write!(self.out, "), {width})").ok();
                } else if width == 64 {
                    write!(self.out, "(({c_unsigned})(").ok();
                    self.emit_expr(target);
                    write!(self.out, "))").ok();
                } else {
                    let mask = (1u64 << width) - 1;
                    write!(self.out, "(({c_unsigned})(((uint64_t)(").ok();
                    self.emit_expr(target);
                    write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                }
            }
            "zext" => {
                // Mask to the source width before widening so any high bits
                // in the underlying int64_t storage (from a prior no-op cast)
                // are dropped. When source width is unknown we conservatively
                // assume the receiver already fits in N bits — same shape
                // as arch-com.
                if width > 128 {
                    let words = width.div_ceil(32);
                    write!(self.out, "harc_rt::harc_wide_zext<{words}>(").ok();
                    self.emit_expr(target);
                    if let Some(sw) = source_width {
                        write!(self.out, ", {sw}").ok();
                    }
                    write!(self.out, ")").ok();
                } else {
                    if let Some(sw) = source_width {
                        // A scalar carrier can be wider than the logical
                        // source bit-vector (for example sint<8>(-1) is
                        // held as int64_t all-ones). Normalize the source
                        // bits before widening. harc_trunc_u128 handles
                        // widths through 128 without shift-by-64/128 UB.
                        write!(
                            self.out,
                            "(({c_unsigned})(harc_rt::harc_trunc_u128((_harc_u128)("
                        )
                        .ok();
                        self.emit_expr(target);
                        write!(self.out, "), {sw})))").ok();
                    } else {
                        write!(self.out, "(({c_unsigned})(").ok();
                        self.emit_expr(target);
                        write!(self.out, "))").ok();
                    }
                }
            }
            "sext" => {
                // Sign-extend from source width to dest width. Strategy:
                // shift source-width MSB into bit 63 of an int64_t, then
                // arithmetic-shift right back (the standard idiom).
                // The result is masked to the dest width via the C++
                // cast `((cpp_uint_for_width(width))<expr>)`. When source
                // width is unknown, fall back to a plain widening cast
                // — the receiver almost certainly already has the right
                // sign-bit pattern in its underlying storage.
                if let Some(sw) = source_width {
                    if sw < width {
                        if width > 64 {
                            if width > 128 {
                                let words = width.div_ceil(32);
                                write!(self.out, "harc_rt::harc_wide_sext<{words}>(").ok();
                                self.emit_expr(target);
                                write!(self.out, ", {sw}, {width})").ok();
                            } else {
                                write!(self.out, "harc_rt::harc_sext_u128((_harc_u128)(").ok();
                                self.emit_expr(target);
                                write!(self.out, "), {sw}, {width})").ok();
                            }
                        } else {
                            let shift = 64 - sw;
                            if width == 64 {
                                // Want full 64-bit signed-extended view.
                                // `int64_t`, not `uint64_t`: this binds to
                                // `auto` at the use site, and deducing
                                // unsigned here made `p[7:0].sext<64>() > 0`
                                // true under v1 and false under TB-IR, whose
                                // local for a sext is `int64_t`.
                                write!(self.out, "((int64_t)(((int64_t)((uint64_t)(").ok();
                                self.emit_expr(target);
                                write!(self.out, ") << {shift})) >> {shift}))").ok();
                            } else {
                                let mask = (1u64 << width) - 1;
                                write!(self.out, "(({c_unsigned})(((int64_t)((uint64_t)(").ok();
                                self.emit_expr(target);
                                write!(self.out, ") << {shift})) >> {shift}) & 0x{mask:X}ULL)")
                                    .ok();
                            }
                        }
                    } else {
                        if width > 128 {
                            let words = width.div_ceil(32);
                            write!(self.out, "harc_rt::harc_wide_trunc<{words}>(").ok();
                            self.emit_expr(target);
                            write!(self.out, ", {sw})").ok();
                        } else {
                            write!(self.out, "(({c_sext_plain})({sext_narrow}").ok();
                            self.emit_expr(target);
                            write!(self.out, "){sext_narrow_close})").ok();
                        }
                    }
                } else {
                    if width > 128 {
                        let words = width.div_ceil(32);
                        write!(self.out, "harc_rt::harc_wide_zext<{words}>(").ok();
                        self.emit_expr(target);
                        write!(self.out, ")").ok();
                    } else {
                        write!(self.out, "(({c_sext_plain})({sext_narrow}").ok();
                        self.emit_expr(target);
                        write!(self.out, "){sext_narrow_close})").ok();
                    }
                }
            }
            "resize" => {
                // Direction-agnostic: narrow with mask when narrowing,
                // plain cast otherwise.
                if let Some(sw) = source_width {
                    if width < sw {
                        // Narrowing — mask + cast.
                        if width > 128 {
                            let words = width.div_ceil(32);
                            write!(self.out, "harc_rt::harc_wide_trunc<{words}>(").ok();
                            self.emit_expr(target);
                            write!(self.out, ", {width})").ok();
                        } else if width > 64 {
                            write!(self.out, "harc_rt::harc_trunc_u128((_harc_u128)(").ok();
                            self.emit_expr(target);
                            write!(self.out, "), {width})").ok();
                        } else if width == 64 {
                            write!(self.out, "(({c_unsigned})(").ok();
                            self.emit_expr(target);
                            write!(self.out, "))").ok();
                        } else {
                            let mask = (1u64 << width) - 1;
                            write!(self.out, "(({c_unsigned})(((uint64_t)(").ok();
                            self.emit_expr(target);
                            write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                        }
                    } else {
                        // Widening or same width — plain cast.
                        if width > 128 {
                            let words = width.div_ceil(32);
                            write!(self.out, "harc_rt::harc_wide_zext<{words}>(").ok();
                            self.emit_expr(target);
                            write!(self.out, ", {sw})").ok();
                        } else {
                            write!(self.out, "(({c_unsigned})(").ok();
                            self.emit_expr(target);
                            write!(self.out, "))").ok();
                        }
                    }
                } else {
                    // Unknown source width — default to mask-narrow,
                    // since `.resize<N>()` with `N <= 64` always wants
                    // a value bounded to N bits regardless of source.
                    if width > 128 {
                        let words = width.div_ceil(32);
                        write!(self.out, "harc_rt::harc_wide_trunc<{words}>(").ok();
                        self.emit_expr(target);
                        write!(self.out, ", {width})").ok();
                    } else if width > 64 {
                        write!(self.out, "harc_rt::harc_trunc_u128((_harc_u128)(").ok();
                        self.emit_expr(target);
                        write!(self.out, "), {width})").ok();
                    } else if width == 64 {
                        write!(self.out, "(({c_unsigned})(").ok();
                        self.emit_expr(target);
                        write!(self.out, "))").ok();
                    } else {
                        let mask = (1u64 << width) - 1;
                        write!(self.out, "(({c_unsigned})(((uint64_t)(").ok();
                        self.emit_expr(target);
                        write!(self.out, ") & 0x{mask:X}ULL)))").ok();
                    }
                }
            }
            _ => unreachable!(),
        }
        true
    }

    /// Best-effort source-width inference for the width-method type
    /// checks. Returns the width in bits when it can be statically
    /// determined; `None` otherwise (callers skip the type-direction
    /// check in that case). Recognized shapes:
    ///   - Parenthesized expression → recurse into inner.
    ///   - `<expr> as uint<W>` / `<expr> as sint<W>` → W.
    ///   - `<expr>[hi:lo]` → `hi - lo + 1` for constant bounds.
    ///   - `<expr>.trunc<W>()` / `.zext<W>()` / `.sext<W>()` / `.resize<W>()`
    ///     → W (the prior width-method's target width).
    ///   - Bare integer literal → minimum unsigned bit-width of the
    ///     literal value (cheap heuristic: `64 - leading_zeros(v)` for
    ///     positive values; `None` for negatives).
    ///
    /// The result can be `Some(0)` for a `uint<0>` receiver. Callers
    /// emitting a cast shape must filter that out (it would reach the
    /// sext shift-fill as `64 - 0`, UB); the direction checks want the
    /// raw value.
    ///
    /// Mirrors TB-IR's `infer_expr_width` on TB-IR's whole reachable
    /// domain, but NOT identically for casts: TB-IR caps a cast relabel
    /// at 128 bits, and this returns the raw declared width above that.
    /// The two agree in practice only because TB-IR rejects every cast
    /// outside 1..=128 before inference sees it. Do not "re-unify" them
    /// by delegating this arm — that is what turned
    /// `(big as uint<200>).sext<300>()` into a zero-extension.
    pub(super) fn infer_expr_width_best_effort(&self, e: &Expr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.infer_expr_width_best_effort(inner),
            // RAW width, deliberately not `cast_relabel_width`: that
            // helper caps at 128 and rejects 0, which is right for a wrap
            // operand (see `wrap_operand_width`) but wrong here. The
            // direction checks and the `sext` emission shape need the
            // declared width even above 128 — losing it turned
            // `(big as uint<200>).sext<300>()` into a zero-extension,
            // silently, and dropped the wrong-direction rejection.
            //
            // The arms below still mirror `cast_relabel_width`'s *shape*
            // rules: the capitalised ARCH spellings count, an absent
            // width argument is 64 bits, and a width argument that is not
            // an integer literal is unknown rather than 64.
            ExprKind::Cast { ty, .. } => match ty {
                TypeExpr::Builtin {
                    name:
                        BuiltinTy::UInt
                        | BuiltinTy::UIntCap
                        | BuiltinTy::SInt
                        | BuiltinTy::SIntCap
                        | BuiltinTy::Bits,
                    args,
                    ..
                } => match args.first() {
                    None => Some(64),
                    Some(TypeArg::Expr(e)) => match &*e.kind {
                        ExprKind::Int(s) => s.replace('_', "").parse().ok(),
                        _ => None,
                    },
                    Some(_) => None,
                },
                _ => None,
            },
            ExprKind::BitSlice { hi, lo, .. } => {
                let hi = eval_const_width(hi)?;
                let lo = eval_const_width(lo)?;
                (hi >= lo).then_some(hi - lo + 1)
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Field { name, .. } = &*callee.kind {
                    if Emitter::is_width_method_name(&name.name) {
                        if let Some(CallArg::Expr(w)) = args.first() {
                            return eval_const_width(w);
                        }
                    }
                }
                None
            }
            ExprKind::Int(s) => {
                // Parse the literal text to a u64 and use bit-width.
                let stripped = s.replace('_', "");
                let v: Option<u64> = if let Some(rest) = stripped
                    .strip_prefix("0x")
                    .or_else(|| stripped.strip_prefix("0X"))
                {
                    u64::from_str_radix(rest, 16).ok()
                } else if let Some(rest) = stripped
                    .strip_prefix("0b")
                    .or_else(|| stripped.strip_prefix("0B"))
                {
                    u64::from_str_radix(rest, 2).ok()
                } else {
                    stripped.parse::<u64>().ok()
                };
                v.map(|v| if v == 0 { 1 } else { 64 - v.leading_zeros() })
            }
            ExprKind::Ident(id) => self.let_widths.get(&id.name).map(|lw| lw.bits),
            ExprKind::Field { .. } | ExprKind::Index { .. } => self
                .resolve_value_field_type(e)
                .and_then(|ty| declared_type_bit_width(&ty)),
            _ => None,
        }
    }

    /// Conservative width propagation specifically for shift receivers.
    /// A wide value nested in arithmetic/ternary syntax must not fall back
    /// to C++'s 64-bit shift path. A literal `&` mask is the one narrowing
    /// operation: it bounds the result to the smaller known width.
    pub(super) fn infer_shift_width_best_effort(&self, e: &Expr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => {
                self.infer_shift_width_best_effort(inner)
            }
            ExprKind::Binary {
                op: BinaryOp::BitAnd,
                lhs,
                rhs,
            } => {
                let bound = |operand: &Expr| match &*operand.kind {
                    ExprKind::Int(text) => literal_operand_bit_width(text),
                    _ => self.infer_shift_width_best_effort(operand),
                };
                match (bound(lhs), bound(rhs)) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (width, None) | (None, width) => width,
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => self
                .infer_shift_width_best_effort(lhs)
                .max(self.infer_shift_width_best_effort(rhs)),
            ExprKind::Ternary {
                then_branch,
                else_branch,
                ..
            } => self
                .infer_shift_width_best_effort(then_branch)
                .max(self.infer_shift_width_best_effort(else_branch)),
            ExprKind::Int(text) => literal_operand_bit_width(text),
            _ => self.infer_expr_width_best_effort(e),
        }
    }

    /// Resolve a value-producing field chain to its declared source type.
    /// The root is a let/testbench/component binding from `let_types`; each
    /// following segment may walk a record, component, or transactor field.
    /// This is the v1 counterpart of TB-IR's schema-based record path lookup.
    fn resolve_value_field_type(&self, e: &Expr) -> Option<TypeExpr> {
        match &*e.kind {
            ExprKind::Ident(id) => {
                let ty_name = self.let_types.get(&id.name)?.clone();
                Some(TypeExpr::Named {
                    name: Path {
                        segments: vec![Ident {
                            name: ty_name,
                            span: id.span,
                        }],
                        span: id.span,
                    },
                    generics: Vec::new(),
                    mode: None,
                    span: id.span,
                })
            }
            ExprKind::Index { target, .. } => {
                let container = self.resolve_value_field_type(target)?;
                fixed_vec_type_args(&container).map(|(elem, _)| elem.clone())
            }
            ExprKind::Field { target, name } => {
                let owner = self.resolve_value_field_type(target)?;
                let owner_name = type_simple_name(Some(&owner))?;
                if let Some(fields) = self.record_fields.get(owner_name) {
                    fields
                        .iter()
                        .find(|f| f.name.name == name.name)
                        .map(|f| f.ty.clone())
                } else if let Some(comp) = self.components.get(owner_name) {
                    component_field_type(&comp.items, &name.name)
                } else if let Some(transactor) = self.transactors.get(owner_name) {
                    component_field_type(
                        &synth_component_from_transactor(transactor, /*include_active*/ true).items,
                        &name.name,
                    )
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Best-effort signedness for ordinary expression emission. This mirrors
    /// the TB-IR shift rule closely enough that a wide signed record field and
    /// simple expressions derived from it select arithmetic right shift.
    pub(super) fn infer_expr_signed_best_effort(&self, e: &Expr) -> bool {
        match &*e.kind {
            ExprKind::Paren(inner) | ExprKind::Unary { expr: inner, .. } => {
                self.infer_expr_signed_best_effort(inner)
            }
            ExprKind::Cast { ty, .. } => matches!(
                ty,
                TypeExpr::Builtin {
                    name: BuiltinTy::SInt | BuiltinTy::SIntCap | BuiltinTy::Int,
                    ..
                }
            ),
            ExprKind::Ident(id) => self.let_widths.get(&id.name).is_some_and(|lw| lw.signed),
            ExprKind::Field { .. } | ExprKind::Index { .. } => {
                self.resolve_value_field_type(e).is_some_and(|ty| {
                    matches!(
                        ty,
                        TypeExpr::Builtin {
                            name: BuiltinTy::SInt | BuiltinTy::SIntCap | BuiltinTy::Int,
                            ..
                        }
                    )
                })
            }
            ExprKind::Int(_) => true,
            ExprKind::Ternary {
                then_branch,
                else_branch,
                ..
            } => {
                self.infer_expr_signed_best_effort(then_branch)
                    && self.infer_expr_signed_best_effort(else_branch)
            }
            ExprKind::Binary { op, lhs, rhs } => match op {
                BinaryOp::Shl | BinaryOp::Shr => self.infer_expr_signed_best_effort(lhs),
                _ => {
                    self.infer_expr_signed_best_effort(lhs)
                        && self.infer_expr_signed_best_effort(rhs)
                }
            },
            ExprKind::Call { callee, .. } => matches!(
                &*callee.kind,
                ExprKind::Field { name, .. } if name.name == "sext"
            ),
            _ => false,
        }
    }

    fn is_width_method_name(name: &str) -> bool {
        matches!(name, "trunc" | "zext" | "sext" | "resize")
    }

    /// A wrap's residue is unsigned (spec §2.4), so binding one to a
    /// signed local is a signedness mismatch — TB-IR rejects it, v1
    /// accepted it silently. Unwraps parentheses: `(a +% b)` is the same
    /// wrap, and not seeing through them left the divergence reachable.
    pub(super) fn check_signed_wrap_destination(&mut self, name: &str, dw: u32, value: &Expr) {
        fn unwrap_parens(v: &Expr) -> &Expr {
            match &*v.kind {
                ExprKind::Paren(inner) => unwrap_parens(inner),
                _ => v,
            }
        }
        fn is_wrap(v: &Expr) -> bool {
            match &*v.kind {
                ExprKind::Paren(inner) => is_wrap(inner),
                ExprKind::Binary { op, .. } => matches!(
                    op,
                    BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                ),
                _ => false,
            }
        }
        if is_wrap(value) {
            // Report the VALUE's width, as TB-IR does — the destination's
            // width is a different number whenever the two disagree.
            // If the wrap itself is invalid (unknown or >64-bit operands)
            // its own error already fires and says more; adding a
            // signedness message with a fabricated width is just noise.
            let Some(aw) = (match &*unwrap_parens(value).kind {
                ExprKind::Binary { op, lhs, rhs } => self.wrap_result_width(*op, lhs, rhs).ok(),
                _ => None,
            }) else {
                return;
            };
            self.errors.push(format!(
                "assignment of an unsigned {aw}-bit value to `{name}`, declared \
                 signed {dw} bits. Signedness must match — relabel the value \
                 explicitly with `as sint<{dw}>`."
            ));
        }
    }

    /// Reject a narrowing scalar assignment into a declared-width local.
    /// TB-IR's `check_scalar_assign_width` is the counterpart; v1 used to
    /// emit `HarcWide<7> b = a;` for a 256-bit `a` (which does not
    /// compile) and silently accept the ≤64-bit cases.
    pub(super) fn check_scalar_assign_width(&mut self, name: &str, value: &Expr) {
        let Some(dw) = self.let_widths.get(name).map(|lw| lw.bits) else {
            return;
        };
        if dw == 0 {
            return;
        }
        let Some(aw) = narrowing_source_width(self, value) else {
            return;
        };
        if aw > dw {
            self.errors.push(format!(
                "assignment of a {aw}-bit value to `{name}`, declared {dw} bits, \
                 narrows. Widths must not shrink implicitly — use `.trunc<{dw}>()` \
                 to narrow explicitly, or widen the declaration to {aw} bits."
            ));
        }
    }

    /// Mask width for a wrapping operator: `max(W(lhs), W(rhs))`, the
    /// wider operand's width with no widening (harc#473). Mirrors TB-IR's
    /// `wrap_to_operand_width` / `infer_wrap_operand_width`, including its
    /// two rejections, so both backends accept and reject the same
    /// programs. `Err` carries the user-facing diagnostic.
    pub(super) fn wrap_result_width(
        &self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Result<u32, String> {
        let sym = match op {
            BinaryOp::SubWrap => "-%",
            BinaryOp::MulWrap => "*%",
            _ => "+%",
        };
        let wl = self.wrap_operand_width(lhs);
        let wr = self.wrap_operand_width(rhs);
        let (Some(wl), Some(wr)) = (wl, wr) else {
            return Err(format!(
                "wrapping operator `{sym}` needs both operands to have a statically \
                 known bit-width so the wrap width `max(W(lhs), W(rhs))` is defined \
                 (left is {}, right is {}). Give the operand(s) a scalar type \
                 (`let x : uint<N>`), a cast (`x as uint<N>`), or a width method.",
                if wl.is_some() { "known" } else { "unknown" },
                if wr.is_some() { "known" } else { "unknown" },
            ));
        };
        let width = wl.max(wr);
        if width > 64 {
            return Err(format!(
                "wrapping operator `{sym}` at width {width} (> 64 bits) is not \
                 supported: wrapping arithmetic is lowered for operand widths up \
                 to 64 bits; wider datapaths need the `HarcWide<N>` model, which \
                 is not wired through the wrapping mask yet"
            ));
        }
        Ok(width)
    }

    /// Operand bit-width for a wrapping-op mask: the receiver-width
    /// inference, plus composition through a nested wrap so a chain
    /// (`(a +% b) *% c`) masks at each step's own operand width.
    fn wrap_operand_width(&self, e: &Expr) -> Option<u32> {
        match &*e.kind {
            ExprKind::Paren(inner) => self.wrap_operand_width(inner),
            ExprKind::Binary {
                op: BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap,
                lhs,
                rhs,
            } => {
                let l = self.wrap_operand_width(lhs)?;
                let r = self.wrap_operand_width(rhs)?;
                Some(l.max(r))
            }
            // A cast operand uses TB-IR's own `cast_relabel_width`, which
            // caps at 128 and rejects 0 — TB-IR's wrap inference routes
            // through the same helper, so sharing it keeps the two
            // backends' accepted operand sets aligned by construction.
            // This is the one consumer that wants the capped rule; the
            // direction checks and emission shapes read the raw declared
            // width from `infer_expr_width_best_effort` instead.
            ExprKind::Cast { ty, .. } => crate::ir::lower::cast_relabel_width(ty),
            // `dut.<probe>` carries the probe's declared width — the same
            // width TB-IR reads off `PortRef::width`. A plain top-level
            // port is width-erased and still reports `None` in both.
            //
            // The base MUST be checked, not just the field name: this map
            // is keyed by bare probe name, so an unguarded lookup masked a
            // same-named record field (`t.count +% 10` on a `uint<32>`
            // field, with a `probe count : uint<8>` in scope) at the
            // probe's width — a silently wrong value. Mirrors the
            // read-only-probe guard on the write path.
            ExprKind::Field { target, name } => match &*target.kind {
                ExprKind::Ident(id) if self.is_active_probe_receiver(&id.name) => {
                    self.probe_widths.get(&name.name).copied()
                }
                _ => None,
            },
            _ => self.infer_expr_width_best_effort(e),
        }
    }

    /// `((uint64_t)(((uint64_t)((lhs OP rhs)) & 0xMASK)))` — the same
    /// narrow-to-`width` shape the `.trunc<N>()` intrinsic emits, so the
    /// two spellings of a wrap are byte-identical, and identical to what
    /// TB-IR emits for the `WidthCast::Trunc` its lowering inserts.
    pub(super) fn emit_wrapping_binary(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        width: u32,
    ) {
        let c_unsigned = cpp_uint_for_width(Some(width));
        let s = c_binary_op(op);
        if width == 64 {
            write!(self.out, "(({c_unsigned})(").ok();
        } else {
            write!(self.out, "(({c_unsigned})(((uint64_t)(").ok();
        }
        write!(self.out, "(").ok();
        self.emit_expr(lhs);
        write!(self.out, " {s} ").ok();
        self.emit_expr(rhs);
        write!(self.out, ")").ok();
        if width == 64 {
            write!(self.out, "))").ok();
        } else {
            let mask = (1u64 << width) - 1;
            write!(self.out, ") & 0x{mask:X}ULL)))").ok();
        }
    }
}
