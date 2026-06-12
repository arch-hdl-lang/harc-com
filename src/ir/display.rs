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
                write!(
                    f,
                    "    {}{} : {}",
                    if fld.non_random { "!" } else { "" },
                    fld.name,
                    type_str(&fld.ty)
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
        IrType::Unknown => "unknown".to_string(),
    }
}

fn stmt_str(func: &TbFunction, s: &Stmt) -> String {
    match s {
        Stmt::Assign(l, e) => format!("Assign({}, {})", local_str(func, *l), expr_str(func, e)),
        Stmt::DutWrite(p, e) => format!("DutWrite({}, {})", port_str(p), expr_str(func, e)),
        Stmt::DutRead(l, p) => format!("DutRead({}, {})", local_str(func, *l), port_str(p)),
        Stmt::RecordInit(l, r) => format!("RecordInit({}, r{})", local_str(func, *l), r.0),
        Stmt::RecordFieldWrite { local, field, value } => format!(
            "RecordFieldWrite({}.{field}, {})",
            local_str(func, *local),
            expr_str(func, value)
        ),
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
        Stmt::FailDiag { guard, args } => match guard {
            Some(g) => format!(
                "FailDiag {{ unless: {}, {} }}",
                expr_str(func, g),
                fmt_args_str(func, args)
            ),
            None => format!("FailDiag {{ {} }}", fmt_args_str(func, args)),
        },
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
        LogLevel::Info => "info".to_string(),
        LogLevel::Warn => "warn".to_string(),
        LogLevel::Error => "error".to_string(),
        LogLevel::Fatal => "fatal".to_string(),
        LogLevel::File { path, level } => {
            let lv = match level {
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
    match p.access {
        PortAccess::Port => {}
        PortAccess::Probe => out.push_str(" (probe)"),
        PortAccess::Force => out.push_str(" (force)"),
    }
    out
}

pub(crate) fn expr_str(func: &TbFunction, e: &Expr) -> String {
    match e {
        Expr::Literal { value, .. } => format!("{value}"),
        Expr::Local(l) => local_str(func, *l),
        Expr::Port(p) => port_str(p),
        Expr::RecordField { local, field } => {
            format!("{}.{field}", local_str(func, *local))
        }
        Expr::Binary(op, a, b) => format!(
            "({} {} {})",
            expr_str(func, a),
            bin_op_str(*op),
            expr_str(func, b)
        ),
        Expr::Unary(op, a) => format!("{}{}", un_op_str(*op), expr_str(func, a)),
        Expr::CovBin { inst, point, bin } => {
            format!("CovBin({}.cg{}, {point}, {bin})", inst.tb_field, inst.covgroup.0)
        }
        Expr::Call(target, args) => {
            let t = match target {
                CallTarget::Helper(n) => n.clone(),
                CallTarget::Builtin(n) => format!("builtin:{n}"),
                CallTarget::TransactorMethod { bus_field, method } => {
                    format!("{bus_field}.{method}")
                }
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
