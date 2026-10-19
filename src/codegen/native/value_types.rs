//! Native C++ value-type selection and destination normalization.
//!
//! These helpers provide one shared answer for TLM, component, scoreboard,
//! record bridge, expression, and local declaration emission.

use super::*;

impl Emitter {
    /// Component-aware C++ type rendering for a HARC `TypeExpr`.
    /// Wraps `c_type_for` with awareness of declared transactions /
    /// enums / sub-components: those Named types lower to their bare
    /// struct/enum name rather than the default `V<Name>*` pointer.
    /// Used by hookable-method param types and other component-body
    /// contexts where DUT vs. value-type ambiguity would otherwise
    /// produce wrong code.
    pub(super) fn c_type_for_param(&self, t: &TypeExpr) -> String {
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last() {
                let n = &last.name;
                if self.transactions.contains(n)
                    || self.structs.contains(n)
                    || self.enums.contains_key(n)
                    || self.components.contains_key(n)
                    || self.scoreboards.contains(n)
                    || self.transactors.contains_key(n)
                    || self.covergroups.contains_key(n)
                {
                    return n.clone();
                }
            }
        }
        c_type_for(t)
    }

    pub(super) fn c_type_for_value(&self, t: &TypeExpr) -> String {
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last() {
                let n = &last.name;
                if self.is_record_type(n)
                    || self.enums.contains_key(n)
                    || self.components.contains_key(n)
                    || self.scoreboards.contains(n)
                    || self.transactors.contains_key(n)
                    || self.covergroups.contains_key(n)
                {
                    return n.clone();
                }
            }
        }
        c_type_for(t)
    }

    /// True if a Named-type field on a component refers to a DUT module
    /// (i.e. nothing the codegen knows about other than that it's a
    /// Verilator-compiled module type). Sub-components / scoreboards /
    /// transactors / covergroups are excluded — they're held by-value.
    pub(super) fn is_dut_pointer_field_type(&self, t: &TypeExpr) -> bool {
        if let Some(name) = type_simple_name(Some(t)) {
            // Every known value-shaped Named type is excluded; only an
            // unrecognized Named type (the DUT module) is pointer-shaped.
            // `structs` matters: a struct-typed method param is a by-value
            // record exactly like a transaction-typed one — classifying it
            // as a pointer emitted `r->field` on a by-value `Result r`.
            return !self.transactions.contains(name)
                && !self.structs.contains(name)
                && !self.scoreboards.contains(name)
                && !self.covergroups.contains_key(name)
                && !self.components.contains_key(name)
                && !self.transactors.contains_key(name)
                && !self.enums.contains_key(name);
        }
        false
    }

    /// Field-type lowering for `driver`/`agent`/`env`/`sequencer` bodies.
    pub(super) fn component_field_c_type(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Builtin {
                name: BuiltinTy::Event,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("std::vector<std::function<void({inner})>>")
            }
            TypeExpr::Builtin {
                name: BuiltinTy::Queue,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("harc_rt::HarcQueue<{inner}>")
            }
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last().map(|s| s.name.as_str()).unwrap_or("");
                if self.is_dut_pointer_field_type(t) {
                    // DUT module → Verilator pointer.
                    format!("V{last}*")
                } else {
                    // Sub-component / scoreboard / transaction → by-value.
                    last.to_string()
                }
            }
            _ => self.record_field_c_type(t),
        }
    }

    pub(super) fn scoreboard_field_c_type(&self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Builtin {
                name: BuiltinTy::Queue,
                args,
                ..
            } => {
                let inner = self.payload_type_for_arg(args.first());
                format!("harc_rt::HarcQueue<{inner}>")
            }
            _ => self.record_field_c_type(t),
        }
    }

    /// Resolve a `<T>` payload arg (the inside of `event<T>`,
    /// `queue<T>`, etc.) to a C++ type name. User-declared
    /// transactions and enums get their bare name (so payloads
    /// round-trip through C++ as the struct/enum directly);
    /// everything else falls back to `txn_field_c_type` which
    /// widens narrow ints into `int64_t`/`uint64_t`.
    pub(super) fn payload_type_for_arg(&self, arg: Option<&TypeArg>) -> String {
        match arg {
            Some(TypeArg::Type(ty)) => {
                if let Some(name) = type_simple_name(Some(ty)) {
                    if self.is_record_type(name) || self.enums.contains_key(name) {
                        return name.to_string();
                    }
                }
                self.record_field_c_type(ty)
            }
            Some(TypeArg::Expr(e)) => {
                // `event<RegOp>` parses as TypeArg::Expr(Ident) at the
                // type-arg layer — the parser doesn't always know the
                // arg's a type until the user actually references it.
                if let ExprKind::Ident(id) = &*e.kind {
                    if self.is_record_type(&id.name) || self.enums.contains_key(&id.name) {
                        return id.name.clone();
                    }
                }
                "uint64_t".into()
            }
            _ => "uint64_t".into(),
        }
    }

    pub(super) fn emit_value_for_destination(&mut self, ty: &TypeExpr, value: &Expr) {
        let dest_signed = matches!(
            ty,
            TypeExpr::Builtin {
                name: BuiltinTy::SInt | BuiltinTy::SIntCap | BuiltinTy::Int,
                ..
            }
        );
        let dest_width = self
            .packed_width(ty)
            .and_then(|width| u32::try_from(width).ok());
        let source_width = self.infer_shift_width_best_effort(value);
        if let (true, Some(dest_width), Some(source_width)) = (
            dest_signed && self.infer_expr_signed_best_effort(value),
            dest_width,
            source_width,
        ) {
            if source_width < dest_width {
                if dest_width > 128 {
                    write!(
                        self.out,
                        "harc_rt::harc_wide_sext<{}>(",
                        dest_width.div_ceil(32)
                    )
                    .ok();
                    self.emit_expr(value);
                    write!(self.out, ", {source_width}, {dest_width})").ok();
                } else {
                    write!(self.out, "harc_rt::harc_sext_u128((_harc_u128)(").ok();
                    self.emit_expr(value);
                    write!(self.out, "), {source_width}, {dest_width})").ok();
                }
                return;
            }
        }
        self.emit_expr(value);
    }

    pub(super) fn local_value_c_type(&self, t: &TypeExpr) -> String {
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last().map(|s| s.name.as_str()) {
                if self.is_record_type(last) {
                    return last.to_string();
                }
                // An enum is a value type, not a DUT module — falling
                // through to `c_type_for` below would declare
                // `VColor* c = RED;` for `let c : Color = RED`, and
                // `VColor` is a Verilator handle for a module that does
                // not exist. Route it through the SAME mapping an
                // enum-typed record FIELD gets (`int64_t`), so a local
                // and a field of one enum type are the same C++ type
                // and `p.c = d` / `d = p.c` both compile. TB-IR agrees:
                // it lowers an enum-typed local to `int64_t` too.
                if self.enums.contains_key(last) {
                    return self.record_field_c_type(t);
                }
            }
            return c_type_for(t);
        }
        self.record_field_c_type(t)
    }
}
