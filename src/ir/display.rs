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
            for sf in &tb.scalar_fields {
                write!(
                    f,
                    " field {}:{}={}",
                    sf.name,
                    type_str(&sf.ty),
                    sf.default
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
                writeln!(f, "    state {} = {}", sf.name, sf.default)?;
            }
            for m in &x.methods {
                let hooks = if m.pre_hooks.is_empty() && m.post_hooks.is_empty() {
                    String::new()
                } else {
                    let pre: Vec<String> =
                        m.pre_hooks.iter().map(|h| format!("fn{}", h.0)).collect();
                    let post: Vec<String> =
                        m.post_hooks.iter().map(|h| format!("fn{}", h.0)).collect();
                    format!(" [pre={}] [post={}]", pre.join(","), post.join(","))
                };
                writeln!(
                    f,
                    "    method {}({} arg{}){} = fn{}{hooks}",
                    m.name,
                    m.n_params,
                    if m.n_params == 1 { "" } else { "s" },
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
                    ComponentFieldKind::Queue { elem } => {
                        use crate::ir::QueueElem;
                        let inner = match elem {
                            QueueElem::Scalar { signed: true } => "sint".to_string(),
                            QueueElem::Scalar { signed: false } => "uint".to_string(),
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
                    ComponentFieldKind::Sub { component } => format!("sub c{}", component.0),
                    ComponentFieldKind::Dut { dut_type } => format!("dut {dut_type}"),
                    ComponentFieldKind::ScoreboardSub { scoreboard } => {
                        format!("sub scoreboard sb{}", scoreboard.0)
                    }
                };
                writeln!(f, "    field {} : {desc}", fld.name)?;
            }
            for m in &c.methods {
                writeln!(
                    f,
                    "    method {}({} arg{}){} = fn{}",
                    m.name,
                    m.n_params,
                    if m.n_params == 1 { "" } else { "s" },
                    if m.has_ret { " -> ret" } else { "" },
                    m.function.0
                )?;
            }
            for e in &c.connects {
                let sink = match &e.sink {
                    crate::ir::ConnectSink::Method { method } => format!("{method} (method)"),
                    crate::ir::ConnectSink::Event { event } => format!("{event} (event)"),
                };
                writeln!(
                    f,
                    "    connect {}.{} -> {}.{} (c{})",
                    e.src_path.join("."),
                    e.src_event,
                    e.sink_path.join("."),
                    sink,
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
                    "    on {} cycles{phase} = fn{}",
                    expr_str_for_component(self, ph.function, &ph.period),
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
                        "    on bus.{channel}.handshake [{}] ({edge}) = fn{}",
                        expr_str_for_component(self, ch.function, &ch.trigger),
                        ch.function.0
                    )?;
                } else {
                    writeln!(
                        f,
                        "    on {} ({edge}) = fn{}",
                        expr_str_for_component(self, ch.function, &ch.trigger),
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
                    "    watchdog period {period} max_idle {max_idle} = fn{}",
                    w.function.0
                )?;
            }
        }
        for (i, cg) in self.covgroups.iter().enumerate() {
            let trig = match cg.trigger {
                CovTrigger::PosedgeDutClk => "@posedge(dut.clk)",
            };
            writeln!(f, "  covgroup cg{} {} {}", i, cg.name, trig)?;
            for p in &cg.points {
                write!(f, "    point {} <- {}:", p.name, port_str(&p.target))?;
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
        for func in &self.functions {
            writeln!(f)?;
            write!(f, "{func}")?;
        }
        Ok(())
    }
}

impl Display for TbFunction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "fn fn{} {} [kind={}", self.id.0, self.name, kind_str(&self.kind))?;
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
        FunctionKind::Tseq { record } => format!("Tseq(r{})", record.0),
        FunctionKind::TestHook => "TestHook".to_string(),
    }
}

fn type_str(t: &IrType) -> String {
    match t {
        IrType::UInt(Some(w)) => format!("uint<{w}>"),
        IrType::UInt(None) => "uint".to_string(),
        IrType::SInt(Some(w)) => format!("sint<{w}>"),
        IrType::SInt(None) => "sint".to_string(),
        IrType::Bool => "bool".to_string(),
        IrType::Record(r) => format!("record(r{})", r.0),
        IrType::RecordSeq(r) => format!("seq(r{})", r.0),
        IrType::Unknown => "unknown".to_string(),
    }
}

fn stmt_str(func: &TbFunction, s: &Stmt) -> String {
    match s {
        Stmt::Assign(l, e) => format!("Assign({}, {})", local_str(func, *l), expr_str(func, e)),
        Stmt::DutWrite(p, e) => format!("DutWrite({}, {})", port_str(p), expr_str(func, e)),
        Stmt::DutRead(l, p) => format!("DutRead({}, {})", local_str(func, *l), port_str(p)),
        Stmt::ProbeRelease(p) => format!("ProbeRelease({})", port_str(p)),
        Stmt::RecordInit(l, r) => format!("RecordInit({}, r{})", local_str(func, *l), r.0),
        Stmt::RecordFieldWrite { local, field, index, value } => {
            let idx = match index {
                Some(i) => format!("[{}]", expr_str(func, i)),
                None => String::new(),
            };
            format!(
                "RecordFieldWrite({}.{field}{idx}, {})",
                local_str(func, *local),
                expr_str(func, value)
            )
        }
        Stmt::RecordWriteCb { local, binding, field, offset, value, callback } => {
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
        Stmt::TransactorStateWrite { instance, field, value } => {
            format!("TransactorStateWrite({instance}.{field}, {})", expr_str(func, value))
        }
        Stmt::Log { level, args } => {
            format!("Log({}, {})", level_str(level), fmt_args_str(func, args))
        }
        Stmt::AssertCheck { cond, on_fail } => format!(
            "AssertCheck {{ cond: {}, on_fail: {} }}",
            expr_str(func, cond),
            fmt_args_str(func, on_fail)
        ),
        Stmt::CovReport(inst) => {
            format!("CovReport({}.cg{})", inst.tb_field, inst.covgroup.0)
        }
        Stmt::TransactorCall { dest, call } => match dest {
            Some(d) => format!(
                "TransactorCall({} = {})",
                local_str(func, *d),
                expr_str(func, call)
            ),
            None => format!("TransactorCall({})", expr_str(func, call)),
        },
        Stmt::FailDiag { guard, args } => match guard {
            Some(g) => format!(
                "FailDiag {{ unless: {}, {} }}",
                expr_str(func, g),
                fmt_args_str(func, args)
            ),
            None => format!("FailDiag {{ {} }}", fmt_args_str(func, args)),
        },
        Stmt::ScoreboardOp { sb, field, op, nested_path } => {
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
        Stmt::ComponentEmit { base, event, args } => {
            let a: Vec<String> = args.iter().map(|e| expr_str(func, e)).collect();
            format!("ComponentEmit({}.{event}, [{}])", comp_base_str(base), a.join(", "))
        }
        Stmt::ComponentCall { base, component, method, args, dest } => {
            let a: Vec<String> = args.iter().map(|e| expr_str(func, e)).collect();
            let call = format!("{}.c{}::{method}([{}])", comp_base_str(base), component.0, a.join(", "));
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
            let descs: Vec<String> =
                pending.iter().map(|d| tlm_fork_desc_str(func, d)).collect();
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
    match v {
        CovBinValue::Eq(x) => x.to_string(),
        CovBinValue::Range { lo, hi } => format!(
            "[{}..{}]",
            lo.map(|x| x.to_string()).unwrap_or_default(),
            hi.map(|x| x.to_string()).unwrap_or_default()
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
    format!("%{}", func.local(l).name)
}

fn port_str(p: &PortRef) -> String {
    let mut out = format!("{}.{}", p.testbench_field, p.port_path.join("."));
    if let Some(lane) = p.lane {
        out.push_str(&format!("[{lane}]"));
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
        Expr::Port(p) => port_str(p),
        Expr::RecordField { local, field, index } => {
            let idx = match index {
                Some(i) => format!("[{}]", expr_str(func, i)),
                None => String::new(),
            };
            format!("{}.{field}{idx}", local_str(func, *local))
        }
        Expr::RegRead { mirror, helper_ty, field, offset, reads_bus } => {
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
        Expr::TransactorState { instance, field } => format!("{instance}.{field}"),
        Expr::ScoreboardQuery { sb, field, query, nested_path } => {
            let access = match nested_path {
                Some(p) => p.join("."),
                None => format!("sb{}.{field}", sb.0),
            };
            format!("ScoreboardQuery({access}.{})", sb_query_str(query))
        }
        Expr::ComponentField { base, field } => {
            format!("{}.{field}", comp_base_str(base))
        }
        Expr::ComponentQueueQuery { base, query } => {
            format!("ComponentQueueQuery({}.{})", comp_base_str(base), sb_query_str(query))
        }
        Expr::ComponentIdle { base, kind, n } => {
            let m = match kind {
                crate::ir::IdleKind::In => "idle_in",
                crate::ir::IdleKind::Out => "idle_out",
                crate::ir::IdleKind::Both => "idle",
            };
            format!("{}.{m}({})", comp_base_str(base), expr_str(func, n))
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
            let sw = src_width
                .map(|w| format!(" from {w}"))
                .unwrap_or_default();
            format!("{}.{k}<{width}{sw}>()", expr_str(func, inner))
        }
        Expr::CovBin { inst, point, bin } => {
            format!("CovBin({}.cg{}, {point}, {bin})", inst.tb_field, inst.covgroup.0)
        }
        Expr::SeqLen(l) => format!("SeqLen({})", local_str(func, *l)),
        Expr::SeqIndex { seq, index } => {
            format!("SeqIndex({}, {})", local_str(func, *seq), expr_str(func, index))
        }
        Expr::Call(target, args) => {
            let t = match target {
                CallTarget::Helper(n) => n.clone(),
                CallTarget::Builtin(n) => format!("builtin:{n}"),
                CallTarget::ExternFn(n) => format!("extern:{n}"),
                CallTarget::TransactorMethod { bus_field, method } => {
                    format!("{bus_field}.{method}")
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
