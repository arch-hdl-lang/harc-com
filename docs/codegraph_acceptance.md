# HARC code graph acceptance baseline

Commit under test: `c3ca30da` plus the changes in this worktree. The source graph
and compiler-development graph are separate indexes.

## Curated corpus

The stable acceptance corpus is embedded in
`graph::tests::curated_graph_has_relationships_and_stable_output`. It covers
transactions, transactors, scoreboards, covergroups, regblocks, addrmaps,
tests, TB IR lowering, bus binding, and SystemVerilog DUTs. Imported ARCH bus
files are included by the indexer.

Command:

```sh
cargo test --offline --lib graph::tests
```

Observed index: 12 files, 55 nodes, 217 edges, zero parse/read skips, zero
lowering omissions. The acceptance test checks `binds_bus`, `binds_dut`,
`calls`, `checks`, `covers`, `lowers_to`, `randomizes`, `samples`, and
`uses_transactor` edges; stable output on reindex; bounded
context; and path/reason diagnostics for malformed input. A separate test
checks stale-source rejection.

## Full-tree audit

```sh
cargo run --offline --bin harc -- graph index tests/fixtures tests/dut --out .harcgraph
cargo run --offline --bin harc -- graph tests-for top_counter --index .harcgraph
cargo run --offline --bin harc -- graph query AxilXactor --index .harcgraph
cargo run --offline --bin harc -- graph context 'add coverage for AXI Lite read response' --index .harcgraph --token-budget 250
cargo run --offline --bin harc -- graph html --index .harcgraph --out harc-graph.html
```

Observed index: 363 files, 1,991 nodes, 6,481 edges, zero read/parse skips,
18 lowering omissions. The run took 4.97 seconds including `cargo run` startup
and build check. `tests-for top_counter` returned `TopCounterTest` at
`tests/fixtures/top_counter_test.harc:13`. The lowering omissions are written
with source paths and reasons to `issues.jsonl`; they include incomplete
standalone fixture dependencies and known TB IR unsupported forms, not parse
skips. The curated corpus remains the zero-omission acceptance gate.
The bounded context example now leads with `AxiCov` and `AxiOps` covergroups
and an AXI Lite read helper, rather than unrelated one-word coverage hits.

## Developer graph

```sh
cargo run --offline --bin harc -- graph dev-index
cargo run --offline --bin harc -- graph dev-query 'transactor lowering'
```

Observed index: 10 features, 63 nodes, 84 curated edges. Each mapped Rust
symbol is parsed and location-checked. The index records source fingerprints
and refuses queries after a mapped file changes. Curated ownership edges are
not static call edges or runtime proofs.

## Validation and remaining work

Direct calls through the installed MCP Python environment validated
`harc_dev_graph_index`, `harc_dev_graph_query`, `harc_graph_index`, and
`harc_graph_context` against the local compiler. The transactor query returned
the parser, lowerer, IR schema, backend, AST, semantic check, and fixture locations; the user
context query returned `AxilXactor` and its `binds_bus` edge within a
100-token response budget.

Compiler regressions: all 176 library tests and 21 binary tests
passed. `harc check` and `harc dump-ir` passed on
`transactor_active_test.harc`; emit-only generated the C++ testbench; the
full `harc sim --sv tests/dut/AxiLiteRegs.sv` run passed all tests at cycle
98. The HTML graph command wrote a viewer successfully. The benchmark
analyzer's three unit tests also passed. The MCP stdio smoke test initialized
a client session, listed graph tools, and queried both graph types through
fresh indexes, exercising automatic index creation. It also refreshed a
stale custom index from its recorded source path, without replacing it with
the default fixture corpus. Source-set and generator-change tests reject
stale indexes; directory indexing also skips symlinked descendants and
directory cycles. The MCP dependency is
constrained to 1.x because the server uses FastMCP.

The frozen paired token-efficiency experiment is reported in
`codegraph_confirmation_results.md`: compiler tasks met the 20% gate in the
focused corpus, while HARC-user tasks did not. Graph retrieval remains
selective rather than the default for all edits. CI includes a developer-index CLI
smoke check and a stdio MCP check, but that workflow has not yet run on GitHub.
