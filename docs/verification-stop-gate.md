# Scoped verification coverage and stopping gate

`verification/contracts.json` is a critical-contract inventory, with ten
registered families and concrete check owners. It is deliberately incomplete.
The denominator is registered contracts, **not source lines, language features,
or all possible bugs**. Its `missing_families` list remains visible in reports.
One family has source-extracted symbolic arithmetic proofs; the other families
have bounded executable evidence. Passing a native fixture is not a formal
proof of its family. See `docs/formal-arithmetic.md` for each arithmetic domain,
assumption, tool trust boundary and unresolved wide division equivalence.

The inventory covers parser boundaries, arithmetic kernels, solver captures,
control flow, component ownership, clock progress, aggregate widths, coverage
ordering, CLI launch routing and RAL assignment. Each entry states its source
owner, bounded domain and required commands. The checker pins this inventory's
check ownership, known commands, exact proof identities and stopping thresholds.
An inventory extension requires corresponding checker review; deleting checks
or editing thresholds in JSON cannot silently turn a report green.

## Run and inspect

From the repository root, with Rust, Python, a native C++ compiler, Verilator,
Z3, CBMC and Kani available:

```sh
python3 -m unittest discover -s tools -p test_verification_coverage.py
python3 tools/verification_coverage.py inventory
python3 tools/verification_coverage.py run --out /tmp/harc-coverage-run
python3 tools/verification_coverage.py report \
  --checks /tmp/harc-coverage-run/checks.json \
  --evidence /tmp/fuzz-1.json --evidence /tmp/fuzz-2.json \
  --evidence /tmp/fuzz-3.json --evidence /tmp/mutation.json \
  --evidence /tmp/hunt-1.json --evidence /tmp/hunt-2.json \
  --evidence /tmp/hunt-3.json
```

`run` captures commands, exit statuses and complete combined output in an output
directory, including paths outside the checkout. Source directories cannot be
used for output. Every required check runs; a missing dependency is a failed
check. Native checks set `HARC_REQUIRE_VERILATOR=1`; the runner removes a global
`HARC_NO_LEARN` so learning-store tests retain their intended behavior.
Timeouts kill the invocation's process group. Rust output must contain a
recognized nonzero passing summary with zero failed and zero ignored tests;
explicit skipped/unavailable output fails. The arithmetic runner's own suite
must report exactly ten clean unittest tests. Formal evidence must include all
30 expected CBMC obligation IDs and all 12 Kani harness IDs; raw solver logs
are reparsed, including nonvacuous assertion results. Missing or unknown output
formats fail closed. `run` exits nonzero on any failure. `report` exits zero
only when `READY` is true.

The source binding hashes every file below `src`, `runtime`, `proofs`, `tests`,
`tools`, `verification`, `scripts`, `stdlib`, `signatures`, `ci` and `.cargo`,
plus Cargo manifests/lockfile, build script and Rust toolchain declarations.
Python bytecode/cache files are excluded. The binding includes the checker and
registry themselves. Before and after hashes detect changes during checks;
reports require exact current bindings and rehash stored raw logs. Rebuilding
or editing any included input invalidates old evidence. The production digest
is a canonical SHA-256 of production source/runtime/stdlib/signatures/config
and root build-manifest hashes. It lets search records name the precise
production snapshot in addition to the stricter full input binding.

## Evidence schema, version 1

Every external record is an object containing `schema_version: 1`, `kind`, a
nonempty unique `run_id`, positive integer Unix `completed_at`, `complete: true`,
`inputs` equal to `source_inputs(root)`, and `production_digest` equal to
`production_digest(root)`. The helper functions and `validate_binding`,
`validate_fuzz`, `validate_mutation`, `validate_hunt` are importable from
`tools/verification_coverage.py`. Provide campaign records in strictly
increasing completion-time order within each kind. Every supplied record must
be source-current; do not mix historical stale snapshots into a current report.
Keep the full campaign ledger externally so omitted intervening failures remain
reviewable. JSON records and identity fields are provenance attestations, not
cryptographic proof of execution, independence or identity.

A **fuzz** record has `kind: "fuzz"`, at least three distinct integer `seeds`,
at least 10,000 `distinct_cases`, matching unique SHA-256 `case_ids`, an empty
`failures` list and a unique-string `failure_signatures` list (possibly empty).
A case ID hashes the canonical tree and environment, excluding backend,
rendering, context and repeated observations. The checker verifies count and
uniqueness; the generator owns the canonicalization semantics. Each campaign
must independently reach 10,000 cases; combining three small campaigns does
not satisfy the threshold. Include:

```json
{"compiler": {"path": "/absolute/path/to/harc", "sha256": "<sha256>",
 "version": "<version output>",
 "build_command": ["cargo", "build", "--offline", "--bin", "harc"],
 "build_log": "/absolute/path/to/build.log", "build_log_sha256": "<sha256>"}}
```

The checker rehashes binary and build log, checks the exact build command and
requires a version. This records build provenance; a hash alone cannot prove
that a binary was built from the declared source. A final report needs three
full campaigns whose last three signature sets are equal (no new signatures).

A **broad mutation** record has `kind: "broad_mutation"` and `mutants`, at least
30 uniquely identified records across at least eight of the ten registered
areas. Each has `id`, `area`, `status` and `evidence`. Status is `killed`,
`survived`, `invalid` or `blocked`. Evidence contains `command` (nonempty string
array), `log` (absolute path), `log_sha256` (checked against the file) and
`reviewer` (nonempty identity attestation). At least 90% of **all** registered
mutants must be killed. Invalid/blocked mutants do not count as kills, and any
blocked result fails the gate. Ten arithmetic-only harness sensitivity mutants
are useful guards but cannot satisfy this broad semantic mutation gate.
Mutation selection, semantic validity and the quality of kill explanations
remain independent review responsibilities.

A **hunt** record has `kind: "hunt"`, exactly ten completed `scopes`, one per registered `area`, with unique
`id` and `investigator` identities and nonempty `summary`, plus a `candidates`
array (explicitly empty when no candidates). Each candidate has unique `id`,
`scope_id`, `verifier` different from its scope investigator, `classification`
and nonempty `evidence` describing/linking the independent reproduction or
rejection. Classifications are `invalid`, `rejected`, `duplicate` or
`proven_fixed`. Unresolved/proven-unfixed reports fail. A proven fixed bug also
makes that hunt unclean and resets the consecutive clean count. The record's
`review` has `accepted: true`, a `reviewer` outside the ten investigators and
nonempty `attestation`. Three consecutive source-current clean reviewed hunts
are required. IDs attest independence; the checker cannot establish who
operated an account or whether an evidence description is truthful.

## Readiness is a conjunction

`READY` requires every registered required check to pass with current sources,
adequate broad mutation detection, three full flat fuzz campaigns, and three
consecutive clean ten-scope independently reviewed hunts. Missing, stale,
blocked, malformed or unrecognized evidence keeps it false. A green result is
a stopping decision for this scoped campaign, not a proof that HARC has no
remaining bugs. No current campaign is claimed to satisfy this gate merely
because the infrastructure's synthetic positive-control unit test passes.
