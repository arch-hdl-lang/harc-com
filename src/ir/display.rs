//! Textual form of the TB-IR, roughly matching the worked examples in
//! `docs/tb-ir-design.md`. Deterministic (stable ordering, no hashes)
//! so `harc dump-ir` output can be snapshot-tested; not reparseable —
//! the IR has no surface syntax.

use super::*;
use std::fmt::{self, Display, Formatter, Write as _};

impl Display for TbProgram {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "tb-ir program")?;
        for (i, tb) in self.testbenches.iter().enumerate() {
            write!(
                f,
                "  testbench tb{} {}{} {{ {}: {} }}",
                i,
                tb.name,
                if tb.synthetic { " (synthetic)" } else { "" },
                tb.dut_field,
                tb.dut_type
            )?;
            for (field, cov) in &tb.cov_fields {
                write!(f, " cov {field}=cg{}", cov.0)?;
            }
            for field in &tb.state_fields {
                match field {
                    TbStateFieldSchema::Scalar(sf) => {
                        write!(f, " field {}:{}={}", sf.name, type_str(&sf.ty), sf.default)?;
                    }
                    TbStateFieldSchema::Queue(qf) => {
                        let elem = match &qf.elem {
                            QueueElem::Scalar { ty } => type_str(ty),
                            QueueElem::Record(r) => self.records[r.index()].name.clone(),
                        };
                        write!(f, " queue {}:{elem}", qf.name)?;
                    }
                }
            }
            for edge in &tb.connects {
                let sink = match &edge.sink {
                    ConnectSink::Method { method } => method,
                    ConnectSink::Event { event } => event,
                };
                write!(
                    f,
                    " connect {}.{}->{}.{}",
                    edge.src_path.join("."),
                    edge.src_event,
                    edge.sink_path.join("."),
                    sink,
                )?;
            }
            for b in &tb.bus_bindings {
                write!(f, " bus {}={}", b.field, b.bus)?;
                if !b.methods.is_empty() {
                    let ms = b
                        .methods
                        .iter()
                        .map(|m| {
                            format!(
                                "{}({}){}",
                                m.name,
                                m.args.join(","),
                                if m.has_ret { "->r" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    write!(f, "[{ms}]")?;
                }
                if !b.remap.is_empty() {
                    let rs = b
                        .remap
                        .iter()
                        .map(|((ch, sig), port)| format!("{ch}.{sig}={port}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    write!(f, " with{{{rs}}}")?;
                }
            }
            for (field, x) in &tb.transactor_fields {
                write!(f, " xactor {field}=x{}", x.0)?;
            }
            for binding in &tb.component_fields {
                let mode = match binding.mode {
                    Some(ComponentInstanceMode::Active) => " active",
                    Some(ComponentInstanceMode::Passive) => " passive",
                    None => "",
                };
                write!(
                    f,
                    " component {}=c{}{}",
                    binding.field, binding.component.0, mode
                )?;
            }
            for b in &tb.regblock_bindings {
                write!(
                    f,
                    " regblock {}=rb{} via {}",
                    b.field, b.regblock.0, b.helper_field
                )?;
                for (reg, fid) in &b.callbacks {
                    write!(f, " [on {reg}=fn{}]", fid.0)?;
                }
            }
            for svc in &tb.periodic_services {
                let phase = match svc.phase {
                    crate::ir::HandlerPhase::Checker => "checker",
                    crate::ir::HandlerPhase::PostEval => "post_eval",
                };
                write!(
                    f,
                    " periodic every {} cyc phase {phase} fn{}",
                    svc.period, svc.function.0
                )?;
            }
            writeln!(f)?;
        }
        for t in &self.tests {
            write!(
                f,
                "  test {} tb=tb{} run=fn{}",
                t.name, t.testbench.0, t.run.0
            )?;
            match t.check {
                Some(c) => write!(f, " check=fn{}", c.0)?,
                None => write!(f, " check=-")?,
            }
            for c in &t.clocks {
                write!(f, " clock={}@{}ps", c.name, c.period_ps)?;
                if let Some(d) = &c.domain {
                    write!(f, "({d})")?;
                }
            }
            writeln!(f)?;
        }
        for (i, r) in self.records.iter().enumerate() {
            writeln!(f, "  record r{} {}", i, r.name)?;
            for fld in &r.fields {
                let ty = match fld.vec_len {
                    Some(n) => format!("Vec<{}, {n}>", type_str(&fld.ty)),
                    None => type_str(&fld.ty),
                };
                write!(
                    f,
                    "    {}{} : {ty}",
                    if fld.non_random { "!" } else { "" },
                    fld.name,
                )?;
                if let Some(d) = fld.default {
                    if fld.ty == IrType::Bool {
                        write!(f, " default {}", d != 0)?;
                    } else {
                        write!(f, " default {d}")?;
                    }
                }
                for a in &fld.attr_src {
                    write!(f, " {a} (inert)")?;
                }
                writeln!(f)?;
            }
            for k in &r.keeps {
                writeln!(f, "    keep {k} (inert)")?;
            }
        }
        for (i, x) in self.transactors.iter().enumerate() {
            match &x.bound_bus {
                Some(bus) => {
                    writeln!(f, "  transactor x{} {} bound to {}", i, x.name, bus)?;
                }
                None => {
                    writeln!(
                        f,
                        "  transactor x{} {} {{ {}: {} }}",
                        i, x.name, x.dut_field, x.dut_type
                    )?;
                }
            }
            for sf in &x.state_fields {
                match &sf.kind {
                    crate::ir::StateFieldKind::Scalar { default, .. } => {
                        writeln!(f, "    state {} = {}", sf.name, default)?;
                    }
                    crate::ir::StateFieldKind::Queue { elem } => {
                        let e = match elem {
                            crate::ir::QueueElem::Scalar { ty } => type_str(ty),
                            crate::ir::QueueElem::Record(r) => format!("rec{}", r.index()),
                        };
                        writeln!(f, "    state {} : queue<{}>", sf.name, e)?;
                    }
                    crate::ir::StateFieldKind::Record { record } => {
                        writeln!(f, "    state {} : rec{}", sf.name, record.index())?;
                    }
                }
            }
            for m in &x.methods {
                writeln!(
                    f,
                    "    method {}({} arg{}){} = fn{}",
                    m.name,
                    m.param_names.len(),
                    if m.param_names.len() == 1 { "" } else { "s" },
                    if m.has_ret { " -> ret" } else { "" },
                    m.function.0
                )?;
            }
            for tm in &x.target_methods {
                writeln!(
                    f,
                    "    target thread bus.{}({} arg{}){}{} = fn{}",
                    tm.name,
                    tm.args.len(),
                    if tm.args.len() == 1 { "" } else { "s" },
                    if tm.has_ret { " -> ret" } else { "" },
                    match tm.ooo_tags {
                        Some(n) => format!(" ooo tags {n}"),
                        None => String::new(),
                    },
                    tm.function.0
                )?;
            }
        }
        for (i, rb) in self.regblocks.iter().enumerate() {
            writeln!(f, "  regblock rb{} {} mirror=r{}", i, rb.name, rb.record.0)?;
            for reg in &rb.registers {
                writeln!(
                    f,
                    "    register {} @ 0x{:x} width {} access {}",
                    reg.name,
                    reg.offset,
                    reg.width,
                    reg.access.keyword()
                )?;
                for fld in &reg.fields {
                    writeln!(
                        f,
                        "      field {} [{}+:{}] access {}",
                        fld.name,
                        fld.bit_pos,
                        fld.bit_width,
                        fld.access.keyword()
                    )?;
                }
            }
        }
        for (i, c) in self.components.iter().enumerate() {
            writeln!(f, "  component c{} {} ({})", i, c.name, c.kind.keyword())?;
            for fld in &c.fields {
                use crate::ir::ComponentFieldKind;
                let desc = match &fld.kind {
                    ComponentFieldKind::Scalar { default, .. } => {
                        format!("scalar = {default}")
                    }
                    ComponentFieldKind::FixedVec(vec) => {
                        format!("fixed-vec<{:?}, {}>", vec.elem, vec.len)
                    }
                    ComponentFieldKind::Record { record } => {
                        format!("record {}", self.records[record.index()].name)
                    }
                    ComponentFieldKind::Queue { elem } => {
                        use crate::ir::QueueElem;
                        let inner = match elem {
                            QueueElem::Scalar { ty } => type_str(ty),
                            QueueElem::Record(r) => self.records[r.index()].name.clone(),
                        };
                        format!("queue<{inner}>")
                    }
                    ComponentFieldKind::Event { payload } => {
                        use crate::ir::EventPayload;
                        let inner = match payload {
                            EventPayload::Scalar { signed: true } => "sint".to_string(),
                            EventPayload::Scalar { signed: false } => "uint".to_string(),
                            EventPayload::Record(r) => self.records[r.index()].name.clone(),
                        };
                        format!("out event<{inner}>")
                    }
                    ComponentFieldKind::Sub { component, mode } => format!(
                        "sub c{}{}",
                        component.0,
                        match mode {
                            Some(ComponentInstanceMode::Active) => " active",
                            Some(ComponentInstanceMode::Passive) => " passive",
                            None => "",
                        }
                    ),
                    ComponentFieldKind::Dut { dut_type } => format!("dut {dut_type}"),
                    ComponentFieldKind::ScoreboardSub { scoreboard } => {
                        format!("sub scoreboard sb{}", scoreboard.0)
                    }
                };
                let activation = match fld.activation {
                    crate::ir::Activation::Always => "",
                    crate::ir::Activation::ActiveOnly => " active-only",
                };
                writeln!(f, "    field {}{activation} : {desc}", fld.name)?;
            }
            for m in &c.methods {
                writeln!(
                    f,
                    "    method {}{}({} arg{}){} = fn{}",
                    m.name,
                    match m.activation {
                        crate::ir::Activation::Always => "",
                        crate::ir::Activation::ActiveOnly => " active-only",
                    },
                    m.param_names.len(),
                    if m.param_names.len() == 1 { "" } else { "s" },
                    if m.has_ret { " -> ret" } else { "" },
                    m.function.0
                )?;
            }
            for e in &c.connects {
                let sink = match &e.sink {
                    crate::ir::ConnectSink::Method { method } => format!("{method} (method)"),
                    crate::ir::ConnectSink::Event { event } => format!("{event} (event)"),
                };
                // Through the same labeller the diagnostics use: an
                // owner-relative endpoint has an EMPTY path, and a naive
                // join renders it `.own_ev`.
                writeln!(
                    f,
                    "    connect {} -> {} (c{})",
                    crate::ir::lower::endpoint_label(&e.src_path, &e.src_event),
                    crate::ir::lower::endpoint_label(&e.sink_path, &sink),
                    e.sink_component.0
                )?;
            }
            for ph in &c.periodic_handlers {
                // Bodies print as their own `fn` blocks below; the
                // summary line records the period source and phase.
                let phase = match ph.phase {
                    crate::ir::HandlerPhase::Checker => "",
                    crate::ir::HandlerPhase::PostEval => " phase post_eval",
                };
                writeln!(
                    f,
                    "    on {} cycles{phase}{} = fn{}",
                    expr_str_for_component(self, ph.function, &ph.period),
                    match ph.activation {
                        crate::ir::Activation::Always => "",
                        crate::ir::Activation::ActiveOnly => " active-only",
                    },
                    ph.function.0
                )?;
            }
            for ch in &c.cycle_handlers {
                let edge = match ch.edge {
                    crate::ir::CycleEdge::Rising => "rising",
                    crate::ir::CycleEdge::Falling => "falling",
                    crate::ir::CycleEdge::Level => "level",
                };
                if let Some(channel) = &ch.monitor_channel {
                    writeln!(
                        f,
                        "    on bus.{channel}.handshake [{}] ({edge}){} = fn{}",
                        expr_str_for_component(self, ch.function, &ch.trigger),
                        match ch.activation {
                            crate::ir::Activation::Always => "",
                            crate::ir::Activation::ActiveOnly => " active-only",
                        },
                        ch.function.0
                    )?;
                } else {
                    writeln!(
                        f,
                        "    on {} ({edge}){} = fn{}",
                        expr_str_for_component(self, ch.function, &ch.trigger),
                        match ch.activation {
                            crate::ir::Activation::Always => "",
                            crate::ir::Activation::ActiveOnly => " active-only",
                        },
                        ch.function.0
                    )?;
                }
            }
            if let Some(w) = &c.watchdog {
                let period = w
                    .period
                    .as_ref()
                    .map(|e| expr_str_for_component(self, w.function, e))
                    .unwrap_or_else(|| "default".to_string());
                let max_idle = w
                    .max_idle
                    .as_ref()
                    .map(|e| expr_str_for_component(self, w.function, e))
                    .unwrap_or_else(|| "default".to_string());
                writeln!(
                    f,
                    "    watchdog period {period} max_idle {max_idle}{} = fn{}",
                    match w.activation {
                        crate::ir::Activation::Always => "",
                        crate::ir::Activation::ActiveOnly => " active-only",
                    },
                    w.function.0
                )?;
            }
        }
        for (i, cg) in self.covgroups.iter().enumerate() {
            let trig = match &cg.trigger {
                CovTrigger::PosedgeDutClk => "@posedge(dut.clk)".to_string(),
                CovTrigger::Hook {
                    receiver_path,
                    method,
                    param_names,
                    side,
                } => format!(
                    "@({}.{}({}) {})",
                    receiver_path.join("."),
                    method,
                    param_names.join(", "),
                    match side {
                        crate::ast::HookSide::Pre => "pre",
                        crate::ast::HookSide::Post => "post",
                    }
                ),
            };
            writeln!(f, "  covgroup cg{} {} {}", i, cg.name, trig)?;
            for p in &cg.points {
                write!(f, "    point {} <- {}:", p.name, cover_expr_str(&p.target))?;
                for b in &p.bins {
                    let vals = b
                        .values
                        .iter()
                        .map(bin_value_str)
                        .collect::<Vec<_>>()
                        .join(",");
                    write!(f, " {}={{{vals}}}", b.name)?;
                }
                writeln!(f)?;
            }
            for c in &cg.crosses {
                let names = c
                    .point_indices
                    .iter()
                    .map(|&i| cg.points[i].name.as_str())
                    .collect::<Vec<_>>()
                    .join(" x ");
                writeln!(f, "    cross {names}")?;
            }
        }
        for (i, p) in self.property_checks.iter().enumerate() {
            let sev = match p.severity {
                crate::ir::PropertySeverity::Fail => "assert",
                crate::ir::PropertySeverity::AssumeFail => "assume",
            };
            let body = match &p.shape {
                crate::ir::PropertyShape::Implies { ante, cons } => {
                    format!("{} |-> {}", check_expr_str(ante), check_expr_str(cons))
                }
                crate::ir::PropertyShape::ImpliesNext { ante, cons } => {
                    format!("{} |=> {}", check_expr_str(ante), check_expr_str(cons))
                }
                crate::ir::PropertyShape::Invariant(e) => check_expr_str(e),
            };
            writeln!(f, "  property p{i} {sev} `{}` [{}] {body}", p.label, p.tag)?;
            if let Some(m) = &p.message {
                writeln!(f, "    else fail \"{}\"", m.fmt.escape_debug())?;
            }
            for (si, slot) in p.temporals.iter().enumerate() {
                writeln!(f, "    latch #{si} <- {}", check_expr_str(&slot.inner))?;
            }
        }
        for (i, c) in self.cover_checks.iter().enumerate() {
            writeln!(
                f,
                "  cover c{i} `{}` [{}] {}",
                c.label,
                c.tag,
                check_expr_str(&c.cond)
            )?;
            for (si, slot) in c.temporals.iter().enumerate() {
                writeln!(f, "    latch #{si} <- {}", check_expr_str(&slot.inner))?;
            }
        }
        for func in &self.functions {
            writeln!(f)?;
            write!(f, "{func}")?;
        }
        Ok(())
    }
}

/// Render a concurrent-check body expression. Check bodies live outside
/// any `TbFunction`, so they carry no local table — `expr_str` needs one
/// to name locals, and a check body has none by construction (its
/// operands are ports, host state, constants, and latch readings).
fn check_expr_str(e: &Expr) -> String {
    expr_str(&EMPTY_CHECK_SCOPE, e)
}

/// An empty function used purely as the (unused) local-name scope for
/// concurrent-check body rendering.
static EMPTY_CHECK_SCOPE: std::sync::LazyLock<TbFunction> =
    std::sync::LazyLock::new(|| TbFunction {
        id: crate::ir::FunctionId(u32::MAX),
        name: "<check>".to_string(),
        kind: crate::ir::FunctionKind::Run,
        owner: None,
        params: Vec::new(),
        locals: Vec::new(),
        blocks: Vec::new(),
        entry: crate::ir::BlockId(0),
        ret: None,
        implicit_returns: Vec::new(),
    });

impl Display for TbFunction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "fn fn{} {} [kind={}",
            self.id.0,
            self.name,
            kind_str(&self.kind)
        )?;
        if let Some(owner) = self.owner {
            write!(f, ", owner=tb{}", owner.0)?;
        }
        writeln!(f, "]")?;
        if !self.params.is_empty() {
            writeln!(f, "  params:")?;
            for p in &self.params {
                writeln!(f, "    {} : {}", p.name, type_str(&p.ty))?;
            }
        }
        if !self.locals.is_empty() {
            writeln!(f, "  locals:")?;
            for (i, l) in self.locals.iter().enumerate() {
                writeln!(f, "    %{i} {} : {}", l.name, type_str(&l.ty))?;
            }
        }
        if let Some(r) = self.ret {
            writeln!(f, "  ret = %{}", self.local(r).name)?;
        }
        writeln!(f, "  entry = b{}", self.entry.0)?;
        for (i, b) in self.blocks.iter().enumerate() {
            writeln!(f)?;
            writeln!(f, "  b{i}:")?;
            for s in &b.stmts {
                writeln!(f, "    {}", stmt_str(self, s))?;
            }
            writeln!(f, "    -> {}", term_str(self, &b.terminator))?;
        }
        Ok(())
    }
}

fn kind_str(k: &FunctionKind) -> String {
    match k {
        FunctionKind::Run => "Run".to_string(),
        FunctionKind::Check => "Check".to_string(),
        FunctionKind::SamplerAuto { covgroup } => format!("SamplerAuto(cg{})", covgroup.0),
        FunctionKind::Helper => "Helper".to_string(),
        FunctionKind::TransactorBody { transactor } => {
            format!("TransactorBody(x{})", transactor.0)
        }
        FunctionKind::ComponentMethod { component } => {
            format!("ComponentMethod(c{})", component.0)
        }
        FunctionKind::Tseq { elem } => match elem {
            crate::ir::TseqElem::Record(r) => format!("Tseq(r{})", r.0),
            crate::ir::TseqElem::Scalar(t) => format!("Tseq({})", type_str(t)),
        },
        FunctionKind::TestHook => "TestHook".to_string(),
    }
}

fn type_str(t: &IrType) -> String {
    match t {
        IrType::UInt(Some(w)) => format!("uint<{w}>"),
        IrType::UInt(None) => "uint".to_string(),
        IrType::SInt(Some(w)) => format!("sint<{w}>"),
        IrType::SInt(None) => "sint".to_string(),
        IrType::Event(p) => match p {
            EventPayload::Scalar { signed: true } => "event<sint>".to_string(),
            EventPayload::Scalar { signed: false } => "event<uint>".to_string(),
            EventPayload::Record(r) => format!("event<r{}>", r.0),
        },
        IrType::Bool => "bool".to_string(),
        IrType::Record(r) => format!("record(r{})", r.0),
        IrType::RecordSeq(r) => format!("seq(r{})", r.0),
        IrType::Seq(t) => format!("seq({})", type_str(t)),
        IrType::Component(c) => format!("component(c{})", c.0),
        IrType::Unknown => "unknown".to_string(),
    }
}

fn stmt_str(func: &TbFunction, s: &Stmt) -> String {
    match s {
        Stmt::Assign(l, e) => format!("Assign({}, {})", local_str(func, *l), expr_str(func, e)),
        Stmt::DutWrite(p, e) => format!(
            "DutWrite({}, {})",
            port_str(Some(func), p),
            expr_str(func, e)
        ),
        Stmt::DutRead(l, p) => format!(
            "DutRead({}, {})",
            local_str(func, *l),
            port_str(Some(func), p)
        ),
        Stmt::ProbeRelease(p) => format!("ProbeRelease({})", port_str(Some(func), p)),
        Stmt::RecordInit(l, r) => format!("RecordInit({}, r{})", local_str(func, *l), r.0),
        Stmt::RecordFieldWrite {
            local,
            field,
            path,
            mid_indices,
            index,
            value,
        } => {
            let chain = record_chain_str(func, field, path, mid_indices, index.as_ref());
            format!(
                "RecordFieldWrite({}.{chain}, {})",
                local_str(func, *local),
                expr_str(func, value)
            )
        }
        Stmt::RecordWriteCb {
            local,
            binding,
            field,
            offset,
            value,
            callback,
        } => {
            let cb = match callback {
                Some(fid) => format!(", cb=fn{}", fid.0),
                None => String::new(),
            };
            format!(
                "RecordWriteCb({}.{field}@0x{offset:x}, {}, depth={binding}_cb_depth{cb})",
                local_str(func, *local),
                expr_str(func, value)
            )
        }
        Stmt::TbFieldWrite { field, value } => {
            format!("TbFieldWrite(_tb.{field}, {})", expr_str(func, value))
        }
        Stmt::TbQueuePush { field, value } => {
            format!("TbQueuePush(_tb.{field}, {})", expr_str(func, value))
        }
        Stmt::TbQueuePop { field, dest } => {
            format!("TbQueuePop({} = _tb.{field}.pop())", local_str(func, *dest))
        }
        Stmt::TransactorStateWrite {
            instance,
            field,
            value,
        } => {
            format!(
                "TransactorStateWrite({instance}.{field}, {})",
                expr_str(func, value)
            )
        }
        Stmt::TransactorStateRecordFieldWrite {
            instance,
            field,
            path,
            mid_indices,
            index,
            value,
        } => {
            let chain = record_chain_str(func, &path[0], &path[1..], mid_indices, index.as_ref());
            format!(
                "TransactorStateRecordFieldWrite({instance}.{field}.{chain}, {})",
                expr_str(func, value)
            )
        }
        Stmt::TransactorStateQueuePush {
            instance,
            field,
            value,
        } => {
            format!(
                "TransactorStateQueuePush({instance}.{field}, {})",
                expr_str(func, value)
            )
        }
        Stmt::TransactorStateQueuePop {
            instance,
            field,
            dest,
        } => {
            format!(
                "TransactorStateQueuePop({}, {instance}.{field})",
                local_str(func, *dest)
            )
        }
        Stmt::Log { level, args } => {
            format!("Log({}, {})", level_str(level), fmt_args_str(func, args))
        }
        Stmt::AssumeCheck { cond, on_fail } => format!(
            "AssumeCheck {{ cond: {}, on_fail: {} }}",
            expr_str(func, cond),
            fmt_args_str(func, on_fail)
        ),
        Stmt::AssertCheck { cond, on_fail } => format!(
            "AssertCheck {{ cond: {}, on_fail: {} }}",
            expr_str(func, cond),
            fmt_args_str(func, on_fail)
        ),
        Stmt::CovReport(inst) => {
            format!("CovReport({}.cg{})", inst.tb_field, inst.covgroup.0)
        }
        Stmt::PropertyCheck(p) => format!("PropertyCheck(p{})", p.0),
        Stmt::CoverCheck(c) => format!("CoverCheck(c{})", c.0),
        Stmt::CycleHandler(h) => format!("CycleHandler(h{})", h.0),
        Stmt::EventSubscribe { event, handler } => {
            let event = match event {
                crate::ir::EventChannelRef::Local(event) => local_str(func, *event),
                crate::ir::EventChannelRef::Component { base, event, .. } => {
                    format!("{}.{event}", comp_base_str(base))
                }
            };
            format!("EventSubscribe({event} <- fn{})", handler.0)
        }
        Stmt::MethodHookSubscribe {
            target,
            side,
            handler,
            captures,
        } => {
            let target = match target {
                crate::ir::MethodHookTarget::Transactor { field, method, .. } => {
                    format!("{field}.{method}")
                }
                crate::ir::MethodHookTarget::Component { base, method, .. } => {
                    format!("{}.{method}", comp_base_str(base))
                }
            };
            let captures = captures
                .iter()
                .map(|local| local_str(func, *local))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "MethodHookSubscribe({target} {side:?} <- fn{} captures=[{captures}])",
                handler.0
            )
        }
        Stmt::EventEmit { event, args } => format!(
            "EventEmit({}({}))",
            local_str(func, *event),
            args.iter()
                .map(|a| expr_str(func, a))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Stmt::TransactorCall { dest, call } => match dest {
            Some(d) => format!(
                "TransactorCall({} = {})",
                local_str(func, *d),
                expr_str(func, call)
            ),
            None => format!("TransactorCall({})", expr_str(func, call)),
        },
        Stmt::TransactorSelfCall { dest, call } => match dest {
            Some(d) => format!(
                "TransactorSelfCall({} = {})",
                local_str(func, *d),
                expr_str(func, call)
            ),
            None => format!("TransactorSelfCall({})", expr_str(func, call)),
        },
        Stmt::FailDiag { guard, args } => match guard {
            Some(g) => format!(
                "FailDiag {{ unless: {}, {} }}",
                expr_str(func, g),
                fmt_args_str(func, args)
            ),
            None => format!("FailDiag {{ {} }}", fmt_args_str(func, args)),
        },
        Stmt::ScoreboardOp {
            sb,
            field,
            op,
            nested_path,
        } => {
            let access = match nested_path {
                Some(p) => p.join("."),
                None => format!("sb{}.{field}", sb.0),
            };
            format!("ScoreboardOp({access}, {})", sb_op_str(func, op))
        }
        Stmt::ComponentFieldWrite { base, field, value } => format!(
            "ComponentFieldWrite({}.{field}, {})",
            comp_base_str(base),
            expr_str(func, value)
        ),
        Stmt::ComponentVecElementWrite {
            base,
            field,
            index_pos,
            index,
            value,
        } => {
            let field = indexed_chain_str(func, field, *index_pos, index);
            format!(
                "ComponentVecElementWrite({}.{field}, {})",
                comp_base_str(base),
                expr_str(func, value)
            )
        }
        Stmt::ComponentEmit { base, event, args } => {
            let a: Vec<String> = args.iter().map(|e| expr_str(func, e)).collect();
            format!(
                "ComponentEmit({}.{event}, [{}])",
                comp_base_str(base),
                a.join(", ")
            )
        }
        Stmt::ComponentCall {
            base,
            component,
            method,
            args,
            dest,
        } => {
            let a: Vec<String> = args.iter().map(|e| expr_str(func, e)).collect();
            let call = format!(
                "{}.c{}::{method}([{}])",
                comp_base_str(base),
                component.0,
                a.join(", ")
            );
            match dest {
                Some(d) => format!("ComponentCall({} = {call})", local_str(func, *d)),
                None => format!("ComponentCall({call})"),
            }
        }
        Stmt::ComponentQueuePush { base, queue, value } => format!(
            "ComponentQueuePush({}.{queue}, {})",
            comp_base_str(base),
            expr_str(func, value)
        ),
        Stmt::ComponentQueuePop { base, queue, dest } => format!(
            "ComponentQueuePop({} = {}.{queue}.pop())",
            local_str(func, *dest),
            comp_base_str(base)
        ),
        Stmt::ComponentSubAssign { dst, field, src } => format!(
            "ComponentSubAssign({}.{field} = {})",
            comp_base_str(dst),
            comp_base_str(src)
        ),
        Stmt::SeqPush { seq, value } => format!(
            "SeqPush({}, {})",
            local_str(func, *seq),
            expr_str(func, value)
        ),
        Stmt::TlmFork(desc) => format!("TlmFork({})", tlm_fork_desc_str(func, desc)),
        Stmt::TlmJoinAll(pending) => {
            let descs: Vec<String> = pending.iter().map(|d| tlm_fork_desc_str(func, d)).collect();
            format!("TlmJoinAll([{}])", descs.join(", "))
        }
    }
}

fn tlm_fork_desc_str(func: &crate::ir::TbFunction, desc: &crate::ir::TlmForkDesc) -> String {
    let a: Vec<String> = desc.args.iter().map(|e| expr_str(func, e)).collect();
    let dest = match desc.dest {
        Some(d) => format!("{} = ", local_str(func, d)),
        None => String::new(),
    };
    let tag = match desc.tag {
        Some(t) => format!(" tag={t}"),
        None => String::new(),
    };
    format!(
        "{dest}{}.{}([{}]){tag}",
        desc.bus_field,
        desc.method,
        a.join(", ")
    )
}

fn comp_base_str(base: &crate::ir::ComponentBase) -> String {
    use crate::ir::ComponentBase;
    match base {
        ComponentBase::SelfField => "self".to_string(),
        ComponentBase::Path(path) => path.join("."),
        ComponentBase::Local(l) => format!("local(l{})", l.0),
    }
}

fn sb_op_str(func: &TbFunction, op: &crate::ir::ScoreboardOp) -> String {
    use crate::ir::ScoreboardOp;
    match op {
        ScoreboardOp::QueuePush { queue, value } => {
            format!("{queue}.push({})", expr_str(func, value))
        }
        ScoreboardOp::QueuePop { queue, dest } => {
            format!("{} = {queue}.pop()", local_str(func, *dest))
        }
        ScoreboardOp::ScalarWrite { scalar, value } => {
            format!("{scalar} = {}", expr_str(func, value))
        }
    }
}

fn sb_query_str(query: &crate::ir::ScoreboardQuery) -> String {
    use crate::ir::ScoreboardQuery;
    match query {
        ScoreboardQuery::Scalar { scalar } => scalar.clone(),
        ScoreboardQuery::QueueSize { queue } => format!("{queue}.size()"),
        ScoreboardQuery::QueueEmpty { queue } => format!("{queue}.empty()"),
    }
}

fn term_str(func: &TbFunction, t: &Terminator) -> String {
    match t {
        Terminator::Jump(b) => format!("Jump(b{})", b.0),
        Terminator::Branch(c, bt, bf) => {
            format!("Branch({}, b{}, b{})", expr_str(func, c), bt.0, bf.0)
        }
        Terminator::WaitCycles(e, clock, b) => match clock {
            Some(c) => format!("WaitCycles({} on {}, b{})", expr_str(func, e), c.name, b.0),
            None => format!("WaitCycles({}, b{})", expr_str(func, e), b.0),
        },
        Terminator::WaitCyclesSync(e, b) => {
            format!("WaitCyclesSync({}, b{})", expr_str(func, e), b.0)
        }
        Terminator::WaitTimePs(ps, b) => format!("WaitTimePs({ps}, b{})", b.0),
        Terminator::WaitUntil { preds, mode, succ } => format!(
            "WaitUntil {{ {} [{}], b{} }}",
            preds_str(func, preds),
            mode_str(mode),
            succ.0
        ),
        Terminator::WaitUntilTimeout {
            preds,
            mode,
            cycles,
            on_fire,
            on_timeout,
        } => format!(
            "WaitUntilTimeout {{ {} [{}], cycles: {}, fire: b{}, timeout: b{} }}",
            preds_str(func, preds),
            mode_str(mode),
            expr_str(func, cycles),
            on_fire.0,
            on_timeout.0
        ),
        Terminator::Randomize {
            target,
            constraints,
            succ,
        } => format!(
            "Randomize {{ {}, constraints: c{}, b{} }}",
            local_str(func, *target),
            constraints.0,
            succ.0
        ),
        Terminator::Return => "Return".to_string(),
        Terminator::Fatal(args) => format!("Fatal({})", fmt_args_str(func, args)),
    }
}

fn preds_str(func: &TbFunction, preds: &[PredSrc]) -> String {
    preds
        .iter()
        .map(|p| expr_str(func, &p.expr))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn mode_str(m: &WaitMode) -> &'static str {
    match m {
        WaitMode::Single => "single",
        WaitMode::AllOf => "all_of",
        WaitMode::AnyOf => "any_of",
    }
}

fn bin_value_str(v: &CovBinValue) -> String {
    fn bound_str(b: &CovBinBound) -> String {
        match b {
            CovBinBound::Const(x) => x.to_string(),
            CovBinBound::Runtime(e) => cover_expr_str(e),
        }
    }
    match v {
        CovBinValue::Eq(x) => bound_str(x),
        CovBinValue::Range { lo, hi } => format!(
            "[{}..{}]",
            lo.as_ref().map(bound_str).unwrap_or_default(),
            hi.as_ref().map(bound_str).unwrap_or_default()
        ),
    }
}

fn level_str(l: &LogLevel) -> String {
    match l {
        LogLevel::Debug => "debug".to_string(),
        LogLevel::Info => "info".to_string(),
        LogLevel::Warn => "warn".to_string(),
        LogLevel::Error => "error".to_string(),
        LogLevel::Fatal => "fatal".to_string(),
        LogLevel::File { path, level } => {
            let lv = match level {
                FileLogLevel::Debug => "debug",
                FileLogLevel::Info => "info",
                FileLogLevel::Warn => "warn",
                FileLogLevel::Error => "error",
                FileLogLevel::Fatal => "fatal",
            };
            format!("file({path:?}, {lv})")
        }
    }
}

fn fmt_args_str(func: &TbFunction, a: &FmtArgs) -> String {
    let mut out = String::new();
    write!(out, "{:?}", a.fmt).ok();
    for arg in &a.args {
        write!(out, ", {}", expr_str(func, &arg.expr)).ok();
        if arg.wide_hex.is_some() {
            out.push_str(" (wide-hex)");
        }
    }
    out
}

fn local_str(func: &TbFunction, l: LocalId) -> String {
    // A concurrent-check body is rendered outside any function's local
    // table (its operands are ports, host state, and — legitimately —
    // locals of whatever function registered it), so fall back to the
    // raw id rather than indexing a scope that does not hold it.
    match func.locals.get(l.index()) {
        Some(t) => format!("%{}", t.name),
        None => format!("%#{}", l.0),
    }
}

/// Render a record-field chain `field[.path…]` with its element
/// selections: a `[idx]` after any mid-chain `Vec<Record, N>` segment
/// (`entries[%i].tag`) and after the leaf when `index` is `Some`.
fn record_chain_str(
    func: &TbFunction,
    field: &str,
    path: &[String],
    mid_indices: &[(usize, Expr)],
    index: Option<&Expr>,
) -> String {
    let mut out = String::from(field);
    for (pos, seg) in std::iter::once(None)
        .chain(path.iter().map(Some))
        .enumerate()
    {
        if let Some(seg) = seg {
            out.push('.');
            out.push_str(seg);
        }
        for (_, idx) in mid_indices.iter().filter(|(p, _)| *p == pos) {
            out.push_str(&format!("[{}]", expr_str(func, idx)));
        }
    }
    if let Some(idx) = index {
        out.push_str(&format!("[{}]", expr_str(func, idx)));
    }
    out
}

fn indexed_chain_str(func: &TbFunction, field: &str, index_pos: usize, index: &Expr) -> String {
    let mut out = String::new();
    for (pos, segment) in field.split('.').enumerate() {
        if pos != 0 {
            out.push('.');
        }
        out.push_str(segment);
        if pos == index_pos {
            out.push_str(&format!("[{}]", expr_str(func, index)));
        }
    }
    out
}

fn port_str(func: Option<&TbFunction>, p: &PortRef) -> String {
    let mut out = format!("{}.{}", p.testbench_field, p.port_path.join("."));
    match &p.lane {
        None => {}
        Some(crate::ir::LaneIndex::Const(c)) => out.push_str(&format!("[{c}]")),
        Some(crate::ir::LaneIndex::Var(e)) => {
            let idx = func.map_or_else(|| cover_expr_str(e), |f| expr_str(f, e));
            out.push_str(&format!("[{idx}]"));
        }
    }
    match p.access {
        PortAccess::Port => {}
        PortAccess::Probe => out.push_str(" (probe)"),
        PortAccess::Force => out.push_str(" (force)"),
    }
    out
}

/// Render a period/max_idle clause expr (which lowered in the same
/// builder as the component body `function`) using that function for any
/// local-name resolution. Field reads render as `self.<field>`; literals
/// and `cycle_count` need no function context.
fn expr_str_for_component(
    prog: &crate::ir::TbProgram,
    function: crate::ir::FunctionId,
    e: &Expr,
) -> String {
    expr_str(prog.function(function), e)
}

pub(crate) fn expr_str(func: &TbFunction, e: &Expr) -> String {
    match e {
        Expr::Literal { value, .. } => format!("{value}"),
        Expr::CycleCount => "cycle_count".to_string(),
        Expr::ErrorCount => "errors".to_string(),
        Expr::WideLiteral(words) => {
            // MSB-first hex dump of the word list (deterministic,
            // reparse-free — the IR has no surface syntax).
            let mut s = String::from("0x");
            for w in words.iter().rev() {
                s.push_str(&format!("{w:08x}"));
            }
            s
        }
        Expr::Local(l) => local_str(func, *l),
        Expr::Port(p) => port_str(Some(func), p),
        Expr::RecordField {
            local,
            field,
            path,
            mid_indices,
            index,
        } => {
            let chain = record_chain_str(func, field, path, mid_indices, index.as_deref());
            format!("{}.{chain}", local_str(func, *local))
        }
        Expr::RegRead {
            mirror,
            helper_ty,
            field,
            offset,
            reads_bus,
        } => {
            if *reads_bus {
                format!(
                    "RegRead({}.{field} = {helper_ty}.read({offset}))",
                    local_str(func, *mirror)
                )
            } else {
                format!("RegRead({}.{field})", local_str(func, *mirror))
            }
        }
        Expr::TbField(field) => format!("_tb.{field}"),
        Expr::TemporalSlot { slot, kind } => {
            let f = match kind {
                crate::ir::TemporalFn::Past => "past",
                crate::ir::TemporalFn::Rose => "rose",
                crate::ir::TemporalFn::Fell => "fell",
                crate::ir::TemporalFn::Stable => "stable",
            };
            format!("{f}(#{slot})")
        }
        Expr::TbQueueQuery { field, query } => match query {
            crate::ir::ScoreboardQuery::QueueSize { .. } => format!("_tb.{field}.size()"),
            crate::ir::ScoreboardQuery::QueueEmpty { .. } => format!("_tb.{field}.empty()"),
            crate::ir::ScoreboardQuery::Scalar { .. } => format!("_tb.{field}"),
        },
        Expr::TransactorState { instance, field } => format!("{instance}.{field}"),
        Expr::TransactorStateRecordField {
            instance,
            field,
            path,
            mid_indices,
            index,
        } => {
            let chain = record_chain_str(func, &path[0], &path[1..], mid_indices, index.as_deref());
            format!("{instance}.{field}.{chain}")
        }
        Expr::TransactorStateQueueQuery {
            instance,
            field,
            query,
        } => {
            let q = match query {
                crate::ir::ScoreboardQuery::QueueSize { .. } => "size",
                crate::ir::ScoreboardQuery::QueueEmpty { .. } => "empty",
                crate::ir::ScoreboardQuery::Scalar { .. } => "scalar",
            };
            format!("{instance}.{field}.{q}()")
        }
        Expr::ScoreboardQuery {
            sb,
            field,
            query,
            nested_path,
        } => {
            let access = match nested_path {
                Some(p) => p.join("."),
                None => format!("sb{}.{field}", sb.0),
            };
            format!("ScoreboardQuery({access}.{})", sb_query_str(query))
        }
        Expr::ComponentField { base, field } => {
            format!("{}.{field}", comp_base_str(base))
        }
        Expr::ComponentVecElement {
            base,
            field,
            index_pos,
            index,
        } => {
            let field = indexed_chain_str(func, field, *index_pos, index);
            format!("{}.{field}", comp_base_str(base))
        }
        Expr::ComponentValue { base } => {
            format!("ComponentValue({})", comp_base_str(base))
        }
        Expr::ComponentQueueQuery { base, query } => {
            format!(
                "ComponentQueueQuery({}.{})",
                comp_base_str(base),
                sb_query_str(query)
            )
        }
        Expr::ComponentIdle { base, kind, n } => {
            let m = match kind {
                crate::ir::IdleKind::In => "idle_in",
                crate::ir::IdleKind::Out => "idle_out",
                crate::ir::IdleKind::Both => "idle",
            };
            format!("{}.{m}({})", comp_base_str(base), expr_str(func, n))
        }
        Expr::TransactorIdle { field, kind, n, .. } => {
            let m = match kind {
                crate::ir::IdleKind::In => "idle_in",
                crate::ir::IdleKind::Out => "idle_out",
                crate::ir::IdleKind::Both => "idle",
            };
            format!("{field}.{m}({})", expr_str(func, n))
        }
        Expr::Binary(op, a, b) => format!(
            "({} {} {})",
            expr_str(func, a),
            bin_op_str(*op),
            expr_str(func, b)
        ),
        Expr::Unary(op, a) => format!("{}{}", un_op_str(*op), expr_str(func, a)),
        Expr::Ternary(c, t, e) => format!(
            "({} ? {} : {})",
            expr_str(func, c),
            expr_str(func, t),
            expr_str(func, e)
        ),
        Expr::BitSlice { target, hi, lo } => {
            format!("{}[{hi}:{lo}]", expr_str(func, target))
        }
        Expr::BitSliceDyn { target, hi, lo } => format!(
            "{}[{}:{}]",
            expr_str(func, target),
            expr_str(func, hi),
            expr_str(func, lo)
        ),
        Expr::WidthCast {
            kind,
            width,
            src_width,
            inner,
        } => {
            let k = match kind {
                WidthCastKind::Trunc => "trunc",
                WidthCastKind::Zext => "zext",
                WidthCastKind::Sext => "sext",
                WidthCastKind::Resize => "resize",
            };
            let sw = src_width.map(|w| format!(" from {w}")).unwrap_or_default();
            format!("{}.{k}<{width}{sw}>()", expr_str(func, inner))
        }
        Expr::CovBin { inst, point, bin } => {
            format!(
                "CovBin({}.cg{}, {point}, {bin})",
                inst.tb_field, inst.covgroup.0
            )
        }
        Expr::CovHookParam {
            param,
            field,
            index,
        } => match index {
            Some(i) => format!("CovHookParam({param}.{field}[{}])", expr_str(func, i)),
            None => format!("CovHookParam({param}.{field})"),
        },
        Expr::CovHookArg { param } => format!("CovHookArg({param})"),
        Expr::SeqLen(l) => format!("SeqLen({})", local_str(func, *l)),
        Expr::SeqIndex { seq, index } => {
            format!(
                "SeqIndex({}, {})",
                local_str(func, *seq),
                expr_str(func, index)
            )
        }
        Expr::Call(target, args) => {
            let t = match target {
                CallTarget::Helper { name, .. } => name.clone(),
                CallTarget::Builtin(n) => format!("builtin:{n}"),
                CallTarget::ExternFn { name, .. } => format!("extern:{name}"),
                CallTarget::TransactorMethod { bus_field, method } => {
                    format!("{bus_field}.{method}")
                }
                CallTarget::TransactorSelfMethod { transactor, method } => {
                    format!("self:{transactor}.{method}")
                }
                CallTarget::Tseq(n) => format!("tseq:{n}"),
            };
            let a = args
                .iter()
                .map(|x| expr_str(func, x))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{t}({a})")
        }
    }
}

fn cover_expr_str(e: &Expr) -> String {
    match e {
        Expr::Literal { value, .. } => value.to_string(),
        Expr::Port(p) => port_str(None, p),
        Expr::Binary(op, a, b) => {
            format!(
                "({} {} {})",
                cover_expr_str(a),
                bin_op_str(*op),
                cover_expr_str(b)
            )
        }
        Expr::Unary(op, a) => format!("{}{}", un_op_str(*op), cover_expr_str(a)),
        Expr::Ternary(c, t, f) => {
            format!(
                "({} ? {} : {})",
                cover_expr_str(c),
                cover_expr_str(t),
                cover_expr_str(f)
            )
        }
        Expr::BitSlice { target, hi, lo } => format!("{}[{hi}:{lo}]", cover_expr_str(target)),
        Expr::BitSliceDyn { target, hi, lo } => format!(
            "{}[{}:{}]",
            cover_expr_str(target),
            cover_expr_str(hi),
            cover_expr_str(lo)
        ),
        Expr::WidthCast {
            kind, width, inner, ..
        } => {
            let k = match kind {
                WidthCastKind::Trunc => "trunc",
                WidthCastKind::Zext => "zext",
                WidthCastKind::Sext => "sext",
                WidthCastKind::Resize => "resize",
            };
            format!("{}.{k}<{width}>()", cover_expr_str(inner))
        }
        Expr::CovHookArg { param } => param.clone(),
        Expr::CovHookParam {
            param,
            field,
            index,
        } => match index {
            Some(i) => format!("{param}.{field}[{}]", cover_expr_str(i)),
            None => format!("{param}.{field}"),
        },
        Expr::Call(target, args) => {
            let t = match target {
                CallTarget::Helper { name, .. } => name.clone(),
                CallTarget::ExternFn { name, .. } => format!("extern:{name}"),
                CallTarget::Builtin(n) => format!("builtin:{n}"),
                CallTarget::TransactorMethod { bus_field, method } => {
                    format!("{bus_field}.{method}")
                }
                CallTarget::TransactorSelfMethod { transactor, method } => {
                    format!("self:{transactor}.{method}")
                }
                CallTarget::Tseq(n) => format!("tseq:{n}"),
            };
            let a = args
                .iter()
                .map(cover_expr_str)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{t}({a})")
        }
        other => format!("{other:?}"),
    }
}

pub(crate) fn bin_op_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::BitAnd => "&",
        BinOp::BitOr => "|",
        BinOp::BitXor => "^",
        BinOp::Shl => "<<",
        BinOp::Shr => ">>",
    }
}

pub(crate) fn un_op_str(op: UnOp) -> &'static str {
    match op {
        UnOp::Neg => "-",
        UnOp::Not => "!",
        UnOp::BitNot => "~",
    }
}
