# R15 supported-contract regressions

This inventory records ten bug patterns and the executable checks added for
them. Its denominator is these ten patterns, not the language or all compiler
code. Passing these checks does not establish bug freedom or replace the
broader formal, mutation, fuzz, and independent-search stopping gates.

| Pattern | Required regression |
| --- | --- |
| Bare `return` consumes a statement on the next line | Parser AST/formatter test `return_ends_at_newline_but_parenthesized_value_can_continue`; `return_newline_native` |
| Compound or inferred width-method receivers lose source width | TBIR test `width_methods_preserve_lowered_receiver_width_metadata`; native `signed_wide_ops_test.harc` fixture, including masked and explicit-cast controls |
| Signed captured fields lose their value across solver widths | `native_signed_capture_e2e`: source/target width boundaries, unequal captures in both operand orders, large relational SAT/UNSAT cases, disjunctions, and soft constraints |
| Bare captured Boolean constraints abort inside Z3 | TBIR producer test and `native_bool_capture_e2e`: true/false captures, explicit comparisons, target flags, and nested logical expressions |
| Event callback payload shadows a component receiver named `_t` | `tbir_component_local_init`: payload delivery and runtime-graph ownership in default and common layouts |
| Fatal wall-clock waits execute future edges or caller successors | `fatal_wall_wait_e2e`: current-edge completion, forbidden future edges, void/value helper callers, cycle and nonfatal controls, and fatal-before-zero-wait behavior |
| Static signed coverage bounds become unsigned literals | TBIR producer tests and `static_cover_bins_e2e`: cross-zero intervals, `i64::MIN`, runtime equivalents, and an unsigned `u64::MAX` control |
| Runtime signed coverage bounds use unsigned host ordering | Native `cov_runtime_signed_bounds_test.harc` fixture: mixed widths, static/runtime composition, negative/positive intervals, 65-bit sign-bit samples, and unsigned controls |
| Default ARCH launcher selection ignores `PATH` | `harc` binary unit tests and `check_cli::sim_uses_path_arch_launcher_and_forwards_arguments`; the launcher stub tests routing, not DUT simulation |
| Pure void helper calls carry value-return metadata | TBIR lower/verify and malformed-IR tests; `tbir_void_helper_e2e`: caller continuation and argument effects/order in default and common layouts |

Run Rust targets with `cargo test --test <target>`. The named HARC fixtures are
registered in the fixture tables and require native simulation, not just
emission. Set `HARC_REQUIRE_VERILATOR=1` and make Verilator and Z3 available when
collecting native evidence. An unavailable dependency or skipped test does not
cover a pattern. Learning-store unit tests should run without globally setting
`HARC_NO_LEARN`, because they explicitly exercise isolated learning stores.

The new findings reset the consecutive clean-search count to zero. A readiness
decision additionally needs current source-bound evidence for the full contract
inventory, the required proof/mutation/fuzz campaigns, and three completed clean
ten-scope searches. This regression ledger alone cannot make that decision.
