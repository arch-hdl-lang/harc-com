#!/usr/bin/env python3
"""Source-bound, fail-closed scoped contract coverage and stop-gate evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
SHA = re.compile(r'^[0-9a-f]{64}$')
SCHEMA = 2
AREAS = {'parser', 'arithmetic', 'solver', 'control', 'components', 'clocks',
         'aggregates', 'coverage', 'CLI', 'RAL'}
TARGETS = ('check_cli', 'arithmetic_solver_conformance', 'native_signed_capture_e2e',
           'native_bool_capture_e2e', 'fatal_wall_wait_e2e', 'tbir_component_local_init',
           'tbir_component_sub_assign', 'static_cover_bins_e2e',
           'tbir_wide_cover_selectors_e2e', 'native_ral_assignment_e2e',
           'tbir_void_helper_e2e', 'temporal_helpers_e2e')
BOUND_DIRS = ('src', 'runtime', 'proofs', 'tests', 'tools', 'verification', 'scripts', 'stdlib', 'signatures', 'ci', '.cargo')
ROOT_FILES = ('Cargo.toml', 'Cargo.lock', 'build.rs', 'build_version.rs', 'rust-toolchain', 'rust-toolchain.toml')
PRODUCTION_DIRS = ('src/', 'runtime/', 'stdlib/', 'signatures/', '.cargo/')
# Pinned here, not read from JSON: editing the registry cannot weaken the gate.
STOP_GATE = dict(minimum_mutation_areas=8, minimum_mutation_count=30, minimum_mutation_percent=90,
                 minimum_distinct_fuzz_cases=10000, minimum_fuzz_seeds=3, flat_fuzz_campaigns=3,
                 clean_hunts=3, hunt_scopes=10)
CONTRACT_CHECKS = {
    'parser-boundaries': ['rust-lib', 'check_cli'],
    'arithmetic-kernels': ['arithmetic-formal', 'proof-runner'],
    'solver-capture': ['native_signed_capture_e2e', 'native_bool_capture_e2e', 'arithmetic_solver_conformance'],
    'control-flow': ['fatal_wall_wait_e2e', 'tbir_void_helper_e2e'],
    'component-ownership': ['tbir_component_local_init', 'tbir_component_sub_assign'],
    'clock-progress': ['fatal_wall_wait_e2e', 'temporal_helpers_e2e'],
    'aggregate-widths': ['rust-lib', 'tbir_wide_cover_selectors_e2e'],
    'coverage-ordering': ['static_cover_bins_e2e', 'tbir_wide_cover_selectors_e2e'],
    'cli-launch': ['rust-cli', 'check_cli'],
    'ral-assignment': ['native_ral_assignment_e2e']}
CONTRACT_OWNERS = {
    'parser-boundaries': ['src/parser.rs'],
    'arithmetic-kernels': ['runtime/harc_thread_rt.h', 'runtime/harc_random_rt.h', 'src/numeric_semantics.rs'],
    'solver-capture': ['src/codegen/randomize_renderer.rs', 'src/codegen/randomize.rs'],
    'control-flow': ['src/codegen/tbir/func.rs'],
    'component-ownership': ['src/ir/lower/components_impl.rs'],
    'clock-progress': ['runtime/harc_thread_rt.h', 'src/codegen/tbir/common.rs'],
    'aggregate-widths': ['src/codegen/tbir/expr.rs'],
    'coverage-ordering': ['src/codegen/tbir/covergroup.rs', 'src/ir/lower/covergroups.rs'],
    'cli-launch': ['src/main.rs'],
    'ral-assignment': ['src/ir/lower/regblock.rs', 'src/ir/lower/addrmap.rs']}
MISSING_FAMILIES = [
    'Complete parser grammar and diagnostic equivalence',
    'Whole expression lowering, SMT sort construction and solver emission',
    '32/64/128-bit full signed division and remainder equivalence',
    'Wide multiply/divide and arbitrary storage capacities',
    'Coroutine scheduling, fairness and unbounded temporal behavior',
    'Unbounded component graphs and arbitrary aggregate nesting',
    'Complete coverage cross-bin semantics',
    'Complete RAL bus/protocol and external DUT behavior',
    'Scoreboards, transactors and sequence (tseq) semantics',
    'Bus, addrmap and register-model behavior beyond single frontdoor assignment',
    'Temporal properties and assertions',
    'Randomization distribution and constraint solution quality']
NATIVE_TOOLS = ['clang++', 'verilator', 'z3']
FORMAL_INPUTS = {'runtime/harc_thread_rt.h', 'runtime/harc_random_rt.h', 'src/numeric_semantics.rs',
                 'proofs/arithmetic/run.py', 'proofs/arithmetic/u128.cpp', 'proofs/arithmetic/wide.cpp',
                 'proofs/arithmetic/rng.cpp', 'proofs/arithmetic/kani.rs'}
COMPILER_BUILD = ['cargo', 'build', '--offline', '--bin', 'harc']
CLOCK_SKEW = 300
LEDGERS = {kind: ROOT / 'target/verification-ledger' / (kind + '.jsonl') for kind in ('fuzz', 'hunt')}


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def record_digest(record):
    return hashlib.sha256(canonical(record)).hexdigest()


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def read_ledger(ledger):
    """Ledger lines in order: each campaign appears when it starts (sha256 null) and when it finishes."""
    path = Path(ledger)
    lines = path.read_text().splitlines() if path.exists() else []
    entries = [json.loads(line, object_pairs_hook=unique_keys) for line in lines if line.strip()]
    require(all(isinstance(e, dict) and nonempty_str(e.get('run_id')) and
                (e.get('sha256') is None or (isinstance(e['sha256'], str) and SHA.fullmatch(e['sha256'])))
                for e in entries), 'malformed campaign ledger')
    return entries


def ledger_link(entries):
    """Predecessor link for the next campaign, or (None, None) for the first."""
    if not entries:
        return None, None
    last = entries[-1]
    if last['sha256'] is None:
        # The last campaign started but never finished. Link to a digest no
        # record can have, so no streak continues across the abort.
        return last['run_id'], hashlib.sha256(('aborted:' + last['run_id']).encode()).hexdigest()
    return last['run_id'], last['sha256']


def append_ledger(ledger, record, path, finished=True, prepare=None):
    """Append one ledger line under an exclusive lock and return the predecessor link it saw.

    `prepare(entries)` runs under the same lock before the append and may return
    a replacement record, so reading the tail and appending cannot interleave
    with another campaign.
    """
    import fcntl  # POSIX only; imported here so inventory/report still work elsewhere.
    ledger = Path(ledger)
    ledger.parent.mkdir(parents=True, exist_ok=True)
    with ledger.open('a') as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        try:
            entries = read_ledger(ledger)
            if prepare is not None:
                record = prepare(entries)
            stream.write(json.dumps(dict(run_id=record['run_id'], sha256=record_digest(record) if finished else None,
                                         report=str(path))) + '\n')
            stream.flush()
        finally:
            fcntl.flock(stream, fcntl.LOCK_UN)
    return ledger_link(entries)


def validate_ledger(records, entries):
    """Supplied campaigns must be finished, adjacent in the ledger, and end at its tail."""
    order, final = [], {}
    for entry in entries:
        if entry['run_id'] not in final:
            order.append(entry['run_id'])
        final[entry['run_id']] = entry['sha256']
    for record in records:
        require(final.get(record['run_id']) == record_digest(record), 'campaign not finished in the ledger as supplied')
    positions = [order.index(r['run_id']) for r in records]
    require(positions == list(range(positions[0], positions[0] + len(records))),
            'campaigns not consecutive in the ledger')
    require(positions[-1] == len(order) - 1, 'a later campaign in the ledger was not supplied')


def require(condition, message):
    if not condition:
        raise ValueError(message)


def is_int(value):
    # bool is an int subclass and JSON floats such as 1.0 compare equal to ints.
    return type(value) is int


def nonempty_str(value):
    return isinstance(value, str) and bool(value.strip())


def unique_keys(pairs):
    keys = [k for k, _ in pairs]
    require(len(keys) == len(set(keys)), 'duplicate JSON key')
    return dict(pairs)


def load_json(path):
    """Parse JSON, rejecting duplicate keys (last-wins would hide a contradiction)."""
    return json.loads(Path(path).read_text(), object_pairs_hook=unique_keys)


def source_inputs(root):
    """Complete source/harness/test/registry binding; exclude generated outputs."""
    root = Path(root)
    paths = set()
    for directory in BOUND_DIRS:
        for p in (root / directory).rglob('*'):
            # rglob does not descend symlinked directories, so a link could hide inputs.
            require(not p.is_symlink(), 'symlink in source inputs: ' + p.relative_to(root).as_posix())
            if p.is_file() and '__pycache__' not in p.parts and p.suffix != '.pyc':
                paths.add(p)
    for name in ROOT_FILES:
        require(not (root / name).is_symlink(), 'symlink in source inputs: ' + name)
        if (root / name).is_file():
            paths.add(root / name)
    return {p.relative_to(root).as_posix(): digest(p) for p in sorted(paths)}


def production_digest(root, inputs=None):
    inputs = source_inputs(root) if inputs is None else inputs
    production = {k: v for k, v in inputs.items() if k.startswith(PRODUCTION_DIRS) or k in ROOT_FILES}
    require(any(k.startswith('src/') for k in production), 'missing production sources')
    return hashlib.sha256(canonical(production)).hexdigest()


def unique_strings(values, label, nonempty=True):
    require(isinstance(values, list) and (bool(values) or not nonempty), label)
    require(all(nonempty_str(x) for x in values), label)
    require(len(values) == len(set(values)), 'duplicate ' + label)
    return values


def validate_binding(evidence, root, inputs=None):
    inputs = source_inputs(root) if inputs is None else inputs
    require(isinstance(evidence, dict), 'evidence must be an object')
    require(is_int(evidence.get('schema_version')) and evidence['schema_version'] == SCHEMA, 'unknown evidence schema')
    require(evidence.get('complete') is True, 'incomplete evidence')
    require(is_int(evidence.get('completed_at')) and evidence['completed_at'] > 0, 'missing completion timestamp')
    require(evidence['completed_at'] <= time.time() + CLOCK_SKEW, 'completion timestamp in the future')
    require(nonempty_str(evidence.get('run_id')), 'missing run_id')
    require(evidence.get('inputs') == inputs, 'stale or incomplete source inputs')
    require(evidence.get('production_digest') == production_digest(root, inputs), 'stale production digest')


def validate_link(evidence):
    """A campaign names its ledger predecessor; null only for the first campaign ever run."""
    previous, previous_sha = evidence.get('previous_run_id'), evidence.get('previous_sha256')
    require('previous_run_id' in evidence and 'previous_sha256' in evidence, 'missing campaign ledger link')
    require((previous is None and previous_sha is None) or
            (nonempty_str(previous) and isinstance(previous_sha, str) and SHA.fullmatch(previous_sha)),
            'malformed campaign ledger link')


def validate_fuzz(evidence, root, inputs=None):
    validate_binding(evidence, root, inputs)
    require(evidence.get('kind') == 'fuzz', 'wrong fuzz kind')
    validate_link(evidence)
    seeds = evidence.get('seeds')
    require(isinstance(seeds, list) and len(seeds) >= STOP_GATE['minimum_fuzz_seeds'] and
            all(is_int(s) for s in seeds) and len(seeds) == len(set(seeds)), 'invalid fuzz seeds')
    ids = unique_strings(evidence.get('case_ids'), 'fuzz case IDs')
    require(all(SHA.fullmatch(x) for x in ids), 'case IDs must hash canonical tree and environment')
    require(is_int(evidence.get('distinct_cases')) and evidence['distinct_cases'] == len(ids), 'distinct case count mismatch')
    require(len(ids) >= STOP_GATE['minimum_distinct_fuzz_cases'], 'fuzz campaign below distinct case threshold')
    # Flat means failure-free: no failures and no failure signatures, known or new.
    require(evidence.get('failures') == [], 'fuzz failures or missing failures')
    require(evidence.get('failure_signatures') == [], 'fuzz failure signatures or missing signatures')
    compiler = evidence.get('compiler')
    require(isinstance(compiler, dict), 'missing compiler provenance')
    require(compiler.get('build_command') == COMPILER_BUILD, 'unknown compiler build command')
    require(isinstance(compiler.get('version'), str) and re.match(r'harc \d+\.\d+\.\d+', compiler['version']),
            'compiler version is not a harc version')
    for path_key, hash_key in (('path', 'sha256'), ('build_log', 'build_log_sha256')):
        require(isinstance(compiler.get(path_key), str) and Path(compiler[path_key]).is_absolute(), 'missing compiler artifact path')
        require(isinstance(compiler.get(hash_key), str) and SHA.fullmatch(compiler[hash_key]), 'missing compiler artifact digest')
        require(digest(compiler[path_key]) == compiler[hash_key], 'missing or changed compiler artifact')
    require(Path(compiler['path']).name in ('harc', 'harc.exe'), 'compiler artifact is not a harc binary')
    return evidence


def killed_log(log):
    """A kill must be visible in the raw log, not only declared in the record."""
    rust = re.findall(r'test result: FAILED\. \d+ passed; (\d+) failed;', log)
    unittest_failed = re.search(r'^FAILED \((?:failures|errors)=[1-9]', log, re.M)
    return any(int(n) > 0 for n in rust) or unittest_failed is not None


def validate_mutation(evidence, root, inputs=None):
    validate_binding(evidence, root, inputs)
    require(evidence.get('kind') == 'broad_mutation', 'wrong mutation kind')
    mutants = evidence.get('mutants')
    require(isinstance(mutants, list) and all(isinstance(m, dict) for m in mutants), 'malformed mutant records')
    require(len(mutants) >= STOP_GATE['minimum_mutation_count'], 'need at least 30 mutant records')
    unique_strings([m.get('id') for m in mutants], 'mutant IDs')
    for mutant in mutants:
        require(mutant.get('area') in AREAS, 'unknown mutant area')
        require(mutant.get('status') in ('killed', 'survived', 'invalid', 'blocked'), 'unknown mutant status')
        proof = mutant.get('evidence')
        require(isinstance(proof, dict), 'missing mutant evidence')
        require(isinstance(proof.get('command'), list) and proof['command'] and
                all(nonempty_str(x) for x in proof['command']), 'missing mutant command')
        require(isinstance(proof.get('log_sha256'), str) and SHA.fullmatch(proof['log_sha256']), 'missing mutant log digest')
        require(isinstance(proof.get('log'), str) and Path(proof['log']).is_absolute(), 'missing absolute mutant log path')
        require(digest(proof['log']) == proof['log_sha256'], 'missing or changed mutant log')
        require(nonempty_str(proof.get('reviewer')), 'unattested mutant')
        if mutant['status'] == 'killed':
            require(killed_log(Path(proof['log']).read_text(errors='replace')), 'killed mutant log shows no failing test')
    unique_strings([m['evidence']['log'] for m in mutants], 'mutant logs')
    # One genuine failing log copied under many names is not many kills.
    unique_strings([m['evidence']['log_sha256'] for m in mutants], 'mutant log contents')
    require(len({m['area'] for m in mutants}) >= STOP_GATE['minimum_mutation_areas'], 'mutation campaign needs at least eight areas')
    require(not any(m['status'] == 'blocked' for m in mutants), 'blocked mutation evidence')
    # Invalid mutants remain in the denominator and never count as kills.
    killed = sum(m['status'] == 'killed' for m in mutants)
    require(killed * 100 >= len(mutants) * STOP_GATE['minimum_mutation_percent'], 'mutation detection below 90 percent')
    return evidence


def validate_hunt(evidence, root, inputs=None):
    validate_binding(evidence, root, inputs)
    require(evidence.get('kind') == 'hunt', 'wrong hunt kind')
    validate_link(evidence)
    scopes = evidence.get('scopes')
    require(isinstance(scopes, list) and all(isinstance(s, dict) for s in scopes), 'malformed hunt scopes')
    require(len(scopes) == STOP_GATE['hunt_scopes'], 'hunt needs ten scopes')
    unique_strings([s.get('id') for s in scopes], 'scope IDs')
    require({s.get('area') for s in scopes} == AREAS, 'hunt must cover all ten registered areas')
    investigators = unique_strings([s.get('investigator') for s in scopes], 'investigator IDs')
    require(all(s.get('complete') is True for s in scopes), 'incomplete hunt scopes')
    require(all(nonempty_str(s.get('summary')) for s in scopes), 'missing scope evidence')
    candidates = evidence.get('candidates')
    require(isinstance(candidates, list) and all(isinstance(c, dict) for c in candidates), 'missing candidate ledger')
    unique_strings([c.get('id') for c in candidates], 'candidate IDs', False)
    owners = {s['id']: s['investigator'] for s in scopes}
    for candidate in candidates:
        require(candidate.get('scope_id') in owners, 'unknown candidate scope')
        require(nonempty_str(candidate.get('verifier')) and candidate['verifier'] != owners[candidate['scope_id']],
                'candidate needs independent verifier')
        require(candidate.get('classification') in ('invalid', 'rejected', 'duplicate', 'proven_fixed'), 'unresolved candidate')
        require(nonempty_str(candidate.get('evidence')), 'missing candidate evidence')
        # A fixed finding is not a clean hunt: it resets the consecutive streak.
        require(candidate['classification'] != 'proven_fixed', 'hunt found a proven bug')
    review = evidence.get('review')
    require(isinstance(review, dict) and review.get('accepted') is True and nonempty_str(review.get('reviewer'))
            and review['reviewer'] not in investigators and nonempty_str(review.get('attestation')),
            'missing independent hunt review')
    return evidence


def known_checks():
    # --nocapture keeps tests' own skip messages visible to parse_rust. The lib
    # suite's solver C++ compile checks use HARC_CXX, which defaults to c++.
    known = {'rust-lib': (['cargo', 'test', '--lib', '--', '--nocapture'], 'rust', ['c++']),
             'rust-cli': (['cargo', 'test', '--bin', 'harc', '--', '--nocapture'], 'rust', []),
             'proof-runner': (['python3', 'proofs/arithmetic/test_runner.py'], 'unittest', ['cbmc', 'kani']),
             'arithmetic-formal': (['python3', 'proofs/arithmetic/run.py', '--out', '{out}/formal'], 'formal', ['cbmc', 'kani', 'z3'])}
    known.update({t: (['cargo', 'test', '--test', t, '--', '--nocapture'], 'rust', NATIVE_TOOLS) for t in TARGETS})
    return known


def cbmc_obligations():
    cbmc = ['u128-' + s for s in ('mask', 'compare', 'shift', 'divzero')]
    cbmc += ['u128-divrem-' + str(n) for n in range(1, 9)] + ['u128-sext', 'u128-shift-count']
    cbmc += ['wide-' + op + '-' + str(n) for n in (1, 2, 4, 5, 8, 32) for op in ('arith', 'extend')]
    return cbmc + ['rng-' + s for s in ('full', 'half', 'small', 'degenerate')]


KANI_HARNESSES = ['const_add_sub', 'const_mul', 'const_wrap', 'const_compare', 'const_shift',
                  'const_bit_logic', 'const_bitwise_laws', 'const_wrapping_roundtrip', 'const_div_rem',
                  'const_errors', 'solver_promotion', 'solver_unknown_widths']


def load_registry(root, inputs=None):
    registry = load_json(Path(root) / 'verification/contracts.json')
    require(isinstance(registry, dict) and is_int(registry.get('schema_version')) and registry['schema_version'] == SCHEMA,
            'unknown registry schema')
    contracts, checks = registry.get('contracts'), registry.get('checks')
    require(isinstance(contracts, list) and all(isinstance(c, dict) for c in contracts), 'malformed contracts')
    require(isinstance(checks, list) and all(isinstance(c, dict) for c in checks), 'malformed checks')
    unique_strings([c.get('id') for c in contracts], 'contract IDs')
    require({c.get('area') for c in contracts} == AREAS and len(contracts) == len(AREAS), 'changed contract inventory requires checker review')
    unique_strings([c.get('id') for c in checks], 'check IDs')
    known = known_checks()
    require({c['id'] for c in checks} == set(known), 'changed required checks')
    for check in checks:
        command, parser, tools = known[check['id']]
        require((check.get('command'), check.get('parser')) == (command, parser), 'unrecognized check command')
        require(check.get('requires', []) == tools, 'changed check tool requirements')
        require(is_int(check.get('timeout')) and check['timeout'] > 0, 'invalid timeout')
    runner = next(c for c in checks if c['id'] == 'proof-runner')
    require(is_int(runner.get('expected_tests')) and runner['expected_tests'] == 10, 'runner test count drift')
    for contract in contracts:
        require(contract.get('required') is True and nonempty_str(contract.get('domain')), 'incomplete contract')
    require({c['id']: c.get('checks') for c in contracts} == CONTRACT_CHECKS, 'changed contract check ownership')
    require({c['id']: c.get('owners') for c in contracts} == CONTRACT_OWNERS, 'changed contract source owners')
    inputs = source_inputs(root) if inputs is None else inputs
    for owners in CONTRACT_OWNERS.values():
        require(all(owner in inputs for owner in owners), 'contract owner is not a bound source file')
    require(all(c.get('formal') is (c['id'] == 'arithmetic-kernels') for c in contracts), 'changed formal classification')
    require(registry.get('stop_gate') == STOP_GATE, 'changed stop-gate thresholds')
    require(registry.get('missing_families') == MISSING_FAMILIES, 'changed missing families requires checker review')
    require(registry.get('formal') == {'cbmc': cbmc_obligations(), 'kani': KANI_HARNESSES}, 'formal inventory drift')
    return registry


# A bare SKIP/skipping marker at the start of a line is how native tests report a missing tool.
SKIP_LINE = re.compile(r'^\s*(?:skip(?:ped|ping)?\b|unavailable\b)', re.I | re.M)


def parse_rust(log):
    summaries = re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;', log)
    require(summaries and all(s[0] == 'ok' and int(s[1]) > 0 and s[2:] == ('0', '0') for s in summaries), 'missing, failing, ignored, or vacuous Rust results')
    require(not SKIP_LINE.search(log) and not re.search(r'\b(skipp(?:ed|ing)|unavailable)\b', log, re.I),
            'skipped or unavailable Rust dependency')


def parse_unittest(log, expected):
    require(re.findall(r'^Ran (\d+) tests? in ', log, re.M) == [str(expected)], 'unexpected unittest count')
    require(re.search(r'^OK\s*$', log, re.M) is not None and not re.search(r'\b(skipped|FAILED|ERROR)\b', log), 'unittest did not pass cleanly')


def same_path(a, b):
    return isinstance(a, str) and Path(a).resolve() == Path(b).resolve()


def parse_formal(out, registry, root, inputs=None):
    formal_dir = Path(out) / 'formal'
    report = load_json(formal_dir / 'report.json')
    expected = registry['formal']['cbmc'] + ['rust']
    require(isinstance(report, dict), 'malformed formal report')
    results = report.get('results')
    require(isinstance(results, list) and all(isinstance(r, dict) for r in results), 'malformed formal results')
    require(len(results) == len(expected) and sorted(r.get('name') or '' for r in results) == sorted(expected), 'missing formal obligations')
    # proofs/arithmetic/run.py records passed/failed/timeout; only passed counts.
    require(all(r.get('status') == 'passed' for r in results), 'formal failure')
    for result in results:
        name, command = result['name'], result.get('command')
        require(isinstance(command, list) and all(isinstance(x, str) for x in command), 'missing formal command')
        log = (formal_dir / (name + '.log')).read_text()
        if name == 'rust':
            require(re.findall(r'Complete - (\d+) successfully verified harnesses, (\d+) failures, (\d+) total', log) == [('12', '0', '12')], 'incomplete Kani result')
            require([command[i + 1] for i, x in enumerate(command[:-1]) if x == '--harness'] == registry['formal']['kani'], 'wrong Kani harnesses')
            require(any(x == '--target-dir' and same_path(command[i + 1], formal_dir / 'kani-build')
                        for i, x in enumerate(command[:-1])), 'Kani result not from this run')
            continue
        # Tie each raw log to its own obligation: CBMC names the unit it checked.
        unit = formal_dir / (name + '.cpp')
        require(len(command) > 1 and same_path(command[1], unit), 'CBMC command is not for its obligation')
        messages = json.loads(log, object_pairs_hook=unique_keys)
        require(isinstance(messages, list) and all(isinstance(m, dict) for m in messages), 'unknown CBMC format')
        properties = [p for m in messages for p in m.get('result', [])]
        require(properties and all(isinstance(p, dict) and isinstance(p.get('property'), str) for p in properties), 'unknown CBMC property format')
        assertions = [p for p in properties if p['property'].startswith('main.assertion.')]
        require(any(m.get('cProverStatus') == 'success' for m in messages) and assertions and
                all(p.get('status') == 'SUCCESS' for p in properties), 'incomplete CBMC result')
        require(all(same_path((p.get('sourceLocation') or {}).get('file'), unit) for p in assertions), 'CBMC log is not for its obligation')
    require(isinstance(report.get('inputs'), dict) and report['inputs'], 'missing formal source binding')
    current = source_inputs(root) if inputs is None else inputs
    for name, sha in report['inputs'].items():
        require(current.get(name) == sha, 'stale formal input')
    require(FORMAL_INPUTS <= set(report['inputs']), 'incomplete formal source binding')
    return report


def run_process(command, root, timeout, env):
    try:
        process = subprocess.Popen(command, cwd=root, env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            output, _ = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            output, _ = process.communicate()
            return -1, output.decode(errors='replace'), 'timeout'
        return process.returncode, output.decode(errors='replace'), None
    except OSError as error:
        return -1, str(error), 'unavailable tool'


def guard_output(root, out):
    """Logs must not become source inputs or overwrite tracked source files."""
    root = Path(root).resolve()
    require(out != root and not root.is_relative_to(out), 'output overlaps source inputs')
    for name in BOUND_DIRS + ROOT_FILES:
        require(not out.is_relative_to(root / name), 'output overlaps source inputs')


def formal_artifacts(out):
    """Digests of the formal report and raw logs, so they cannot change after the run."""
    formal_dir = Path(out) / 'formal'
    files = sorted(formal_dir.glob('*.log')) + [formal_dir / 'report.json'] if formal_dir.is_dir() else []
    return {f.name: digest(f) for f in files if f.is_file()}


def run_checks(root, out):
    registry = load_registry(root)
    out = Path(out).resolve()
    guard_output(root, out)
    out.mkdir(parents=True, exist_ok=True)
    before = source_inputs(root)
    env = dict(os.environ, HARC_REQUIRE_VERILATOR='1')
    env.pop('HARC_NO_LEARN', None)
    tools = sorted({'cargo', 'python3'} | {t for c in registry['checks'] for t in c.get('requires', [])})
    report = dict(schema_version=SCHEMA, kind='checks', run_id=str(time.time_ns()), complete=False,
                  inputs=before, production_digest=production_digest(root, before), checks=[],
                  environment=dict(tools={t: shutil.which(t, path=env.get('PATH')) for t in tools},
                                   variables={k: env.get(k) for k in ('HARC_CXX', 'HARC_REQUIRE_VERILATOR', 'KANI_HOME')}))
    for check in registry['checks']:
        command = [part.replace('{out}', str(out)) for part in check['command']]
        status, reason = 'FAIL', None
        missing = [tool for tool in check.get('requires', []) if not shutil.which(tool)]
        code, log, error = (-1, 'missing tools: ' + ', '.join(missing), 'unavailable tool') if missing else run_process(command, root, check['timeout'], env)
        log_path = out / (check['id'] + '.log')
        log_path.write_text(log)
        try:
            require(error is None and code == 0, error or 'nonzero exit status')
            if check['parser'] == 'rust':
                parse_rust(log)
            elif check['parser'] == 'unittest':
                parse_unittest(log, check['expected_tests'])
            else:
                parse_formal(out, registry, root, before)
            require(source_inputs(root) == before, 'source changed during check')
            status = 'PASS'
        except (ValueError, KeyError, OSError, TypeError) as failure:
            reason = str(failure)
        entry = dict(id=check['id'], status=status, reason=reason,
                     command=command, exit_code=code, log=str(log_path), log_sha256=digest(log_path))
        if check['parser'] == 'formal':
            entry['artifacts'] = formal_artifacts(out)
        report['checks'].append(entry)
        print(check['id'] + ': ' + status + (' (' + reason + ')' if reason else ''), flush=True)
        (out / 'checks.json').write_text(json.dumps(report, indent=2) + '\n')
    report['complete'] = source_inputs(root) == before
    report['completed_at'] = int(time.time())
    (out / 'checks.json').write_text(json.dumps(report, indent=2) + '\n')
    return report


def evaluate_checks(root, registry, checks, inputs):
    """Return the set of passing check IDs; each check fails independently."""
    validate_binding(checks, root, inputs)
    require(checks.get('kind') == 'checks', 'wrong checks evidence kind')
    entries = checks.get('checks')
    require(isinstance(entries, list) and all(isinstance(c, dict) for c in entries), 'malformed check results')
    unique_strings([c.get('id') for c in entries], 'check results')
    expected = {c['id']: c for c in registry['checks']}
    require({c['id'] for c in entries} == set(expected), 'missing required checks')
    passed, failures = set(), []
    for entry in entries:
        try:
            require(entry.get('status') == 'PASS' and is_int(entry.get('exit_code')) and entry['exit_code'] == 0, 'failed')
            path = Path(entry['log'])
            require(digest(path) == entry.get('log_sha256'), 'missing or changed check log')
            check = expected[entry['id']]
            command = [p.replace('{out}', str(path.parent.resolve())) for p in check['command']]
            require(entry.get('command') == command, 'unrecognized result command')
            if check['parser'] == 'rust': parse_rust(path.read_text())
            elif check['parser'] == 'unittest': parse_unittest(path.read_text(), check['expected_tests'])
            else:
                artifacts = entry.get('artifacts')
                require(isinstance(artifacts, dict) and 'report.json' in artifacts and artifacts == formal_artifacts(path.parent),
                        'missing or changed formal artifacts')
                parse_formal(path.parent, registry, root, inputs)
            passed.add(entry['id'])
        except (ValueError, KeyError, OSError, TypeError) as error:
            failures.append('required check ' + entry['id'] + ': ' + str(error))
    return passed, failures


def validate_campaigns(kind, records, validator, minimum, root, inputs, ledger=None):
    require(len(records) >= minimum, 'missing campaigns')
    unique_strings([r.get('run_id') for r in records], kind + ' run IDs')
    for record in records: validator(record, root, inputs)
    for a, b in zip(records, records[1:]):
        require(a['completed_at'] < b['completed_at'], 'campaigns not strictly chronological')
    if kind in ('fuzz', 'hunt'):
        # The ledger link makes "consecutive" checkable: no campaign was left out in between.
        for a, b in zip(records, records[1:]):
            require(b['previous_run_id'] == a['run_id'] and b['previous_sha256'] == record_digest(a),
                    'campaigns not consecutive in the ledger')
        require(ledger is not None, 'missing campaign ledger')
        validate_ledger(records, read_ledger(ledger))
    if kind == 'fuzz':
        for i, a in enumerate(records):
            for b in records[i + 1:]:
                require(not set(a['seeds']) & set(b['seeds']), 'fuzz campaigns share seeds')
                require(set(a['case_ids']) != set(b['case_ids']), 'fuzz campaigns have identical case sets')


def evaluate(root, checks, evidence, ledgers=None):
    inputs = source_inputs(root)
    registry = load_registry(root, inputs)
    reasons, passed = [], set()
    try:
        passed, failures = evaluate_checks(root, registry, checks, inputs)
        reasons += failures
    except (ValueError, KeyError, OSError, TypeError) as error:
        reasons.append('checks: ' + str(error))
    groups = {'fuzz': [], 'hunt': [], 'broad_mutation': []}
    for item in evidence:
        if not isinstance(item, dict) or item.get('kind') not in groups:
            reasons.append('unknown evidence kind')
        else:
            groups[item['kind']].append(item)
    for kind, validator, minimum in [('fuzz', validate_fuzz, STOP_GATE['flat_fuzz_campaigns']),
                                     ('hunt', validate_hunt, STOP_GATE['clean_hunts']),
                                     ('broad_mutation', validate_mutation, 1)]:
        try:
            validate_campaigns(kind, groups[kind], validator, minimum, root, inputs, (ledgers or {}).get(kind))
        except (ValueError, KeyError, TypeError, OSError) as error:
            reasons.append(kind + ': ' + str(error))
    contracts = {c['id']: all(x in passed for x in c['checks']) for c in registry['contracts']}
    # Name the ledgers this verdict trusted, so a reviewer can audit them.
    trusted = {}
    for kind, ledger in sorted((ledgers or {}).items()):
        path = Path(ledger)
        trusted[kind] = dict(path=str(path.resolve()), sha256=digest(path) if path.is_file() else None,
                             lines=len(path.read_text().splitlines()) if path.is_file() else 0)
    return dict(schema_version=SCHEMA, READY=not reasons, ledgers=trusted,
                scope='registered contracts only; not source lines, language features or all possible bugs',
                contracts=dict(passing=sorted(k for k, ok in contracts.items() if ok), registered=len(contracts)),
                formal_contracts=[dict(id=c['id'], passing=contracts[c['id']], domain=c['domain'])
                                  for c in registry['contracts'] if c['formal']],
                reasons=reasons, missing_families=registry['missing_families'])


def link(ledger, record, path):
    """Record a finished hand-assembled hunt in its ledger; the predecessor comes from the ledger, not the caller."""
    # Fuzz campaigns are linked by verification_fuzz.py as they run; relinking
    # one afterwards could splice a failed campaign out of the chain.
    require(isinstance(record, dict) and record.get('kind') == 'hunt', 'link only records hunt campaigns')
    linked = {}

    def prepare(entries):
        require(record.get('run_id') not in {e['run_id'] for e in entries}, 'campaign already in the ledger')
        previous_run_id, previous_sha256 = ledger_link(entries)
        linked.update(record, previous_run_id=previous_run_id, previous_sha256=previous_sha256)
        Path(path).write_text(json.dumps(linked, indent=2) + '\n')
        return linked

    append_ledger(ledger, record, path, prepare=prepare)
    return linked


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['inventory', 'run', 'report', 'link'])
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--out', type=Path, default=ROOT / 'target/verification-coverage')
    parser.add_argument('--checks', type=Path)
    parser.add_argument('--evidence', type=Path, action='append', default=[])
    parser.add_argument('--record', type=Path, help='link: finished hunt record to link and update in place')
    parser.add_argument('--fuzz-ledger', type=Path, default=LEDGERS['fuzz'])
    parser.add_argument('--hunt-ledger', type=Path, default=LEDGERS['hunt'])
    args = parser.parse_args()
    try:
        if args.mode == 'inventory': result = load_registry(args.root)
        elif args.mode == 'run':
            result = run_checks(args.root, args.out)
        elif args.mode == 'link':
            require(args.record is not None, 'link needs --record')
            result = link(args.hunt_ledger, load_json(args.record), args.record)
            result = {k: result[k] for k in ('run_id', 'previous_run_id', 'previous_sha256')}
        else:
            checks = load_json(args.checks) if args.checks else {}
            result = evaluate(args.root, checks, [load_json(p) for p in args.evidence],
                              dict(fuzz=args.fuzz_ledger, hunt=args.hunt_ledger))
        print(json.dumps(result, indent=2))
        if args.mode == 'report': return 0 if result['READY'] else 1
        if args.mode == 'run': return 0 if result['complete'] and all(c['status'] == 'PASS' for c in result['checks']) else 1
        return 0
    except (ValueError, OSError, KeyError, TypeError) as error:
        print(json.dumps({'READY': False, 'error': str(error)}))
        return 1


if __name__ == '__main__':
    sys.exit(main())
