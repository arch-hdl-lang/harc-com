# 03 — Preserve nested mode inheritance and overrides

**What to build:** Nested component paths resolve transactor modes consistently across test-scope roots and structural env/agent composition. Root mode carriers propagate to unannotated descendants, explicit child transactor modes override inheritance, and structural components never acquire transactor ownership semantics of their own.

**Parent:** https://github.com/arch-hdl-lang/harc-com/issues/534

**Blocked by:** 01 — Make direct analysis-source transactor bindings mode-correct; 02 — Gate `when active` surfaces by effective mode.

**Status:** ready-for-agent

- [ ] A test-scope root mode carrier reaches an unannotated nested transactor.
- [ ] An explicit nested transactor mode overrides its inherited root or parent mode.
- [ ] A nested transactor with no explicit or inherited mode fails at the unresolved leaf path.
- [ ] A mode on an env, agent, scoreboard, or sequencer field is rejected rather than treated as an override.
- [ ] Recursive method, field, event, connect, lifecycle, and handler resolution uses one effective-mode rule.
- [ ] A passive override suppresses active-only registrations beneath an active root.
- [ ] Nested active/passive instances retain independent state and preserve declaration-order analysis fanout.
- [ ] Existing env- and agent-mode inheritance fixtures remain compatible.
