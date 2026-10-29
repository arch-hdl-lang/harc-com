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
`owners` (bound source files on the default TB-IR path), bounded domain and
required checks. The checker pins, in Python rather than JSON, the check
commands and tool requirements, each contract's checks and owners, the formal
obligation identities, the `missing_families` list and the stopping thresholds.
Every owner must be a file in the source binding. An inventory change therefore
requires checker review; editing the JSON alone cannot turn a report green.

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

`report` reads the campaign ledgers from `target/verification-ledger/fuzz.jsonl`
and `hunt.jsonl` by default (`--fuzz-ledger`, `--hunt-ledger` to override).

`run` captures commands, exit statuses and complete combined output in an output
directory, including paths outside the checkout. The output directory may not be
the checkout root, an ancestor of it, or inside any bound input. Every required
check runs; a missing dependency is a failed check. Native checks set
`HARC_REQUIRE_VERILATOR=1`; the runner removes a global `HARC_NO_LEARN` so
learning-store tests retain their intended behavior. Timeouts kill the
invocation's process group. Rust suites run with `--nocapture` so a test's own
skip message reaches the log. Each check's tool requirements are pinned: the
native targets need `clang++`, `verilator` and `z3`; the library suite needs
`c++` (its solver C++ compile checks use `HARC_CXX`, default `c++`); the proof
runner needs `cbmc` and `kani`; the formal run needs `cbmc`, `kani` and `z3`.
`checks.json` records the resolved tool paths and the relevant environment
variables. Rust output must contain a recognized nonzero
passing summary with zero failed and zero ignored tests; skipped or unavailable
output fails, including a bare `SKIP` line. The arithmetic runner's own suite
must report exactly ten clean unittest tests. Formal evidence must include all
30 expected CBMC obligation IDs and all 12 Kani harness IDs, each with the
`passed` status that `proofs/arithmetic/run.py` writes (`failed` and `timeout`
fail). Raw solver logs are reparsed: every CBMC log needs a successful
nonvacuous `main.assertion` result whose source location is that obligation's
own unit, and the Kani result must come from this run's build directory. A log
copied from another obligation fails. The digests of the formal `report.json`
and every raw `formal/*.log` are recorded with the check, so they cannot be
replaced after the run. Missing or unknown output formats fail
closed. `run` exits nonzero on any failure. `report` exits zero only when
`READY` is true.

The source binding hashes every file below `src`, `runtime`, `proofs`, `tests`,
`tools`, `verification`, `scripts`, `stdlib`, `signatures`, `ci` and `.cargo`,
plus Cargo manifests/lockfile, `build.rs`, `build_version.rs` and Rust toolchain
declarations. Python bytecode/cache files are excluded. A symlink anywhere in
the bound inputs is rejected, because a linked directory would otherwise escape
the hash. The binding includes the checker and registry themselves. Before and
after hashes detect changes during checks; reports require exact current
bindings and rehash stored raw logs. Rebuilding or editing any included input
invalidates old evidence. The production digest is a canonical SHA-256 of
production source/runtime/stdlib/signatures/config and root build-manifest
hashes. It lets search records name the precise production snapshot in addition
to the stricter full input binding.

All JSON the checker reads (registry, checks, evidence, formal report, CBMC
logs) is rejected if an object repeats a key. Integer fields must be JSON
integers: `true` or `2.0` is not accepted where an integer is required.

## Evidence schema, version 2

Every external record is an object containing `schema_version: 2`, `kind`, a
nonempty unique `run_id`, positive integer Unix `completed_at` no more than five
minutes in the future, `complete: true`, `inputs` equal to `source_inputs(root)`,
and `production_digest` equal to `production_digest(root)`. The helper functions
and `validate_binding`, `validate_fuzz`, `validate_mutation`, `validate_hunt`
are importable from `tools/verification_coverage.py`. Every supplied record must
be source-current; do not mix historical stale snapshots into a current report.
JSON records and identity fields are provenance attestations, not cryptographic
proof of execution, independence or identity.

### Campaign ledger

Fuzz and hunt campaigns are recorded in append-only ledgers (JSON lines, one
`run_id` and record `sha256` per line, written under a file lock). Each record
carries `previous_run_id` and `previous_sha256`: the run ID and canonical record
digest of the campaign that preceded it in the ledger, or both `null` for the
first campaign. The predecessor is always read from the ledger, never chosen by
the caller.

`report` requires the supplied campaigns of each kind to be, in order:

- linked to one another through `previous_run_id` and `previous_sha256`;
- each finished in the ledger with exactly the supplied record's digest, so
  a record edited or relinked after it was recorded is rejected;
- adjacent in the ledger, so a failed or aborted campaign in between breaks
  the streak;
- the last campaigns in the ledger, so a later campaign cannot be left out.

`tools/verification_fuzz.py` writes the fuzz ledger itself: it appends a line
when a campaign starts and another when it finishes, failed campaigns included.
A campaign that started but never finished leaves the next one linked to a
digest no record can have. Hunt records are assembled by hand. Record each
finished hunt, clean or not, in order with:

```sh
python3 tools/verification_coverage.py link --record hunt-2.json
```

`link` takes the predecessor from the hunt ledger, writes the link into the
record and appends it. It refuses fuzz records, which only the campaign script
may record, and refuses a run already in the ledger.

Limits: a ledger is a file. A campaign run against a different `--ledger` path,
in another checkout, or after the ledger was deleted is invisible to `report`.
Run all campaigns of one stop-gate attempt against the same ledgers, and keep
them. Whether every hunt was recorded is the coordinator's attestation.
The report output names each ledger it read, with its digest and line count.

Two consequences of these rules:

- A hunt record must be final, review included, before `link`. Editing it
  afterwards changes its digest, and it cannot be linked again, so the
  streak restarts.
- Every campaign on the default fuzz ledger counts, including small
  exploratory runs, which fail the case threshold and reset the streak.
  Point exploratory runs at a separate `--ledger`.

### Fuzz

A **fuzz** record has `kind: "fuzz"`, at least three distinct integer `seeds`,
at least 10,000 `distinct_cases`, matching unique SHA-256 `case_ids`, and the
ledger link. A case ID hashes the canonical tree and environment, excluding
backend, rendering, context and repeated observations. The checker verifies
count and uniqueness; the generator owns the canonicalization semantics. Each
campaign must independently reach 10,000 cases; combining three small campaigns
does not satisfy the threshold. Include:

```json
{"compiler": {"path": "/absolute/path/to/harc", "sha256": "<sha256>",
 "version": "<version output>",
 "build_command": ["cargo", "build", "--offline", "--bin", "harc"],
 "build_log": "/absolute/path/to/build.log", "build_log_sha256": "<sha256>"}}
```

The campaign script copies the freshly built `harc` into the campaign's output
directory and runs and records that snapshot, so a later rebuild of the shared
`target/debug/harc` does not invalidate the campaign. The checker rehashes
binary and build log, checks the exact build command, and requires a binary
named `harc` whose recorded version starts `harc X.Y.Z`. This
records build provenance; a hash alone cannot prove that a binary was built from
the declared source.

**Flat** means failure-free: each counted campaign has an empty `failures` list
and an empty `failure_signatures` list. A known, triaged failure still keeps a
campaign from counting. **Distinct** means no two supplied campaigns share any
seed, and no two have identical case-ID sets (overlap is allowed).
`verification_fuzz.py` requires `--seeds` and has no default, because three
default runs would be identical campaigns. A final report needs the three most
recent fuzz campaigns in the ledger to be chained, flat and distinct.

### Broad mutation

A **broad mutation** record has `kind: "broad_mutation"` and `mutants`, at least
30 uniquely identified records across at least eight of the ten registered
areas. Each has `id`, `area`, `status` and `evidence`. Status is `killed`,
`survived`, `invalid` or `blocked`. Evidence contains `command` (nonempty string
array), `log` (absolute path, one per mutant), `log_sha256` (checked against the
file) and `reviewer` (nonempty identity attestation). A `killed` mutant's log
must show a failing test: a Rust `test result: FAILED` with at least one
failure, or a unittest `FAILED (failures=N)` / `FAILED (errors=N)`. No two
mutants may share log contents; have the mutation harness write each mutant's
ID into its log so two genuine kills cannot collide. A status alone is not a kill, and one failing
log copied under several names is not several kills. At least 90% of **all**
registered mutants must be killed. Invalid mutants do not count as kills, and
any blocked result fails the gate. Ten
arithmetic-only harness sensitivity mutants are useful guards but cannot satisfy
this broad semantic mutation gate. Mutation selection, semantic validity and the
quality of kill explanations remain independent review responsibilities.

### Hunt

A **hunt** record has `kind: "hunt"`, the ledger link, exactly ten completed
`scopes`, one per registered `area`, with unique `id` and `investigator`
identities and nonempty `summary`, plus a `candidates` array (explicitly empty
when no candidates). Each candidate has unique `id`, `scope_id`, `verifier`
different from its scope investigator, `classification` and nonempty `evidence`
describing/linking the independent reproduction or rejection. Classifications
are `invalid`, `rejected`, `duplicate` or `proven_fixed`. Unresolved/proven-unfixed
reports fail. A proven fixed bug also makes that hunt unclean and resets the
consecutive clean count. The record's `review` has `accepted: true`, a
`reviewer` outside the ten investigators and nonempty `attestation`. The three
most recent hunts in the hunt ledger must be chained, source-current, clean and
reviewed, and no two of them may have identical `scopes`, `candidates` and
`review` together: a hunt copied under a new run ID is not another hunt. This
catches copies, not near-copies; that the hunts were separate work remains the
coordinator's attestation. IDs attest
independence; the checker cannot establish who operated an account or whether
an evidence description is truthful. Scope summaries and candidate evidence
are free text and are not reparsed.

## Readiness is a conjunction

`READY` requires every registered required check to pass with current sources,
adequate broad mutation detection, the last three fuzz campaigns in the ledger
chained, flat and distinct, and the last three hunts in the ledger chained,
distinct, clean, ten-scope and independently reviewed. Missing, stale,
blocked, malformed or unrecognized evidence keeps it false.

The report lists which contracts have all their checks passing (`contracts`),
the formal contract with its exact bounded domain (`formal_contracts`), every
reason READY is false, and `missing_families`. Each check is judged on its own,
so one failing check uncovers only the contracts that depend on it. A green
result is a stopping decision for this scoped campaign, not a proof that HARC
has no remaining bugs. No current campaign is claimed to satisfy this gate
merely because the infrastructure's synthetic positive-control unit test passes.
