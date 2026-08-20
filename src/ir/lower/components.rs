//! Composite-component lowering facade.
//!
//! The implementation body stays in `components_impl.rs`; this facade owns
//! the instance-mode classifier that needs to reason about subscriptions per
//! input event. Keeping that policy here prevents an unrelated always-on
//! handler from masking a different input whose only subscriber is active-only.

// `components_impl.rs` was the original `components.rs` module. It expects its
// `super::...` paths to resolve against `lower`; importing the parent module's
// names here preserves those paths when it is nested below this facade.
#[allow(unused_imports)]
use super::*;

#[path = "components_impl.rs"]
mod implementation;

pub(crate) use implementation::{
    dotted_path, endpoint_label, fold_field_default, is_event_field, lower_component_bodies,
    lower_component_schema, lower_event_payload, lower_queue_elem, resolve_connects,
    resolve_testbench_connects, scoreboard_is_component,
    transactor_has_mode_sensitive_analysis_surface, transactor_is_analysis_source,
    transactor_is_component, transactor_is_dut_poking_bfm, transactor_is_event_driven,
    transactor_is_function_library, transactor_is_passive_helper, transactor_is_reactive_monitor,
    validate_mode_metadata, CompSource,
};

/// Return the bare self-event name subscribed by a non-periodic
/// `on <event>(...)` handler. Dotted calls and other call-shaped triggers are
/// not self-event subscriptions at this classification seam.
fn subscription_event(h: &crate::ast::OnHandler) -> Option<&str> {
    if h.periodic {
        return None;
    }
    let crate::ast::ExprKind::Call { callee, .. } = &*h.event.kind else {
        return None;
    };
    let crate::ast::ExprKind::Ident(id) = &*callee.kind else {
        return None;
    };
    Some(id.name.as_str())
}

/// True when at least one always-visible `in event` has subscribers only in
/// `when active`.
///
/// This must be decided per event, not per transactor. For example, an
/// always-on `on req1(v)` does not make `req2` safe on a passive instance if
/// the only `on req2(v)` lives under `when active`: `emit t.req2(...)` would
/// otherwise iterate an empty subscriber vector and silently drop the
/// transaction.
///
/// Inputs declared themselves under `when active` are intentionally excluded:
/// they are not part of a passive instance's visible surface, and ordinary
/// activation checks already reject attempts to emit through them.
pub(crate) fn transactor_is_active_only_consumer(t: &crate::ast::TransactorDecl) -> bool {
    if !implementation::transactor_is_event_driven(t) {
        return false;
    }

    // The implementation module's original classifier is intentionally only
    // a coarse fast path here. When it returns true there are no ordinary-body
    // non-periodic handlers at all, so any active-only subscription found below
    // certainly lacks an always-on subscriber. Its false result is NOT enough
    // to classify the transactor: mixed req1/req2 cases still require the
    // event-specific check below.
    let no_always_nonperiodic_handlers = implementation::transactor_is_active_only_consumer(t);

    let is_always_input = |event: &str| {
        t.items.iter().any(|item| {
            matches!(
                item,
                crate::ast::ComponentItem::Field(field)
                    if implementation::is_event_field(field)
                        && matches!(field.direction, Some(crate::ast::Direction::In))
                        && field.name.name == event
            )
        })
    };

    let has_always_subscriber = |event: &str| {
        t.items.iter().any(|item| {
            matches!(
                item,
                crate::ast::ComponentItem::OnHandler(handler)
                    if subscription_event(handler) == Some(event)
            )
        })
    };

    t.when_active
        .iter()
        .flatten()
        .filter_map(|item| match item {
            crate::ast::ComponentItem::OnHandler(handler) => subscription_event(handler),
            _ => None,
        })
        .any(|event| {
            is_always_input(event)
                && (no_always_nonperiodic_handlers || !has_always_subscriber(event))
        })
}
