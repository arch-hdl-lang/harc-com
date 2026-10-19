//! Small renderer-neutral helpers shared by native backends.

use crate::ast::*;
use crate::codegen::dut_interface::eval_const_i64;
use crate::codegen::source_transform::{
    fixed_vec_type_args, is_list_type, native_container_kind, scalar_leaf_kind,
    verilated_handle_name, NativeContainerKind, ScalarLeafKind,
};
use std::fmt::Write;

const INDENT: &str = "    ";

pub(crate) fn c_ident(s: &str) -> String {
    s.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn component_unique_registry_name<'a>(
    field_names: impl IntoIterator<Item = &'a str>,
) -> String {
    let occupied = field_names
        .into_iter()
        .map(c_ident)
        .collect::<std::collections::HashSet<_>>();
    collision_free_generated_ident("_harc_unique", &occupied)
}

pub(crate) fn collision_free_generated_ident(
    preferred: &str,
    occupied: &std::collections::HashSet<String>,
) -> String {
    if !occupied.contains(preferred) {
        return preferred.to_string();
    }
    for suffix in 1u32.. {
        let candidate = format!("{preferred}_{suffix}");
        if !occupied.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!("generated identifier suffix space exhausted")
}

pub(crate) fn bus_param_env(
    bus: &BusDecl,
    bind_ty: Option<&TypeExpr>,
) -> std::collections::HashMap<String, i64> {
    let mut env = std::collections::HashMap::new();
    for p in &bus.params {
        if let Some(d) = &p.default {
            if let Some(v) = eval_const_i64(d, &env) {
                env.insert(p.name.name.clone(), v);
            }
        }
    }
    if let Some(TypeExpr::Named { generics, .. }) = bind_ty {
        for g in generics {
            if let TypeArg::Named { name, value } = g {
                if let Some(v) = eval_const_i64(value, &env) {
                    env.insert(name.name.clone(), v);
                }
            }
        }
    }
    env
}

pub(crate) fn bus_param_env_with_port_override(
    bus: &BusDecl,
    bind_ty: Option<&TypeExpr>,
    port_override: Option<&std::collections::HashMap<String, i64>>,
) -> std::collections::HashMap<String, i64> {
    let mut env = bus_param_env(bus, bind_ty);
    if let Some(ov) = port_override {
        for (k, v) in ov {
            env.insert(k.clone(), *v);
        }
    }
    env
}

pub(crate) fn gate_passes(
    gate: Option<&Expr>,
    env: &std::collections::HashMap<String, i64>,
) -> bool {
    match gate {
        None => true,
        Some(cond) => match eval_const_i64(cond, env) {
            Some(v) => v != 0,
            None => true,
        },
    }
}

pub(crate) fn value_abi_class_cpp(class: crate::ir::ValueAbiClass) -> String {
    match class {
        crate::ir::ValueAbiClass::Bool => "bool".into(),
        crate::ir::ValueAbiClass::Unsigned64 => "uint64_t".into(),
        crate::ir::ValueAbiClass::Signed64 => "int64_t".into(),
        crate::ir::ValueAbiClass::Wide128 => "_harc_u128".into(),
        crate::ir::ValueAbiClass::WideWords(words) => {
            format!("harc_rt::HarcWide<{words}>")
        }
        crate::ir::ValueAbiClass::Record(record) => format!("record#{}", record.index()),
    }
}

pub(crate) fn cpp_uint_for_width(w: Option<u32>) -> String {
    value_abi_class_cpp(
        crate::ir::value_abi_class(&crate::ir::IrType::UInt(w))
            .expect("uint always has a native ABI class"),
    )
}

pub(crate) fn cpp_sint_for_width(w: Option<u32>) -> String {
    value_abi_class_cpp(
        crate::ir::value_abi_class(&crate::ir::IrType::SInt(w))
            .expect("sint always has a native ABI class"),
    )
}

pub(crate) fn ir_vec_elem_class(t: &crate::ir::IrType) -> Option<String> {
    use crate::ir::IrType;
    match t {
        IrType::UInt(w) => Some(cpp_uint_for_width(*w)),
        IrType::SInt(w) => Some(cpp_sint_for_width(*w)),
        IrType::Bool => Some("bool".into()),
        IrType::Record(rid) => Some(format!("record#{}", rid.index())),
        IrType::FixedVec { elem, len } => {
            Some(format!("std::array<{}, {len}>", ir_vec_elem_class(elem)?))
        }
        _ => None,
    }
}

pub(crate) fn int_width_from_args(args: &[TypeArg]) -> Option<u32> {
    args.first().and_then(|a| match a {
        TypeArg::Expr(e) => match &*e.kind {
            ExprKind::Int(s) => s.replace('_', "").parse().ok(),
            _ => None,
        },
        _ => None,
    })
}

pub(crate) fn txn_field_c_type(t: &TypeExpr) -> String {
    match native_container_kind(t) {
        Some(NativeContainerKind::List(elem)) => {
            let inner = elem
                .map(txn_field_c_type)
                .unwrap_or_else(|| "uint64_t".into());
            return format!("std::vector<{inner}>");
        }
        Some(NativeContainerKind::FixedVec { elem, len }) => {
            let inner = txn_field_c_type(elem);
            return format!("std::array<{inner}, {len}>");
        }
        None => {}
    }
    match t {
        TypeExpr::Builtin { name, args, .. } => {
            scalar_leaf_c_type(name, args).unwrap_or_else(|| "uint64_t".into())
        }
        TypeExpr::Named { .. } => "int64_t".into(),
    }
}

fn scalar_leaf_c_type(name: &BuiltinTy, args: &[TypeArg]) -> Option<String> {
    match scalar_leaf_kind(name)? {
        ScalarLeafKind::Unsigned => Some(cpp_uint_for_width(int_width_from_args(args))),
        ScalarLeafKind::Signed => Some(cpp_sint_for_width(int_width_from_args(args))),
        ScalarLeafKind::Bool => Some("bool".into()),
    }
}

pub(crate) fn tseq_inner_type(t: &TypeExpr) -> Option<String> {
    if let TypeExpr::Builtin {
        name: BuiltinTy::TSeq,
        args,
        ..
    } = t
    {
        if let Some(TypeArg::Type(inner)) = args.first() {
            return Some(c_type_for(inner));
        }
        if let Some(TypeArg::Expr(e)) = args.first() {
            if let ExprKind::Ident(id) = &*e.kind {
                return Some(id.name.clone());
            }
        }
    }
    None
}

pub(crate) fn c_type_for(t: &TypeExpr) -> String {
    if is_list_type(t) || fixed_vec_type_args(t).is_some() {
        return txn_field_c_type(t);
    }
    match t {
        TypeExpr::Builtin { name, args, .. } => match name {
            BuiltinTy::UInt | BuiltinTy::UIntCap | BuiltinTy::Bits | BuiltinTy::Int => {
                cpp_uint_for_width(int_width_from_args(args))
            }
            BuiltinTy::SInt | BuiltinTy::SIntCap => cpp_sint_for_width(int_width_from_args(args)),
            BuiltinTy::Bool | BuiltinTy::BoolLower | BuiltinTy::Bit => {
                ir_vec_elem_class(&crate::ir::IrType::Bool)
                    .expect("bool always has a native ABI class")
            }
            BuiltinTy::String => "const char*".into(),
            BuiltinTy::Time => "uint64_t".into(),
            BuiltinTy::TSeq => {
                let inner = tseq_inner_type(t).unwrap_or_else(|| "uint64_t".into());
                format!("const std::vector<{inner}>&")
            }
            BuiltinTy::Queue => {
                let inner = args
                    .first()
                    .map(|a| match a {
                        TypeArg::Type(ty) => txn_field_c_type(ty),
                        TypeArg::Expr(e) => match &*e.kind {
                            ExprKind::Ident(id) => id.name.clone(),
                            _ => "uint64_t".into(),
                        },
                        _ => "uint64_t".into(),
                    })
                    .unwrap_or_else(|| "uint64_t".into());
                format!("harc_rt::HarcQueue<{inner}>&")
            }
            _ => format!("/* TODO: type {:?} */ uint64_t", name),
        },
        TypeExpr::Named { .. } => {
            let last = verilated_handle_name(t).unwrap_or("");
            format!("V{last}*")
        }
    }
}

pub(crate) fn cpp_param_names(params: &[Param]) -> Vec<String> {
    let mut discard_idx = 0usize;
    params
        .iter()
        .map(|p| {
            if p.name.name == "_" {
                let name = format!("_discard{discard_idx}");
                discard_idx += 1;
                name
            } else {
                p.name.name.clone()
            }
        })
        .collect()
}

pub(crate) fn emit_extern_fn_decls(out: &mut String, file: &SourceFile) {
    let extern_fns: Vec<&ExternFnDecl> = file
        .items
        .iter()
        .filter_map(|it| match it {
            Item::ExternFn(f) => Some(f),
            _ => None,
        })
        .collect();
    if extern_fns.is_empty() {
        return;
    }
    writeln!(
        out,
        "// extern reference functions (spec §9) — implementations"
    )
    .ok();
    writeln!(
        out,
        "// supplied via `harc sim --ref-src <file>` and linked into the"
    )
    .ok();
    writeln!(out, "// verilator-built binary.").ok();
    writeln!(out, "extern \"C\" {{").ok();
    for f in &extern_fns {
        let param_names = cpp_param_names(&f.params);
        let ret = f
            .return_ty
            .as_ref()
            .map(c_type_for)
            .unwrap_or_else(|| "void".to_string());
        write!(out, "{INDENT}{ret} {}(", f.name.name).ok();
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                write!(out, ", ").ok();
            }
            let pty =
                p.ty.as_ref()
                    .map(c_type_for)
                    .unwrap_or_else(|| "int64_t".to_string());
            write!(out, "{pty} {}", param_names[i]).ok();
        }
        writeln!(out, ");").ok();
    }
    writeln!(out, "}}").ok();
    writeln!(out).ok();
}
