use super::{Expr, FmtArgs, LaneIndex, Stmt, Terminator};
use std::convert::Infallible;

pub fn walk_expr(expr: &Expr, visit: &mut impl FnMut(&Expr)) {
    let result = try_walk_expr(expr, &mut |expr| {
        visit(expr);
        Ok::<(), Infallible>(())
    });
    match result {
        Ok(()) => {}
        Err(error) => match error {},
    }
}

pub fn try_walk_expr<E>(
    expr: &Expr,
    visit: &mut impl FnMut(&Expr) -> Result<(), E>,
) -> Result<(), E> {
    visit(expr)?;
    try_visit_expr_children(expr, &mut |child| try_walk_expr(child, visit))
}

pub fn walk_expr_mut(expr: &mut Expr, visit: &mut impl FnMut(&mut Expr)) {
    visit(expr);
    visit_expr_children_mut(expr, &mut |child| walk_expr_mut(child, visit));
}

pub fn try_visit_expr_children<E>(
    expr: &Expr,
    visit: &mut impl FnMut(&Expr) -> Result<(), E>,
) -> Result<(), E> {
    match expr {
        Expr::Port(port) => try_visit_port_lane_expr(port, visit),
        Expr::Binary(_, lhs, rhs) => {
            visit(lhs)?;
            visit(rhs)
        }
        Expr::Unary(_, inner)
        | Expr::BitSlice { target: inner, .. }
        | Expr::WidthCast { inner, .. }
        | Expr::DynamicListQuery { target: inner, .. } => visit(inner),
        Expr::Ternary(cond, lhs, rhs) => {
            visit(cond)?;
            visit(lhs)?;
            visit(rhs)
        }
        Expr::BitSliceDyn { target, hi, lo } => {
            visit(target)?;
            visit(hi)?;
            visit(lo)
        }
        Expr::RecordField {
            mid_indices, index, ..
        }
        | Expr::TransactorStateRecordField {
            mid_indices, index, ..
        } => {
            for (_, expr) in mid_indices {
                visit(expr)?;
            }
            if let Some(expr) = index {
                visit(expr)?;
            }
            Ok(())
        }
        Expr::TbFieldVecElement {
            index, inner_index, ..
        }
        | Expr::ComponentVecElement {
            index, inner_index, ..
        } => {
            visit(index)?;
            if let Some(expr) = inner_index {
                visit(expr)?;
            }
            Ok(())
        }
        Expr::PortSnapshotLane { port, index, .. } => {
            try_visit_port_lane_expr(port, visit)?;
            visit(index)
        }
        Expr::SeqIndex { index, .. }
        | Expr::ComponentIdle { n: index, .. }
        | Expr::TransactorIdle { n: index, .. } => visit(index),
        Expr::CovHookParam {
            index: Some(index), ..
        } => visit(index),
        Expr::Call(_, args) => {
            for expr in args {
                visit(expr)?;
            }
            Ok(())
        }
        Expr::Literal { .. }
        | Expr::StringLiteral(_)
        | Expr::WideLiteral(_)
        | Expr::Local(_)
        | Expr::TbField(_)
        | Expr::TemporalSlot { .. }
        | Expr::TbQueueQuery { .. }
        | Expr::TransactorState { .. }
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::ScoreboardQuery { .. }
        | Expr::ComponentField { .. }
        | Expr::ComponentValue { .. }
        | Expr::ComponentQueueQuery { .. }
        | Expr::CycleCount
        | Expr::ErrorCount
        | Expr::CovBin { .. }
        | Expr::CovHookParam { index: None, .. }
        | Expr::CovHookArg { .. }
        | Expr::SeqLen(_)
        | Expr::RegRead { .. } => Ok(()),
    }
}

pub fn visit_expr_children_mut(expr: &mut Expr, visit: &mut impl FnMut(&mut Expr)) {
    match expr {
        Expr::Port(port) => visit_port_lane_expr_mut(port, visit),
        Expr::Binary(_, lhs, rhs) => {
            visit(lhs);
            visit(rhs);
        }
        Expr::Unary(_, inner)
        | Expr::BitSlice { target: inner, .. }
        | Expr::WidthCast { inner, .. }
        | Expr::DynamicListQuery { target: inner, .. } => visit(inner),
        Expr::Ternary(cond, lhs, rhs) => {
            visit(cond);
            visit(lhs);
            visit(rhs);
        }
        Expr::BitSliceDyn { target, hi, lo } => {
            visit(target);
            visit(hi);
            visit(lo);
        }
        Expr::RecordField {
            mid_indices, index, ..
        }
        | Expr::TransactorStateRecordField {
            mid_indices, index, ..
        } => {
            for (_, expr) in mid_indices {
                visit(expr);
            }
            if let Some(expr) = index {
                visit(expr);
            }
        }
        Expr::TbFieldVecElement {
            index, inner_index, ..
        }
        | Expr::ComponentVecElement {
            index, inner_index, ..
        } => {
            visit(index);
            if let Some(expr) = inner_index {
                visit(expr);
            }
        }
        Expr::PortSnapshotLane { port, index, .. } => {
            visit_port_lane_expr_mut(port, visit);
            visit(index);
        }
        Expr::SeqIndex { index, .. }
        | Expr::ComponentIdle { n: index, .. }
        | Expr::TransactorIdle { n: index, .. } => visit(index),
        Expr::CovHookParam {
            index: Some(index), ..
        } => visit(index),
        Expr::Call(_, args) => {
            for expr in args {
                visit(expr);
            }
        }
        Expr::Literal { .. }
        | Expr::StringLiteral(_)
        | Expr::WideLiteral(_)
        | Expr::Local(_)
        | Expr::TbField(_)
        | Expr::TemporalSlot { .. }
        | Expr::TbQueueQuery { .. }
        | Expr::TransactorState { .. }
        | Expr::TransactorStateQueueQuery { .. }
        | Expr::ScoreboardQuery { .. }
        | Expr::ComponentField { .. }
        | Expr::ComponentValue { .. }
        | Expr::ComponentQueueQuery { .. }
        | Expr::CycleCount
        | Expr::ErrorCount
        | Expr::CovBin { .. }
        | Expr::CovHookParam { index: None, .. }
        | Expr::CovHookArg { .. }
        | Expr::SeqLen(_)
        | Expr::RegRead { .. } => {}
    }
}

pub fn try_visit_stmt_exprs<E>(
    stmt: &Stmt,
    visit: &mut impl FnMut(&Expr) -> Result<(), E>,
) -> Result<(), E> {
    match stmt {
        Stmt::Assign(_, expr)
        | Stmt::RecordWriteCb { value: expr, .. }
        | Stmt::TbFieldWrite { value: expr, .. }
        | Stmt::TbQueuePush { value: expr, .. }
        | Stmt::TransactorStateWrite { value: expr, .. }
        | Stmt::TransactorStateQueuePush { value: expr, .. }
        | Stmt::ComponentFieldWrite { value: expr, .. }
        | Stmt::ComponentQueuePush { value: expr, .. }
        | Stmt::SeqPush { value: expr, .. } => visit(expr),
        Stmt::DutWrite(port, value) => {
            try_visit_port_lane_expr(port, visit)?;
            visit(value)
        }
        Stmt::DutRead(_, port) | Stmt::ProbeRelease(port) => try_visit_port_lane_expr(port, visit),
        Stmt::RecordRead { addr, .. } => visit(addr),
        Stmt::RecordWrite { addr, value, .. } => {
            visit(addr)?;
            visit(value)
        }
        Stmt::RecordFieldWrite {
            mid_indices,
            index,
            value,
            ..
        }
        | Stmt::TransactorStateRecordFieldWrite {
            mid_indices,
            index,
            value,
            ..
        } => {
            for (_, expr) in mid_indices {
                visit(expr)?;
            }
            if let Some(expr) = index {
                visit(expr)?;
            }
            visit(value)
        }
        Stmt::TbFieldVecElementWrite {
            index,
            inner_index,
            value,
            ..
        }
        | Stmt::ComponentVecElementWrite {
            index,
            inner_index,
            value,
            ..
        } => {
            visit(index)?;
            if let Some(expr) = inner_index {
                visit(expr)?;
            }
            visit(value)
        }
        Stmt::Log { args, .. } => visit_fmt(args, visit),
        Stmt::AssertCheck { cond, on_fail } | Stmt::AssumeCheck { cond, on_fail } => {
            visit(cond)?;
            visit_fmt(on_fail, visit)
        }
        Stmt::FailDiag { guard, args } => {
            if let Some(expr) = guard {
                visit(expr)?;
            }
            visit_fmt(args, visit)
        }
        Stmt::ScoreboardOp { op, .. } => match op {
            super::ScoreboardOp::QueuePush { value, .. }
            | super::ScoreboardOp::ScalarWrite { value, .. } => visit(value),
            super::ScoreboardOp::QueuePop { .. } => Ok(()),
        },
        Stmt::ComponentEmit { args, .. } | Stmt::EventEmit { args, .. } => visit_exprs(args, visit),
        Stmt::ComponentCall { args, .. } | Stmt::TestbenchCall { args, .. } => {
            visit_exprs(args, visit)
        }
        Stmt::TransactorCall { call, .. } | Stmt::TransactorSelfCall { call, .. } => visit(call),
        Stmt::TlmFork(desc) => visit_exprs(&desc.args, visit),
        Stmt::TlmJoinAll(pending) => {
            for desc in pending {
                visit_exprs(&desc.args, visit)?;
            }
            Ok(())
        }
        Stmt::RecordInit(..)
        | Stmt::AggregateInit(_)
        | Stmt::TbQueuePop { .. }
        | Stmt::TransactorStateQueuePop { .. }
        | Stmt::PropertyCheck(_)
        | Stmt::CoverCheck(_)
        | Stmt::CycleHandler(_)
        | Stmt::EventSubscribe { .. }
        | Stmt::MethodHookSubscribe { .. }
        | Stmt::CovReport(_)
        | Stmt::ComponentQueuePop { .. }
        | Stmt::ComponentInit { .. }
        | Stmt::ComponentSubAssign { .. }
        | Stmt::ComponentAssign { .. } => Ok(()),
    }
}

pub fn visit_stmt_exprs_mut(stmt: &mut Stmt, visit: &mut impl FnMut(&mut Expr)) {
    match stmt {
        Stmt::Assign(_, expr)
        | Stmt::RecordWriteCb { value: expr, .. }
        | Stmt::TbFieldWrite { value: expr, .. }
        | Stmt::TbQueuePush { value: expr, .. }
        | Stmt::TransactorStateWrite { value: expr, .. }
        | Stmt::TransactorStateQueuePush { value: expr, .. }
        | Stmt::ComponentFieldWrite { value: expr, .. }
        | Stmt::ComponentQueuePush { value: expr, .. }
        | Stmt::SeqPush { value: expr, .. } => visit(expr),
        Stmt::DutWrite(port, value) => {
            visit_port_lane_expr_mut(port, visit);
            visit(value);
        }
        Stmt::DutRead(_, port) | Stmt::ProbeRelease(port) => visit_port_lane_expr_mut(port, visit),
        Stmt::RecordRead { addr, .. } => visit(addr),
        Stmt::RecordWrite { addr, value, .. } => {
            visit(addr);
            visit(value);
        }
        Stmt::RecordFieldWrite {
            mid_indices,
            index,
            value,
            ..
        }
        | Stmt::TransactorStateRecordFieldWrite {
            mid_indices,
            index,
            value,
            ..
        } => {
            for (_, expr) in mid_indices {
                visit(expr);
            }
            if let Some(expr) = index {
                visit(expr);
            }
            visit(value);
        }
        Stmt::TbFieldVecElementWrite {
            index,
            inner_index,
            value,
            ..
        }
        | Stmt::ComponentVecElementWrite {
            index,
            inner_index,
            value,
            ..
        } => {
            visit(index);
            if let Some(expr) = inner_index {
                visit(expr);
            }
            visit(value);
        }
        Stmt::Log { args, .. } => visit_fmt_mut(args, visit),
        Stmt::AssertCheck { cond, on_fail } | Stmt::AssumeCheck { cond, on_fail } => {
            visit(cond);
            visit_fmt_mut(on_fail, visit);
        }
        Stmt::FailDiag { guard, args } => {
            if let Some(expr) = guard {
                visit(expr);
            }
            visit_fmt_mut(args, visit);
        }
        Stmt::ScoreboardOp { op, .. } => match op {
            super::ScoreboardOp::QueuePush { value, .. }
            | super::ScoreboardOp::ScalarWrite { value, .. } => visit(value),
            super::ScoreboardOp::QueuePop { .. } => {}
        },
        Stmt::ComponentEmit { args, .. } | Stmt::EventEmit { args, .. } => {
            for expr in args {
                visit(expr);
            }
        }
        Stmt::ComponentCall { args, .. } | Stmt::TestbenchCall { args, .. } => {
            for expr in args {
                visit(expr);
            }
        }
        Stmt::TransactorCall { call, .. } | Stmt::TransactorSelfCall { call, .. } => visit(call),
        Stmt::TlmFork(desc) => {
            for expr in &mut desc.args {
                visit(expr);
            }
        }
        Stmt::TlmJoinAll(pending) => {
            for desc in pending {
                for expr in &mut desc.args {
                    visit(expr);
                }
            }
        }
        Stmt::RecordInit(..)
        | Stmt::AggregateInit(_)
        | Stmt::TbQueuePop { .. }
        | Stmt::TransactorStateQueuePop { .. }
        | Stmt::PropertyCheck(_)
        | Stmt::CoverCheck(_)
        | Stmt::CycleHandler(_)
        | Stmt::EventSubscribe { .. }
        | Stmt::MethodHookSubscribe { .. }
        | Stmt::CovReport(_)
        | Stmt::ComponentQueuePop { .. }
        | Stmt::ComponentInit { .. }
        | Stmt::ComponentSubAssign { .. }
        | Stmt::ComponentAssign { .. } => {}
    }
}

pub fn try_visit_terminator_exprs<E>(
    terminator: &Terminator,
    visit: &mut impl FnMut(&Expr) -> Result<(), E>,
) -> Result<(), E> {
    match terminator {
        Terminator::Branch(expr, _, _)
        | Terminator::WaitCycles(expr, _, _)
        | Terminator::WaitCyclesSync(expr, _) => visit(expr),
        Terminator::WaitUntil { preds, .. } => {
            for pred in preds {
                visit(&pred.expr)?;
            }
            Ok(())
        }
        Terminator::WaitUntilTimeout { preds, cycles, .. } => {
            for pred in preds {
                visit(&pred.expr)?;
            }
            visit(cycles)
        }
        Terminator::Fatal(args) => visit_fmt(args, visit),
        Terminator::Randomize { .. }
        | Terminator::TbLifecycleCall { .. }
        | Terminator::Jump(_)
        | Terminator::WaitTimePs(_, _)
        | Terminator::Return => Ok(()),
    }
}

pub fn visit_terminator_exprs_mut(terminator: &mut Terminator, visit: &mut impl FnMut(&mut Expr)) {
    match terminator {
        Terminator::Branch(expr, _, _)
        | Terminator::WaitCycles(expr, _, _)
        | Terminator::WaitCyclesSync(expr, _) => visit(expr),
        Terminator::WaitUntil { preds, .. } => {
            for pred in preds {
                visit(&mut pred.expr);
            }
        }
        Terminator::WaitUntilTimeout { preds, cycles, .. } => {
            for pred in preds {
                visit(&mut pred.expr);
            }
            visit(cycles);
        }
        Terminator::Fatal(args) => visit_fmt_mut(args, visit),
        Terminator::Randomize { .. }
        | Terminator::TbLifecycleCall { .. }
        | Terminator::Jump(_)
        | Terminator::WaitTimePs(_, _)
        | Terminator::Return => {}
    }
}

pub fn try_visit_port_lane_expr<E>(
    port: &super::PortRef,
    visit: &mut impl FnMut(&Expr) -> Result<(), E>,
) -> Result<(), E> {
    match &port.lane {
        Some(LaneIndex::Var(index)) => visit(index),
        Some(LaneIndex::Const(_)) | None => Ok(()),
    }
}

pub fn visit_port_lane_expr(port: &super::PortRef, visit: &mut impl FnMut(&Expr)) {
    let result = try_visit_port_lane_expr(port, &mut |expr| {
        visit(expr);
        Ok::<(), Infallible>(())
    });
    match result {
        Ok(()) => {}
        Err(error) => match error {},
    }
}

pub fn visit_port_lane_expr_mut(port: &mut super::PortRef, visit: &mut impl FnMut(&mut Expr)) {
    if let Some(LaneIndex::Var(index)) = &mut port.lane {
        visit(index);
    }
}

fn visit_exprs<E>(exprs: &[Expr], visit: &mut impl FnMut(&Expr) -> Result<(), E>) -> Result<(), E> {
    for expr in exprs {
        visit(expr)?;
    }
    Ok(())
}

fn visit_fmt<E>(args: &FmtArgs, visit: &mut impl FnMut(&Expr) -> Result<(), E>) -> Result<(), E> {
    for arg in &args.args {
        visit(&arg.expr)?;
    }
    Ok(())
}

fn visit_fmt_mut(args: &mut FmtArgs, visit: &mut impl FnMut(&mut Expr)) {
    for arg in &mut args.args {
        visit(&mut arg.expr);
    }
}
