//! Record layout and native bridge selection helpers.
//!
//! These queries are shared by the focused record renderer and the remaining
//! native TLM, expression, and local-value emission paths.

use super::*;

impl Emitter {
    pub(super) fn is_record_type(&self, name: &str) -> bool {
        self.transactions.contains(name) || self.structs.contains(name)
    }

    pub(super) fn record_field_c_type(&self, t: &TypeExpr) -> String {
        if is_list_type(t) {
            let inner = list_elem_type(t)
                .map(|ty| self.record_field_c_type(ty))
                .unwrap_or_else(|| "uint64_t".into());
            return format!("std::vector<{inner}>");
        }
        if let Some((elem, len)) = fixed_vec_type_args(t) {
            let inner = self.record_field_c_type(elem);
            return format!("std::array<{inner}, {len}>");
        }
        if let TypeExpr::Named { name, .. } = t {
            if let Some(last) = name.segments.last().map(|s| s.name.as_str()) {
                if self.is_record_type(last) {
                    return last.to_string();
                }
            }
        }
        txn_field_c_type(t)
    }

    pub(super) fn packed_record_name(&self, t: &TypeExpr) -> Option<String> {
        let TypeExpr::Named { name, .. } = t else {
            return None;
        };
        let last = name.segments.last()?.name.clone();
        self.is_record_type(&last).then_some(last)
    }

    pub(super) fn packed_width(&self, t: &TypeExpr) -> Option<usize> {
        if let Some((elem, len)) = fixed_vec_type_args(t) {
            return self.packed_width(elem).map(|w| w * len);
        }
        match t {
            TypeExpr::Builtin { name, args, .. } => match name {
                BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits => {
                    Some(int_width_from_args(args).unwrap_or(64) as usize)
                }
                BuiltinTy::SInt | BuiltinTy::SIntCap => {
                    Some(int_width_from_args(args).unwrap_or(64) as usize)
                }
                BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => Some(1),
                BuiltinTy::Int => Some(32),
                _ => None,
            },
            TypeExpr::Named { name, .. } => {
                let last = name.segments.last()?.name.as_str();
                let fields = self.record_fields.get(last)?;
                fields
                    .iter()
                    .try_fold(0usize, |acc, f| self.packed_width(&f.ty).map(|w| acc + w))
            }
        }
    }

    pub(super) fn record_drive_stmt(
        &self,
        ty: &TypeExpr,
        dst_expr: &str,
        value_expr: &str,
    ) -> String {
        if let Some(name) = self.packed_record_name(ty) {
            format!("harc_drive_{name}({dst_expr}, {value_expr});")
        } else {
            format!("harc_rt::harc_assign({dst_expr}, {value_expr});")
        }
    }

    pub(super) fn record_unpack_expr(&self, ty: &TypeExpr, raw_expr: &str) -> String {
        if let Some(name) = self.packed_record_name(ty) {
            format!("harc_unpack_{name}({raw_expr})")
        } else {
            let width = self.packed_width(ty).unwrap_or(64);
            let signed = matches!(
                ty,
                TypeExpr::Builtin {
                    name: BuiltinTy::SInt | BuiltinTy::SIntCap,
                    ..
                }
            );
            if width > 128 {
                format!(
                    "harc_rt::harc_wide_trunc<{}>(harc_rt::harc_read({raw_expr}), {width})",
                    width.div_ceil(32)
                )
            } else if signed && width <= 64 {
                format!(
                    "static_cast<int64_t>(harc_rt::harc_sext_u128(static_cast<_harc_u128>(harc_rt::harc_read({raw_expr})), {width}, 64))"
                )
            } else {
                let cty = self.c_type_for_param(ty);
                format!(
                    "static_cast<{cty}>(harc_rt::harc_trunc_u128(static_cast<_harc_u128>(harc_rt::harc_read({raw_expr})), {width}))"
                )
            }
        }
    }
}
