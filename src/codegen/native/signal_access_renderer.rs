//! Native C++ register, probe, and signal-assignment emission.

use super::*;

impl Emitter {
    /// Emit a single-line `<lhs> = <rhs>` style statement, dispatching
    /// on three orthogonal axes:
    ///   1. Pointer-rooted signal target → wrap with the
    ///      `harc_rt::harc_*` template helpers so `VlWide<N>` ports
    ///      compile against integer values.
    ///   2. Plain local target → bare `lhs = rhs;`.
    ///   3. RHS is a > 128-bit hex literal → route through
    ///      `harc_rt::harc_assign_words` with an
    ///      `std::initializer_list<uint32_t>`. Without this, the
    ///      literal would clamp at 128 bits in `c_int_literal`'s
    ///      composite path and silently lose the high bits.
    /// If `target` is a `regs.NAME` field access where `regs` is a
    /// RAL regblock binding, return `(regs_var, helper_var, helper_ty,
    /// offset_literal, register-access-policy)`. Used by
    /// `emit_signal_assignment` (write side) and `emit_expr_with_arrow`
    /// (read side).
    /// Compile-time-unrolled bitbash walk. Detects a bare statement
    /// `bitbash(<regs_ident>)` where the argument is a `let regs : R =
    /// bind helper` binding, and emits one write-all-ones / read-back
    /// + write-zero / read-back pair per RW register in the regblock.
    /// RO/WO registers are skipped (RO can't accept the write; WO
    /// can't be read meaningfully). Failures bump the `errors`
    /// counter and log via `sim_log_line` so the existing test-result
    /// machinery picks them up.
    pub(super) fn try_emit_bitbash(&mut self, e: &Expr, depth: usize) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Ident(name) = &*callee.kind else {
            return false;
        };
        if name.name != "bitbash" || args.len() != 1 {
            return false;
        }
        let arg = match &args[0] {
            CallArg::Expr(ex) => ex,
            CallArg::Named { value, .. } => value,
        };
        let ExprKind::Ident(regs_id) = &*arg.kind else {
            return false;
        };
        let Some(regs_ty) = self.let_types.get(&regs_id.name).cloned() else {
            return false;
        };
        let Some(block) = self.regblocks.get(&regs_ty).cloned() else {
            return false;
        };
        let Some(helper_var) = self.let_helper.get(&regs_id.name).cloned() else {
            return false;
        };
        let Some(helper_ty) = self.let_types.get(&helper_var).cloned() else {
            return false;
        };

        let default_w = block.default_width.unwrap_or(32);
        self.pad(depth);
        writeln!(
            self.out,
            "// bitbash({}) — RAL walk-all over RW regs of {}",
            regs_id.name, regs_ty
        )
        .ok();
        for reg in &block.registers {
            let w = reg.width.unwrap_or(default_w);
            let mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            let off = c_int_literal_from(&reg.offset.kind);
            let regname = &reg.name.name;
            if !reg.access.writes_to_bus() || !reg.access.reads_from_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// bitbash: skipping {} (access {})",
                    regname,
                    reg.access.keyword()
                )
                .ok();
                continue;
            }
            // Two patterns: all-ones (masked to register width), then
            // zero. Each pair: write → read → compare → bump errors
            // if mismatch + sim_log.
            for (pat_label, pat) in [("ones", mask), ("zero", 0u64)] {
                self.pad(depth);
                writeln!(self.out, "{{").ok();
                self.pad(depth + 1);
                writeln!(self.out, "uint64_t _bb_pat = 0x{pat:x}ull;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "{helper_ty}_write({helper_var}, {off}, _bb_pat);").ok();
                self.pad(depth + 1);
                writeln!(
                    self.out,
                    "uint64_t _bb_got = {helper_ty}_read({helper_var}, {off});"
                )
                .ok();
                self.pad(depth + 1);
                writeln!(self.out, "if (_bb_got != _bb_pat) {{").ok();
                self.pad(depth + 2);
                writeln!(self.out,
                    "sim_log_line(\"FAIL\", \"bitbash {regname} {pat_label}: wrote 0x%llx, got 0x%llx\", (long long)_bb_pat, (long long)_bb_got);").ok();
                self.pad(depth + 2);
                let runtime_context = self.randomize_context_ref().to_string();
                writeln!(self.out, "{runtime_context}.errors++;").ok();
                self.pad(depth + 1);
                writeln!(self.out, "}}").ok();
                self.pad(depth);
                writeln!(self.out, "}}").ok();
            }
        }
        true
    }

    /// `regs.record_write(addr, data)` — **PASSIVE PATH** mirror-only
    /// record of an observed bus write, keyed by address. Decodes
    /// `addr` against the regblock's register offsets at codegen time
    /// (no bus traffic — the checker already saw the transaction),
    /// updates the matching mirror cell masked to the register width,
    /// **then fires `<regs>_cbs.REG(data)` if a per-register callback
    /// is registered**.
    ///
    /// This is the only emission site that dispatches RAL callbacks.
    /// The two ACTIVE frontdoor write paths above (`regs.NAME = expr`
    /// and `regs.REG.FIELD = expr`) intentionally do NOT dispatch —
    /// see their comments and `docs/ral-support.md` §3.2 for why.
    /// The asymmetry is locked from silent drift by the
    /// `regblock_active_frontdoor_write_does_not_dispatch_callback`
    /// test in `tests/codegen.rs`.
    ///
    /// Returns false (no-op) if the call shape isn't a record_write on
    /// a regblock binding.
    pub(super) fn try_emit_record_write(&mut self, e: &Expr, depth: usize) -> bool {
        let ExprKind::Call { callee, args } = &*e.kind else {
            return false;
        };
        let ExprKind::Field { target, name } = &*callee.kind else {
            return false;
        };
        if name.name != "record_write" || args.len() != 2 {
            return false;
        }
        let ExprKind::Ident(regs_id) = &*target.kind else {
            return false;
        };
        let Some(regs_ty) = self.let_types.get(&regs_id.name).cloned() else {
            return false;
        };
        let Some(block) = self.regblocks.get(&regs_ty).cloned() else {
            return false;
        };
        let addr = match &args[0] {
            CallArg::Expr(ex) => ex,
            CallArg::Named { value, .. } => value,
        };
        let data = match &args[1] {
            CallArg::Expr(ex) => ex,
            CallArg::Named { value, .. } => value,
        };
        let regs_var = &regs_id.name;
        let default_w = block.default_width.unwrap_or(32);
        self.pad(depth);
        writeln!(self.out, "{{").ok();
        self.pad(depth + 1);
        write!(self.out, "uint64_t _rec_addr = (uint64_t)(").ok();
        self.emit_expr(addr);
        writeln!(self.out, ");").ok();
        self.pad(depth + 1);
        write!(self.out, "uint64_t _rec_data = (uint64_t)(").ok();
        self.emit_expr(data);
        writeln!(self.out, ");").ok();
        // Recursion guard: a callback body can call `record_write` again
        // (legitimately, to model a CSR side-effect that writes another
        // register, or accidentally, to write the same register). Bump
        // a per-binding depth counter on entry and abort if it crosses
        // `HARC_RAL_CB_MAX_DEPTH` — without the bound a self-write
        // would blow the stack. See docs/ral-support.md §3.2.
        let runtime_context = self.randomize_context_ref().to_string();
        self.pad(depth + 1);
        writeln!(
            self.out,
            "if ({regs_var}_cb_depth >= HARC_RAL_CB_MAX_DEPTH) {{ sim_log_line(\"FATAL\", \"RAL record_write callback recursion exceeded HARC_RAL_CB_MAX_DEPTH (%u) on binding `{regs_var}` at addr 0x%llx\", (unsigned)HARC_RAL_CB_MAX_DEPTH, (unsigned long long)_rec_addr); {}.errors++; _fatal = true; }} else {{",
            runtime_context,
        )
        .ok();
        self.pad(depth + 2);
        writeln!(self.out, "{regs_var}_cb_depth++;").ok();
        // Emit an if/else-if chain so a single addr matches at most one
        // register. Register offsets *should* be unique, but the regblock
        // parser doesn't enforce uniqueness here, so chaining keeps
        // record_write semantics deterministic if a duplicate slips
        // through (the first-declared register wins, matching
        // first-match dispatch elsewhere in cpp_tb).
        for (i, reg) in block.registers.iter().enumerate() {
            let w = reg.width.unwrap_or(default_w);
            let mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            let cty = mirror_field_c_type(w);
            let off = c_int_literal_from(&reg.offset.kind);
            let regname = &reg.name.name;
            self.pad(depth + 2);
            let kw = if i == 0 { "if" } else { "else if" };
            writeln!(
                self.out,
                "{kw} (_rec_addr == (uint64_t)({off})) {{ {regs_var}.{regname} = ({cty})(_rec_data & 0x{mask:x}ull); if ({regs_var}_cbs.{regname}) {regs_var}_cbs.{regname}(_rec_data); }}",
            )
            .ok();
        }
        self.pad(depth + 2);
        writeln!(self.out, "{regs_var}_cb_depth--;").ok();
        self.pad(depth + 1);
        writeln!(self.out, "}}").ok();
        self.pad(depth);
        writeln!(self.out, "}}").ok();
        true
    }

    /// 3-level `chip.inst.REG` addrmap access. Returns
    /// `(chip_var, instance_path, helper_var, helper_ty,
    ///   effective_offset_expr, register-access-policy)`.
    /// 3-level `chip.inst.REG` addrmap access. If `inst` is aliased
    /// to another instance via `alias of`, the returned
    /// `instance_path` points at the TARGET's mirror cell (one
    /// storage shared across windows) while the effective bus
    /// offset still uses this instance's own base.
    pub(super) fn resolve_addrmap_register_lookup(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, String, RegAccess)> {
        let ExprKind::Field {
            target: mid,
            name: reg_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: outer,
            name: inst_name,
        } = &*mid.kind
        else {
            return None;
        };
        let ExprKind::Ident(chip_id) = &*outer.kind else {
            return None;
        };
        let chip_ty = self.let_types.get(&chip_id.name)?;
        let amap = self.addrmaps.get(chip_ty)?;
        let inst = amap
            .instances
            .iter()
            .find(|i| i.name.name == inst_name.name)?;
        let block = self.regblocks.get(&inst.regblock_ty.name)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let helper_var = self.let_helper.get(&chip_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let base = c_int_literal_from(&inst.base_addr.kind);
        let off = c_int_literal_from(&reg.offset.kind);
        let effective = format!("({base} + {off})");
        let mirror_inst_name = inst
            .alias_of
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| inst.name.name.clone());
        let instance_path = format!("{}.{}", chip_id.name, mirror_inst_name);
        Some((
            chip_id.name.clone(),
            instance_path,
            helper_var,
            helper_ty,
            effective,
            reg.access,
        ))
    }

    /// 4-level `chip.inst.REG.FIELD` addrmap+field access. Alias-
    /// aware like its 3-level sibling.
    pub(super) fn resolve_addrmap_subfield_lookup(
        &self,
        target: &Expr,
    ) -> Option<(
        String,
        String,
        String,
        String,
        String,
        String,
        &'static str,
        u32,
        u32,
        RegAccess,
    )> {
        let ExprKind::Field {
            target: lvl3,
            name: fld_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: lvl2,
            name: reg_name,
        } = &*lvl3.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: lvl1,
            name: inst_name,
        } = &*lvl2.kind
        else {
            return None;
        };
        let ExprKind::Ident(chip_id) = &*lvl1.kind else {
            return None;
        };
        let chip_ty = self.let_types.get(&chip_id.name)?;
        let amap = self.addrmaps.get(chip_ty)?;
        let inst = amap
            .instances
            .iter()
            .find(|i| i.name.name == inst_name.name)?;
        let block = self.regblocks.get(&inst.regblock_ty.name)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let fld = reg.fields.iter().find(|f| f.name.name == fld_name.name)?;
        let helper_var = self.let_helper.get(&chip_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let base = c_int_literal_from(&inst.base_addr.kind);
        let off = c_int_literal_from(&reg.offset.kind);
        let effective = format!("({base} + {off})");
        let reg_width = reg.width.unwrap_or(block.default_width.unwrap_or(32));
        let reg_c_type = mirror_field_c_type(reg_width);
        let bit_width = field_bit_width(&fld.ty);
        let mirror_inst_name = inst
            .alias_of
            .as_ref()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| inst.name.name.clone());
        let instance_path = format!("{}.{}", chip_id.name, mirror_inst_name);
        Some((
            chip_id.name.clone(),
            instance_path,
            reg.name.name.clone(),
            helper_var,
            helper_ty,
            effective,
            reg_c_type,
            fld.bit_pos,
            bit_width,
            fld.access,
        ))
    }

    pub(super) fn resolve_regblock_field_lookup(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, RegAccess)> {
        let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(regs_id) = &*outer.kind else {
            return None;
        };
        let regs_ty = self.let_types.get(&regs_id.name)?;
        let block = self.regblocks.get(regs_ty)?;
        let reg = block.registers.iter().find(|r| r.name.name == name.name)?;
        let helper_var = self.let_helper.get(&regs_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let offset_lit = c_int_literal_from(&reg.offset.kind);
        Some((
            regs_id.name.clone(),
            helper_var,
            helper_ty,
            offset_lit,
            reg.access,
        ))
    }

    fn resolve_regblock_field_write(
        &self,
        target: &Expr,
    ) -> Option<(String, String, String, String, RegAccess)> {
        self.resolve_regblock_field_lookup(target)
    }

    /// If `target` is a 3-level `regs.REG.FIELD` access where `regs`
    /// is a RAL regblock binding and FIELD is declared inside REG,
    /// return all metadata needed to emit a masked write / shifted
    /// read: `(regs_var, helper_var, helper_ty, offset_lit, reg_name,
    /// reg_c_type, bit_pos, bit_width, field-access-policy)`.
    pub(super) fn resolve_regblock_subfield_lookup(
        &self,
        target: &Expr,
    ) -> Option<(
        String,
        String,
        String,
        String,
        String,
        &'static str,
        u32,
        u32,
        RegAccess,
    )> {
        let ExprKind::Field {
            target: mid,
            name: fld_name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Field {
            target: outer,
            name: reg_name,
        } = &*mid.kind
        else {
            return None;
        };
        let ExprKind::Ident(regs_id) = &*outer.kind else {
            return None;
        };
        let regs_ty = self.let_types.get(&regs_id.name)?;
        let block = self.regblocks.get(regs_ty)?;
        let reg = block
            .registers
            .iter()
            .find(|r| r.name.name == reg_name.name)?;
        let fld = reg.fields.iter().find(|f| f.name.name == fld_name.name)?;
        let helper_var = self.let_helper.get(&regs_id.name)?.clone();
        let helper_ty = self.let_types.get(&helper_var)?.clone();
        let offset_lit = c_int_literal_from(&reg.offset.kind);
        let reg_width = reg.width.unwrap_or(block.default_width.unwrap_or(32));
        let reg_c_type = mirror_field_c_type(reg_width);
        let bit_width = field_bit_width(&fld.ty);
        Some((
            regs_id.name.clone(),
            helper_var,
            helper_ty,
            offset_lit,
            reg.name.name.clone(),
            reg_c_type,
            fld.bit_pos,
            bit_width,
            fld.access,
        ))
    }

    /// Resolve the SV port name for `<bus>.<channel>.<signal>` access.
    /// Checks the `bind ... with { ch.sig: "port" }` remap first;
    /// falls back to the `<prefix>_<channel>_<signal>` convention.
    /// `prefix` is typically the bind variable's name (same as the
    /// `sig_prefix` field stored in `bus_bindings`).
    pub(super) fn bus_signal_name(&self, prefix: &str, channel: &str, signal: &str) -> String {
        if let Some(map) = self.bus_remap.get(prefix) {
            if let Some(port) = map.get(&(channel.to_string(), signal.to_string())) {
                return port.clone();
            }
        }
        format!("{prefix}_{channel}_{signal}")
    }

    /// If `target` is `dut.<probe>` where the probe was declared with
    /// the `force` modifier, return its `ProbeAccessor`. Used by
    /// `emit_signal_assignment` and the `release` statement to emit
    /// the two-store (drv + en) lowering.
    pub(super) fn resolve_force_probe(&self, target: &Expr) -> Option<ProbeAccessor> {
        let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(id) = &*outer.kind else {
            return None;
        };
        if !self.is_active_probe_receiver(&id.name) {
            return None;
        }
        let probe = self.probes.get(&name.name)?;
        if probe.force {
            Some(probe.clone())
        } else {
            None
        }
    }

    pub(super) fn partial_probe_target<'e>(&self, target: &'e Expr) -> Option<&'e str> {
        let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(id) = &*outer.kind else {
            return None;
        };
        (self.is_active_probe_receiver(&id.name) && self.partial_probe_names.contains(&name.name))
            .then_some(name.name.as_str())
    }

    pub(super) fn is_active_probe_receiver(&self, name: &str) -> bool {
        self.probe_receivers.contains(name) && !self.is_shadowed_probe_receiver(name)
    }

    pub(super) fn is_shadowed_probe_receiver(&self, name: &str) -> bool {
        self.probe_receivers.contains(name) && self.is_shadowed_value(name)
    }

    pub(super) fn is_shadowed_value(&self, name: &str) -> bool {
        self.probe_shadow_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(name))
    }

    pub(super) fn push_probe_shadow_scope(&mut self) {
        self.probe_shadow_scopes
            .push(std::collections::HashSet::new());
    }

    pub(super) fn pop_probe_shadow_scope(&mut self) {
        self.probe_shadow_scopes.pop();
    }

    pub(super) fn declare_probe_shadow(&mut self, name: &str) {
        if self.probe_shadow_scopes.is_empty() {
            self.push_probe_shadow_scope();
        }
        self.probe_shadow_scopes
            .last_mut()
            .expect("probe shadow scope was just created")
            .insert(name.to_string());
    }

    pub(super) fn component_probe_receivers(
        &self,
        component: &ComponentDecl,
    ) -> std::collections::HashSet<String> {
        let mut receivers = std::collections::HashSet::new();
        if let Some(dut_type) = self.probe_dut_type.as_deref() {
            for item in &component.items {
                let ComponentItem::Field(field) = item else {
                    continue;
                };
                if self.is_dut_pointer_field_type(&field.ty)
                    && type_simple_name(Some(&field.ty)) == Some(dut_type)
                {
                    receivers.insert(field.name.name.clone());
                }
            }
        }
        if receivers.is_empty() {
            receivers.insert("dut".to_string());
        }
        receivers
    }

    /// If `e` indexes a packed DUT port (i.e. `dut.<port>[i]` with
    /// `<port>` recorded in `vec_lane_widths`), return
    /// `(port-name, lane-width, &index)`.
    /// Used to route the lane access through the backend-agnostic
    /// `harc_rt::harc_vec_lane_*` helpers instead of a raw C++ subscript
    /// (which does not work on an ARCH native scalar or Verilator packed
    /// scalar). Returns `None` for any other shape, including indexing a
    /// true unpacked-`Vec` port (those aren't in the map; direct
    /// `port[i]` array indexing is correct on both backends).
    pub(super) fn dut_packed_lane<'e>(
        &self,
        e: &'e Expr,
    ) -> Option<(String, String, u32, &'e Expr)> {
        let ExprKind::Index { target, index } = &*e.kind else {
            return None;
        };
        let ExprKind::Field {
            target: root,
            name: port,
        } = &*target.kind
        else {
            return None;
        };
        let ExprKind::Ident(root_id) = &*root.kind else {
            return None;
        };
        if !self.pointer_vars.contains(&root_id.name) {
            return None;
        }
        // The map is keyed by top-module DUT port names (from the `--sv`
        // port table and scalar `.arch`/`.archi` interface); only the DUT
        // pointer's ports can match.
        let w = *self.vec_lane_widths.get(&port.name)?;
        Some((root_id.name.clone(), port.name.clone(), w, index))
    }

    pub(super) fn emit_signal_assignment(&mut self, target: &Expr, value: &Expr, depth: usize) {
        // Packed DUT lane/bit write: `dut.<port>[i] = expr`. A raw C++
        // subscript does not compile for either Verilator packed scalars or
        // ARCH native scalar ports; route through
        // `harc_vec_lane_write<W>`, which deposits into a packed value and
        // still array-indexes true array ports via `if constexpr`.
        if let Some((root, port, w, index)) = self.dut_packed_lane(target) {
            if w > 64 {
                self.errors.push(format!(
                    "DUT access `{root}.{port}` indexes a packed {w}-bit lane; packed lane helpers support at most 64 bits"
                ));
                return;
            }
            write!(
                self.out,
                "harc_rt::harc_vec_lane_write<{w}>({root}->{port}, (std::size_t)("
            )
            .ok();
            self.emit_expr(index);
            write!(self.out, "), ").ok();
            self.emit_expr(value);
            writeln!(self.out, ");").ok();
            return;
        }
        // `probe force <name>` write: lower to a paired store of
        // `<name>_drv = expr` and `<name>_en = 1`. See
        // docs/probe-signals.md. The bound SV stub's `always_comb`
        // picks up `_en=1` next cycle and procedurally forces the
        // target path with the latched `_drv` value.
        if let Some(name) = self.partial_probe_target(target) {
            let receiver = match &*target.kind {
                ExprKind::Field { target, .. } => match &*target.kind {
                    ExprKind::Ident(id) => id.name.as_str(),
                    _ => "dut",
                },
                _ => "dut",
            };
            self.errors.push(format!(
                "shared DUT access `{receiver}.{name}` names a probe that is not declared identically by every test in the suite"
            ));
            return;
        }
        if let Some(probe) = self.resolve_force_probe(target) {
            write!(self.out, "dut->rootp->{} = ", probe.drive()).ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            self.pad(depth);
            writeln!(self.out, "dut->rootp->{} = 1;", probe.enable()).ok();
            return;
        }
        // Plain-probe write: not allowed. Surface a clear codegen
        // error rather than emit invalid C++. Read-only probes
        // can't drive their target signal — declare with `force`
        // for fault injection.
        if let ExprKind::Field {
            target: outer,
            name,
        } = &*target.kind
        {
            if let ExprKind::Ident(id) = &*outer.kind {
                if self.is_active_probe_receiver(&id.name) && self.probes.contains_key(&name.name) {
                    self.errors.push(format!(
                        "write to `{}.{}`: read-only probe — declare with `probe force` to enable fault injection",
                        id.name, name.name,
                    ));
                    return;
                }
            }
        }

        // RAL addrmap subfield write: `chip.inst.REG.FIELD = expr`.
        // Identical lowering to the regblock subfield path, but the
        // bus offset is `base(inst) + offset(REG)` and the mirror path
        // walks through the instance: `chip.inst.REG`.
        if let Some((
            _chip_var,
            inst_path,
            reg_name,
            helper_var,
            helper_ty,
            effective_off,
            reg_c_type,
            bit_pos,
            bit_width,
            access,
        )) = self.resolve_addrmap_subfield_lookup(target)
        {
            let mask = field_mask_literal(bit_width);
            write!(
                self.out,
                "{inst_path}.{reg_name} = ({inst_path}.{reg_name} & ~(({reg_c_type})0x{mask:x}u << {bit_pos})) | (((({reg_c_type})(",
            ).ok();
            self.emit_expr(value);
            writeln!(self.out, ")) & 0x{mask:x}u) << {bit_pos});",).ok();
            if access.writes_to_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {effective_off}, {inst_path}.{reg_name});",
                )
                .ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO field — write to bus suppressed (chip mirror still updated)"
                )
                .ok();
            }
            return;
        }

        // RAL addrmap register write: `chip.inst.REG = expr`.
        if let Some((_chip_var, inst_path, helper_var, helper_ty, effective_off, access)) =
            self.resolve_addrmap_register_lookup(target)
        {
            // The outer Field's `name` is the register name itself.
            write!(self.out, "{inst_path}.").ok();
            if let ExprKind::Field { name, .. } = &*target.kind {
                write!(self.out, "{}", name.name).ok();
            }
            write!(self.out, " = ").ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            if access.writes_to_bus() {
                self.pad(depth);
                write!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {effective_off}, "
                )
                .ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO register — write to bus suppressed (mirror updated)"
                )
                .ok();
            }
            return;
        }

        // RAL frontdoor field-level write: `regs.REG.FIELD = expr`.
        // Lowers to a read-modify-write on the mirror (mask out the
        // FIELD bits, OR in the new value shifted into POS) + a bus
        // write of the full register word. Checked before the
        // register-level path because it's strictly more specific
        // (3-level Field expr vs 2-level).
        //
        // **ACTIVE PATH — no callback dispatch.** `on regs.REG`
        // callbacks fire only on the PASSIVE `record_write` decode
        // (see `try_emit_record_write` below). The active frontdoor
        // is intentionally silent: callbacks are for observing
        // externally-driven mirror updates, not the test's own
        // writes. See `docs/ral-support.md` §3.2 (active-vs-passive
        // asymmetry). If you add callback dispatch here you'll
        // double-fire on any test that mixes both paths.
        if let Some((
            regs_var,
            helper_var,
            helper_ty,
            offset_lit,
            reg_name,
            reg_c_type,
            bit_pos,
            bit_width,
            access,
        )) = self.resolve_regblock_subfield_lookup(target)
        {
            let mask = field_mask_literal(bit_width);
            // Mirror update always happens; even RO mirrors might be
            // useful for the read-predict side. For RO we then DROP
            // the bus write — matches the RFC `ro` semantics.
            write!(
                self.out,
                "{regs_var}.{reg_name} = ({regs_var}.{reg_name} & ~(({reg_c_type})0x{mask:x}u << {bit_pos})) | (((({reg_c_type})(",
            ).ok();
            self.emit_expr(value);
            writeln!(self.out, ")) & 0x{mask:x}u) << {bit_pos});",).ok();
            if access.writes_to_bus() {
                self.pad(depth);
                writeln!(
                    self.out,
                    "{helper_ty}_write({helper_var}, {offset_lit}, {regs_var}.{reg_name});",
                )
                .ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO field — write to bus suppressed (regs.{reg_name}.<field> mirror still updated)",
                ).ok();
            }
            return;
        }

        // RAL frontdoor write: `regs.NAME = expr` where `regs` is a
        // `let regs : <Regblock> = bind <helper>` instantiation. Lowers
        // to mirror update + `<HelperType>_write(helper, OFFSET, expr)`.
        //
        // **ACTIVE PATH — no callback dispatch.** Same asymmetry as
        // the field-level frontdoor above: `on regs.NAME` callbacks
        // are passive-only, fired by `record_write` decode. Adding
        // dispatch here would double-fire and break the documented
        // contract — see `docs/ral-support.md` §3.2.
        if let Some((regs_var, helper_var, helper_ty, offset_lit, access)) =
            self.resolve_regblock_field_write(target)
        {
            write!(self.out, "{regs_var}.").ok();
            if let ExprKind::Field { name, .. } = &*target.kind {
                write!(self.out, "{}", name.name).ok();
            }
            write!(self.out, " = ").ok();
            self.emit_expr(value);
            writeln!(self.out, ";").ok();
            if access.writes_to_bus() {
                self.pad(depth);
                write!(self.out, "{helper_ty}_write({helper_var}, {offset_lit}, ").ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            } else {
                self.pad(depth);
                writeln!(
                    self.out,
                    "// RO register — write to bus suppressed (mirror updated)"
                )
                .ok();
            }
            return;
        }

        let pointer_rooted = self.is_pointer_rooted_signal_lvalue(target);
        let wide_words = is_wide_int_literal(value);

        match (pointer_rooted, wide_words) {
            (true, Some(words)) => {
                // > 128-bit literal into a wide signal — word-list path.
                // Route through the *checked* helper, parameterized by the
                // number of 32-bit words the literal's value actually needs
                // (`significant_word_count`). The helper `static_assert`s
                // that this fits the target signal's word capacity, turning
                // a previously-silent over-width truncation (high words
                // dropped / message misaligned) into a hard, named C++
                // compile error. Leading-zero high words don't count, so a
                // value that fits a wider-than-necessary literal is fine.
                let req_words = significant_word_count(&words);
                write!(self.out, "harc_rt::harc_assign_words_checked<{req_words}>(").ok();
                self.emit_lvalue(target);
                write!(self.out, ", {{").ok();
                for (i, w) in words.iter().enumerate() {
                    if i > 0 {
                        write!(self.out, ", ").ok();
                    }
                    write!(self.out, "{w}").ok();
                }
                writeln!(self.out, "}});").ok();
            }
            (true, None) => {
                // Pointer-rooted signal, ≤128b RHS — uniform helper path.
                write!(self.out, "harc_rt::harc_assign(").ok();
                self.emit_lvalue(target);
                write!(self.out, ", ").ok();
                self.emit_expr(value);
                writeln!(self.out, ");").ok();
            }
            (false, _) => {
                // Plain local — bare assignment. (If RHS happens to be
                // a > 128-bit literal here it'll clamp via
                // `c_int_literal`'s fallback; locals aren't wide
                // today so this is a corner that doesn't show up.)
                self.emit_lvalue(target);
                write!(self.out, " = ").ok();
                self.emit_expr(value);
                writeln!(self.out, ";").ok();
            }
        }
    }

    /// Returns true if `e` is an L-value that ultimately lowers to a
    /// `<root>-><field>...` access chain (where `<root>` is a DUT pointer
    /// or bus-binding root). These are the cases where Verilator may
    /// have given the leaf signal a wide type (`VlWide<N>` for >64-bit
    /// ports), so plain `lhs = rhs` won't compile and we instead emit
    /// a `harc_rt::harc_assign(lhs, rhs);` call.
    fn is_pointer_rooted_signal_lvalue(&self, e: &Expr) -> bool {
        if let ExprKind::Field { target, name } = &*e.kind {
            if let ExprKind::Ident(root) = &*target.kind {
                if self.is_shadowed_probe_receiver(&root.name)
                    && (self.probes.contains_key(&name.name)
                        || self.partial_probe_names.contains(&name.name))
                {
                    return false;
                }
            }
        }
        // Walk down field-access chain; root must be Ident in either
        // pointer_vars (e.g. `dut`) or bus_bindings (e.g. `axil`).
        let mut cur: &Expr = e;
        loop {
            match &*cur.kind {
                ExprKind::Field { target, .. } => cur = target,
                ExprKind::Ident(id) => {
                    return self.pointer_vars.contains(&id.name)
                        || self.bus_bindings.contains_key(&id.name);
                }
                _ => return false,
            }
        }
    }
}
