# 02 — Gate `when active` surfaces by effective mode

**What to build:** The same analysis-source transactor type exposes its `when active` surface only to active instances. Passive instances retain the always-on observation surface while active-only members and runtime registrations remain unreachable.

**Parent:** https://github.com/arch-hdl-lang/harc-com/issues/534

**Blocked by:** 01 — Make direct analysis-source transactor bindings mode-correct.

**Status:** ready-for-agent

- [ ] Activation provenance distinguishes ordinary-body members from `when active` members.
- [ ] Active-only fields, methods, events, event handlers, periodic services, cycle triggers, and watchdogs are registered only for active bindings.
- [ ] Passive bindings retain all always-on registrations and state.
- [ ] Calls, reads, writes, emits, and queue operations targeting active-only members through passive paths fail during lowering with full paths.
- [ ] Always-on method bodies cannot reference active-only sibling members.
- [ ] The active-only periodic behavior in the mixed-mode fixture runs for the active instance and remains absent for passive instances.
- [ ] Cooperative and multithreaded emission both respect the semantic mode.
- [ ] Emitted C++ inspection confirms per-instance registration gating without requiring per-mode schema duplication.
