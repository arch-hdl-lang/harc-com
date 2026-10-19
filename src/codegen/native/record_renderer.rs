//! Native record declaration and packed/structured bridge rendering.
//!
//! Randomization rendering lives in the sibling module; this module owns the
//! record value shape and its generated pack, unpack, and drive helpers.

use super::*;

impl Emitter {
    fn emit_pack_bits(&mut self, ty: &TypeExpr, value_expr: &str, offset: usize, depth: usize) {
        if let Some((elem, len)) = fixed_vec_type_args(ty) {
            let Some(elem_w) = self.packed_width(elem) else {
                return;
            };
            for i in 0..len {
                self.emit_pack_bits(
                    elem,
                    &format!("{value_expr}[{i}]"),
                    offset + i * elem_w,
                    depth,
                );
            }
            return;
        }
        if let Some(name) = self.packed_record_name(ty) {
            let Some(width) = self.packed_width(ty) else {
                return;
            };
            self.pad(depth);
            writeln!(
                self.out,
                "harc_rt::harc_wide_write_bits(_packed, {offset}, {width}, harc_pack_{name}({value_expr}));"
            )
            .ok();
            return;
        }
        let Some(width) = self.packed_width(ty) else {
            return;
        };
        self.pad(depth);
        writeln!(
            self.out,
            "harc_rt::harc_wide_write_bits(_packed, {offset}, {width}, {value_expr});"
        )
        .ok();
    }

    fn emit_unpack_bits(&mut self, ty: &TypeExpr, target_expr: &str, offset: usize, depth: usize) {
        if let Some((elem, len)) = fixed_vec_type_args(ty) {
            let Some(elem_w) = self.packed_width(elem) else {
                return;
            };
            for i in 0..len {
                self.emit_unpack_bits(
                    elem,
                    &format!("{target_expr}[{i}]"),
                    offset + i * elem_w,
                    depth,
                );
            }
            return;
        }
        if let Some(name) = self.packed_record_name(ty) {
            let Some(width) = self.packed_width(ty) else {
                return;
            };
            let words = width.div_ceil(32).max(1);
            self.pad(depth);
            writeln!(
                self.out,
                "{target_expr} = harc_unpack_{name}(harc_rt::harc_wide_extract_bits<{words}>(_packed, {offset}, {width}));"
            )
            .ok();
            return;
        }
        let Some(width) = self.packed_width(ty) else {
            return;
        };
        let cty = self.record_field_c_type(ty);
        let rhs = if width <= 64 {
            format!(
                "({cty})harc_rt::harc_bits(_packed, {}, {offset})",
                offset + width - 1
            )
        } else if width <= 128 {
            format!(
                "static_cast<_harc_u128>(harc_rt::harc_wide_extract_bits<4>(_packed, {offset}, {width}))"
            )
        } else {
            let words = width.div_ceil(32);
            format!("harc_rt::harc_wide_extract_bits<{words}>(_packed, {offset}, {width})")
        };
        self.pad(depth);
        writeln!(self.out, "{target_expr} = {rhs};").ok();
    }

    fn emit_structured_unpack_field(
        &mut self,
        ty: &TypeExpr,
        target_expr: &str,
        raw_expr: &str,
        depth: usize,
    ) {
        if let Some((elem, len)) = fixed_vec_type_args(ty) {
            for i in 0..len {
                self.emit_structured_unpack_field(
                    elem,
                    &format!("{target_expr}[{i}]"),
                    &format!("{raw_expr}[{i}]"),
                    depth,
                );
            }
            return;
        }
        if let Some(name) = self.packed_record_name(ty) {
            self.pad(depth);
            writeln!(self.out, "{target_expr} = harc_unpack_{name}({raw_expr});").ok();
            return;
        }
        let width = self.packed_width(ty).unwrap_or(64);
        let signed = matches!(
            ty,
            TypeExpr::Builtin {
                name: BuiltinTy::SInt | BuiltinTy::SIntCap,
                ..
            }
        );
        let numeric = matches!(
            ty,
            TypeExpr::Builtin {
                name: BuiltinTy::UInt
                    | BuiltinTy::UIntCap
                    | BuiltinTy::Bits
                    | BuiltinTy::Int
                    | BuiltinTy::SInt
                    | BuiltinTy::SIntCap,
                ..
            }
        );
        let rhs = if width > 128 {
            let words = width.div_ceil(32);
            format!("harc_rt::harc_wide_trunc<{words}>(harc_rt::harc_read({raw_expr}), {width})")
        } else if signed && width <= 64 {
            format!(
                "static_cast<int64_t>(harc_rt::harc_sext_u128(static_cast<_harc_u128>(harc_rt::harc_read({raw_expr})), {width}, 64))"
            )
        } else if numeric {
            let cty = self.record_field_c_type(ty);
            format!(
                "static_cast<{cty}>(harc_rt::harc_trunc_u128(static_cast<_harc_u128>(harc_rt::harc_read({raw_expr})), {width}))"
            )
        } else {
            let cty = self.record_field_c_type(ty);
            format!("static_cast<{cty}>(harc_rt::harc_read({raw_expr}))")
        };
        self.pad(depth);
        writeln!(self.out, "{target_expr} = {rhs};").ok();
    }

    fn emit_structured_drive_field(
        &mut self,
        ty: &TypeExpr,
        sig_expr: &str,
        value_expr: &str,
        depth: usize,
    ) {
        if let Some((elem, len)) = fixed_vec_type_args(ty) {
            for i in 0..len {
                self.emit_structured_drive_field(
                    elem,
                    &format!("{sig_expr}[{i}]"),
                    &format!("{value_expr}[{i}]"),
                    depth,
                );
            }
            return;
        }
        self.pad(depth);
        if let Some(name) = self.packed_record_name(ty) {
            writeln!(self.out, "harc_drive_{name}({sig_expr}, {value_expr});").ok();
        } else {
            let width = self.packed_width(ty);
            let normalized = match width {
                Some(w) if w > 128 => {
                    format!("harc_rt::harc_wide_mask_bits({value_expr}, {w})")
                }
                Some(w) => {
                    format!("harc_rt::harc_trunc_u128(static_cast<_harc_u128>({value_expr}), {w})")
                }
                None => value_expr.to_string(),
            };
            writeln!(self.out, "harc_rt::harc_assign({sig_expr}, {normalized});").ok();
        }
    }

    fn emit_record_pack_helpers(&mut self, name: &str, fields: &[Field]) {
        let Some(width) = fields
            .iter()
            .try_fold(0usize, |acc, f| self.packed_width(&f.ty).map(|w| acc + w))
        else {
            return;
        };
        let words = width.div_ceil(32).max(1);
        writeln!(
            self.out,
            "static harc_rt::HarcWide<{words}> harc_pack_{name}(const {name}& value) {{"
        )
        .ok();
        self.pad(1);
        writeln!(self.out, "harc_rt::HarcWide<{words}> _packed{{}};").ok();
        let mut offset = 0usize;
        for f in fields.iter().rev() {
            self.emit_pack_bits(&f.ty, &format!("value.{}", f.name.name), offset, 1);
            offset += self.packed_width(&f.ty).unwrap_or(0);
        }
        self.pad(1);
        writeln!(self.out, "return _packed;").ok();
        writeln!(self.out, "}}").ok();
        writeln!(
            self.out,
            "template<typename Raw> static {name} harc_unpack_{name}(const Raw& raw) {{"
        )
        .ok();
        let raw_checks: Vec<String> = fields
            .iter()
            .map(|f| format!("raw.{}", f.name.name))
            .collect();
        if !raw_checks.is_empty() {
            self.pad(1);
            writeln!(
                self.out,
                "if constexpr (requires {{ {}; }}) {{",
                raw_checks.join("; ")
            )
            .ok();
            self.pad(2);
            writeln!(self.out, "{name} value{{}};").ok();
            for f in fields {
                self.emit_structured_unpack_field(
                    &f.ty,
                    &format!("value.{}", f.name.name),
                    &format!("raw.{}", f.name.name),
                    2,
                );
            }
            self.pad(2);
            writeln!(self.out, "return value;").ok();
            self.pad(1);
            writeln!(self.out, "}} else {{").ok();
        }
        self.pad(1);
        writeln!(
            self.out,
            "auto _packed = harc_rt::harc_wide_zext<{words}>(harc_rt::harc_read(raw));"
        )
        .ok();
        self.pad(1);
        writeln!(self.out, "{name} value{{}};").ok();
        let mut offset = 0usize;
        for f in fields.iter().rev() {
            self.emit_unpack_bits(&f.ty, &format!("value.{}", f.name.name), offset, 1);
            offset += self.packed_width(&f.ty).unwrap_or(0);
        }
        self.pad(1);
        writeln!(self.out, "return value;").ok();
        if !raw_checks.is_empty() {
            self.pad(1);
            writeln!(self.out, "}}").ok();
        }
        writeln!(self.out, "}}").ok();
        writeln!(
            self.out,
            "template<typename Sig> static void harc_drive_{name}(Sig& sig, const {name}& value) {{"
        )
        .ok();
        if !raw_checks.is_empty() {
            self.pad(1);
            writeln!(
                self.out,
                "if constexpr (requires {{ {}; }}) {{",
                raw_checks
                    .iter()
                    .map(|s| s.replacen("raw.", "sig.", 1))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
            .ok();
            for f in fields {
                self.emit_structured_drive_field(
                    &f.ty,
                    &format!("sig.{}", f.name.name),
                    &format!("value.{}", f.name.name),
                    2,
                );
            }
            self.pad(1);
            writeln!(self.out, "}} else {{").ok();
            self.pad(2);
            writeln!(
                self.out,
                "harc_rt::harc_assign(sig, harc_pack_{name}(value));"
            )
            .ok();
            self.pad(1);
            writeln!(self.out, "}}").ok();
        } else {
            self.pad(1);
            writeln!(
                self.out,
                "harc_rt::harc_assign(sig, harc_pack_{name}(value));"
            )
            .ok();
        }
        writeln!(self.out, "}}").ok();
        writeln!(self.out, "").ok();
    }

    fn record_field_default(&self, f: &Field) -> String {
        if let Some(d) = &f.default {
            return format_simple_expr(d);
        }
        if is_list_type(&f.ty) {
            return "{}".into();
        }
        if let TypeExpr::Named { name, .. } = &f.ty {
            if name
                .segments
                .last()
                .is_some_and(|s| self.is_record_type(&s.name))
            {
                return "{}".into();
            }
        }
        field_default(f)
    }

    fn emit_record_struct(&mut self, name: &str, fields: &[Field]) {
        writeln!(self.out, "struct {name} {{").ok();
        for f in fields {
            let cty = self.record_field_c_type(&f.ty);
            let init = self.record_field_default(f);
            writeln!(self.out, "{INDENT}{cty} {} = {init};", f.name.name).ok();
        }
        writeln!(self.out, "}};").ok();

        let field_names: Vec<&str> = fields.iter().map(|f| f.name.name.as_str()).collect();
        if field_names.is_empty() {
            writeln!(
                self.out,
                "inline bool operator==(const {0}& a, const {0}& b) {{ (void)a; (void)b; return true; }}",
                name
            )
            .ok();
        } else {
            write!(
                self.out,
                "inline bool operator==(const {0}& a, const {0}& b) {{ return ",
                name
            )
            .ok();
            for (i, fname) in field_names.iter().enumerate() {
                if i > 0 {
                    write!(self.out, " && ").ok();
                }
                write!(self.out, "a.{fname} == b.{fname}").ok();
            }
            writeln!(self.out, "; }}").ok();
        }
        writeln!(
            self.out,
            "inline bool operator!=(const {0}& a, const {0}& b) {{ return !(a == b); }}",
            name
        )
        .ok();
        writeln!(self.out, "").ok();
        self.emit_record_pack_helpers(name, fields);
    }

    /// Emit a plain `struct` as the shared value-record shape. It gets
    /// the same PRNG helper as a transaction; transaction-only features
    /// such as top-level keeps stay layered in the randomize call path.
    pub(super) fn emit_struct_record(&mut self, s: &StructDecl) {
        self.emit_record_struct(&s.name.name, &s.fields);
        self.emit_record_randomize_fn(&s.name.name, &s.fields);
    }

    /// Emit a transaction as the shared value-record shape plus a
    /// `randomize_T(&t)` function. Transaction keeps are not emitted here;
    /// the call site merges them into the solver path.
    pub(super) fn emit_transaction(&mut self, t: &TransactionDecl) {
        let fields = txn_all_fields(&t.body);
        self.emit_record_struct(&t.name.name, &fields);
        self.emit_record_randomize_fn(&t.name.name, &fields);
    }
}
