# 01 — Make direct analysis-source transactor bindings mode-correct

**What to build:** Direct testbench bindings of an analysis-source transactor preserve their declared `active` or `passive` mode through TB-IR lowering, verification, and simulation. Active and passive instances share one component schema but own independent always-on state, methods, output events, and analysis fanout.

**Parent:** https://github.com/arch-hdl-lang/harc-com/issues/534

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] Direct `active` and `passive` analysis-source transactor fields lower and verify.
- [ ] A single shared schema supports mixed active and passive instances without encounter-order dependence.
- [ ] Each instance has independent persistent state.
- [ ] Always-on methods, output events, and analysis fanout remain available to passive instances.
- [ ] A direct transactor field without a mode fails as an `Invalid` source diagnostic naming the missing effective mode.
- [ ] Modes on env, agent, scoreboard, and sequencer fields fail as source-invalid diagnostics rather than backend fallback cases.
- [ ] A runnable fixture proves always-on state, method calls, event publication, and fanout for active and passive instances.
- [ ] Existing testbench-owned analysis-connect behavior remains green.
