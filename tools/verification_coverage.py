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
AREAS = {'parser', 'arithmetic', 'solver', 'control', 'components', 'clocks',
         'aggregates', 'coverage', 'CLI', 'RAL'}
TARGETS = ('check_cli', 'arithmetic_solver_conformance', 'native_signed_capture_e2e',
           'native_bool_capture_e2e', 'fatal_wall_wait_e2e', 'tbir_component_local_init',
           'tbir_component_sub_assign', 'static_cover_bins_e2e',
           'tbir_wide_cover_selectors_e2e', 'native_ral_assignment_e2e',
           'tbir_void_helper_e2e', 'temporal_helpers_e2e')


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def source_inputs(root):
    """Complete source/harness/test/registry binding; exclude generated outputs."""
    root = Path(root)
    paths = set()
    for directory in ('src', 'runtime', 'proofs', 'tests', 'tools', 'verification', 'scripts', 'stdlib', 'signatures', 'ci', '.cargo'):
        paths.update(p for p in (root / directory).rglob('*') if p.is_file()
                     and '__pycache__' not in p.parts and p.suffix != '.pyc')
    paths.update(root / name for name in ('Cargo.toml', 'Cargo.lock', 'build.rs', 'rust-toolchain', 'rust-toolchain.toml')
                 if (root / name).is_file())
    return {p.relative_to(root).as_posix(): digest(p) for p in sorted(paths)}


def production_digest(root):
    inputs = source_inputs(root)
    production = {k: v for k, v in inputs.items() if k.startswith(('src/', 'runtime/', 'stdlib/', 'signatures/', '.cargo/'))
                  or k in ('Cargo.toml', 'Cargo.lock', 'build.rs', 'rust-toolchain', 'rust-toolchain.toml')}
    require(any(k.startswith('src/') for k in production), 'missing production sources')
    return hashlib.sha256(canonical(production)).hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def unique_strings(values, label, nonempty=True):
    require(isinstance(values, list) and (bool(values) or not nonempty), label)
    require(all(isinstance(x, str) and x.strip() for x in values), label)
    require(len(values) == len(set(values)), 'duplicate ' + label)
    return values


def validate_binding(evidence, root):
    require(isinstance(evidence, dict), 'evidence must be an object')
    require(evidence.get('schema_version') == 1, 'unknown evidence schema')
    require(evidence.get('complete') is True, 'incomplete evidence')
    require(type(evidence.get('completed_at')) is int and evidence['completed_at'] > 0, 'missing completion timestamp')
    require(isinstance(evidence.get('run_id'), str) and evidence['run_id'].strip(), 'missing run_id')
    require(evidence.get('inputs') == source_inputs(root), 'stale or incomplete source inputs')
    require(evidence.get('production_digest') == production_digest(root), 'stale production digest')


def validate_fuzz(evidence, root):
    validate_binding(evidence, root)
    require(evidence.get('kind') == 'fuzz', 'wrong fuzz kind')
    seeds = evidence.get('seeds')
    require(isinstance(seeds, list) and len(seeds) >= 3 and
            all(type(s) is int for s in seeds) and len(seeds) == len(set(seeds)), 'invalid fuzz seeds')
    ids = unique_strings(evidence.get('case_ids'), 'fuzz case IDs')
    require(all(SHA.fullmatch(x) for x in ids), 'case IDs must hash canonical tree and environment')
    require(type(evidence.get('distinct_cases')) is int and evidence['distinct_cases'] == len(ids) and len(ids) >= 10000,
            'distinct case count mismatch')
    require(evidence.get('failures') == [], 'fuzz failures or missing failures')
    unique_strings(evidence.get('failure_signatures'), 'failure signatures', False)
    compiler = evidence.get('compiler')
    require(isinstance(compiler, dict), 'missing compiler provenance')
    require(compiler.get('build_command') == ['cargo', 'build', '--offline', '--bin', 'harc'], 'unknown compiler build command')
    require(isinstance(compiler.get('version'), str) and compiler['version'].strip(), 'missing compiler version')
    for path_key, hash_key in (('path', 'sha256'), ('build_log', 'build_log_sha256')):
        require(isinstance(compiler.get(path_key), str) and Path(compiler[path_key]).is_absolute(), 'missing compiler artifact path')
        require(isinstance(compiler.get(hash_key), str) and SHA.fullmatch(compiler[hash_key]), 'missing compiler artifact digest')
        require(digest(compiler[path_key]) == compiler[hash_key], 'missing or changed compiler artifact')
    return evidence


def validate_mutation(evidence, root):
    validate_binding(evidence, root)
    require(evidence.get('kind') == 'broad_mutation', 'wrong mutation kind')
    mutants = evidence.get('mutants')
    require(isinstance(mutants, list) and len(mutants) >= 30 and all(isinstance(m, dict) for m in mutants), 'need at least 30 mutant records')
    unique_strings([m.get('id') for m in mutants], 'mutant IDs')
    for mutant in mutants:
        require(mutant.get('area') in AREAS, 'unknown mutant area')
        require(mutant.get('status') in ('killed', 'survived', 'invalid', 'blocked'), 'unknown mutant status')
        proof = mutant.get('evidence')
        require(isinstance(proof, dict), 'missing mutant evidence')
        require(isinstance(proof.get('command'), list) and proof['command'] and
                all(isinstance(x, str) for x in proof['command']), 'missing mutant command')
        require(isinstance(proof.get('log_sha256'), str) and SHA.fullmatch(proof['log_sha256']), 'missing mutant log digest')
        require(isinstance(proof.get('log'), str) and Path(proof['log']).is_absolute(), 'missing absolute mutant log path')
        require(digest(proof['log']) == proof['log_sha256'], 'missing or changed mutant log')
        require(isinstance(proof.get('reviewer'), str) and proof['reviewer'].strip(), 'unattested mutant')
    require(len({m['area'] for m in mutants}) >= 8, 'mutation campaign needs at least eight areas')
    # Invalid/blocked mutants remain in the denominator and never count as kills.
    killed = sum(m['status'] == 'killed' for m in mutants)
    require(killed * 100 >= len(mutants) * 90, 'mutation detection below 90 percent')
    require(not any(m['status'] == 'blocked' for m in mutants), 'blocked mutation evidence')
    return evidence


def validate_hunt(evidence, root):
    validate_binding(evidence, root)
    require(evidence.get('kind') == 'hunt', 'wrong hunt kind')
    scopes = evidence.get('scopes')
    require(isinstance(scopes, list) and len(scopes) == 10 and all(isinstance(s, dict) for s in scopes), 'hunt needs ten scopes')
    unique_strings([s.get('id') for s in scopes], 'scope IDs')
    require({s.get('area') for s in scopes} == AREAS, 'hunt must cover all ten registered areas')
    investigators = unique_strings([s.get('investigator') for s in scopes], 'investigator IDs')
    require(all(s.get('complete') is True for s in scopes), 'incomplete hunt scopes')
    require(all(isinstance(s.get('summary'), str) and s['summary'].strip() for s in scopes), 'missing scope evidence')
    candidates = evidence.get('candidates')
    require(isinstance(candidates, list) and all(isinstance(c, dict) for c in candidates), 'missing candidate ledger')
    unique_strings([c.get('id') for c in candidates], 'candidate IDs', False)
    for candidate in candidates:
        require(candidate.get('scope_id') in {s['id'] for s in scopes}, 'unknown candidate scope')
        owner = scopes[[s['id'] for s in scopes].index(candidate['scope_id'])]['investigator']
        require(isinstance(candidate.get('verifier'), str) and candidate['verifier'].strip()
                and candidate['verifier'] != owner, 'candidate needs independent verifier')
        require(candidate.get('classification') in ('invalid', 'rejected', 'duplicate', 'proven_fixed'), 'unresolved candidate')
        require(isinstance(candidate.get('evidence'), str) and candidate['evidence'].strip(), 'missing candidate evidence')
        # A fixed finding is not a clean hunt: it resets the consecutive streak.
        require(candidate['classification'] != 'proven_fixed', 'hunt found a proven bug')
    review = evidence.get('review')
    require(isinstance(review, dict) and review.get('accepted') is True and
            isinstance(review.get('reviewer'), str) and review['reviewer'].strip()
            and review['reviewer'] not in investigators and
            isinstance(review.get('attestation'), str) and review['attestation'].strip(), 'missing independent hunt review')
    return evidence


def load_registry(root):
    registry = json.loads((Path(root) / 'verification/contracts.json').read_text())
    require(registry.get('schema_version') == 1, 'unknown registry schema')
    contracts, checks = registry.get('contracts', []), registry.get('checks', [])
    require(isinstance(contracts, list) and all(isinstance(c, dict) for c in contracts), 'malformed contracts')
    require(isinstance(checks, list) and all(isinstance(c, dict) for c in checks), 'malformed checks')
    unique_strings([c.get('id') for c in contracts], 'contract IDs')
    require({c.get('area') for c in contracts} == AREAS and len(contracts) == 10, 'changed contract inventory requires checker review')
    unique_strings([c.get('id') for c in checks], 'check IDs')
    known = {'rust-lib': (['cargo', 'test', '--lib'], 'rust'),
             'rust-cli': (['cargo', 'test', '--bin', 'harc'], 'rust'),
             'proof-runner': (['python3', 'proofs/arithmetic/test_runner.py'], 'unittest'),
             'arithmetic-formal': (['python3', 'proofs/arithmetic/run.py', '--out', '{out}/formal'], 'formal')}
    known.update({t: (['cargo', 'test', '--test', t, '--', '--nocapture'], 'rust') for t in TARGETS})
    require({c['id'] for c in checks} == set(known), 'changed required checks')
    for check in checks:
        require((check.get('command'), check.get('parser')) == known[check['id']], 'unrecognized check command')
        require(type(check.get('timeout')) is int and check['timeout'] > 0, 'invalid timeout')
    require(next(c for c in checks if c['id'] == 'proof-runner').get('expected_tests') == 10, 'runner test count drift')
    for contract in contracts:
        require(contract.get('required') is True and contract.get('owner') and contract.get('domain'), 'incomplete contract')
        require(contract.get('checks') and set(contract['checks']) <= set(known), 'unknown contract check')
    required_contracts = {
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
    require({c['id']: c['checks'] for c in contracts} == required_contracts, 'changed contract check ownership')
    require(all(c.get('formal') is (c['id'] == 'arithmetic-kernels') for c in contracts), 'changed formal classification')
    require(registry.get('stop_gate') == dict(minimum_mutation_areas=8, minimum_mutation_count=30,
            minimum_mutation_percent=90, minimum_distinct_fuzz_cases=10000, minimum_fuzz_seeds=3,
            flat_fuzz_campaigns=3, clean_hunts=3, hunt_scopes=10), 'changed stop-gate thresholds')
    cbmc = ['u128-' + s for s in ('mask', 'compare', 'shift', 'divzero')]
    cbmc += ['u128-divrem-' + str(n) for n in range(1, 9)] + ['u128-sext', 'u128-shift-count']
    cbmc += ['wide-' + op + '-' + str(n) for n in (1, 2, 4, 5, 8, 32) for op in ('arith', 'extend')]
    cbmc += ['rng-' + s for s in ('full', 'half', 'small', 'degenerate')]
    kani = ['const_add_sub', 'const_mul', 'const_wrap', 'const_compare', 'const_shift',
            'const_bit_logic', 'const_bitwise_laws', 'const_wrapping_roundtrip', 'const_div_rem',
            'const_errors', 'solver_promotion', 'solver_unknown_widths']
    require(registry.get('formal') == {'cbmc': cbmc, 'kani': kani}, 'formal inventory drift')
    return registry


def parse_rust(log):
    summaries = re.findall(r'test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored;', log)
    require(summaries and all(s[0] == 'ok' and int(s[1]) > 0 and s[2:] == ('0', '0') for s in summaries), 'missing, failing, ignored, or vacuous Rust results')
    require(not re.search(r'\b(skipp(?:ed|ing)|unavailable)\b', log, re.I), 'skipped or unavailable Rust dependency')


def parse_unittest(log, expected):
    require(re.findall(r'^Ran (\d+) tests? in ', log, re.M) == [str(expected)], 'unexpected unittest count')
    require(re.search(r'^OK\s*$', log, re.M) is not None and not re.search(r'\b(skipped|FAILED|ERROR)\b', log), 'unittest did not pass cleanly')


def parse_formal(out, registry, root):
    report = json.loads((out / 'formal/report.json').read_text())
    expected = registry['formal']['cbmc'] + ['rust']
    require(isinstance(report, dict), 'malformed formal report')
    results = report.get('results', [])
    require(isinstance(results, list) and all(isinstance(r, dict) for r in results), 'malformed formal results')
    require(len(results) == len(expected) and {r.get('name') for r in results} == set(expected), 'missing formal obligations')
    require(all(r.get('status') == 'PASS' for r in results), 'formal failure')
    # Reparse raw solver output, not merely the report status.
    for result in results:
        log = (out / ('formal/' + result['name'] + '.log')).read_text()
        if result['name'] == 'rust':
            require(re.findall(r'Complete - (\d+) successfully verified harnesses, (\d+) failures, (\d+) total', log) == [('12', '0', '12')], 'incomplete Kani result')
            cmd = result.get('command', [])
            require([cmd[i + 1] for i, x in enumerate(cmd[:-1]) if x == '--harness'] == registry['formal']['kani'], 'wrong Kani harnesses')
        else:
            messages = json.loads(log)
            require(isinstance(messages, list) and all(isinstance(m, dict) for m in messages), 'unknown CBMC format')
            properties = [p for m in messages for p in m.get('result', [])]
            require(all(isinstance(p, dict) for p in properties), 'unknown CBMC property format')
            require(any(m.get('cProverStatus') == 'success' for m in messages) and properties and
                    any(p.get('property', '').startswith('main.assertion.') for p in properties) and
                    all(p.get('status') == 'SUCCESS' for p in properties), 'incomplete CBMC result')
    require(isinstance(report.get('inputs'), dict) and report['inputs'], 'missing formal source binding')
    current = source_inputs(root)
    for name, sha in report['inputs'].items():
        require(current.get(name) == sha, 'stale formal input')
    required = {'runtime/harc_thread_rt.h', 'runtime/harc_random_rt.h', 'src/numeric_semantics.rs',
                'proofs/arithmetic/run.py', 'proofs/arithmetic/u128.cpp', 'proofs/arithmetic/wide.cpp',
                'proofs/arithmetic/rng.cpp', 'proofs/arithmetic/kani.rs'}
    require(required <= set(report['inputs']), 'incomplete formal source binding')
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


def run_checks(root, out):
    registry = load_registry(root)
    out = Path(out).resolve()
    # Logs must not become source inputs or overwrite tracked source files.
    for name in ('src', 'runtime', 'proofs', 'tests', 'tools', 'verification'):
        require(not out.is_relative_to(Path(root).resolve() / name), 'output overlaps source inputs')
    out.mkdir(parents=True, exist_ok=True)
    before = source_inputs(root)
    report = dict(schema_version=1, kind='checks', run_id=str(time.time_ns()), complete=False,
                  inputs=before, production_digest=production_digest(root), checks=[])
    env = dict(os.environ, HARC_REQUIRE_VERILATOR='1')
    env.pop('HARC_NO_LEARN', None)
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
                formal = parse_formal(out, registry, root)
                require(all(before.get(k) == v for k, v in formal['inputs'].items()), 'stale formal report')
            require(source_inputs(root) == before, 'source changed during check')
            status = 'PASS'
        except (ValueError, KeyError, OSError, TypeError) as failure:
            reason = str(failure)
        report['checks'].append(dict(id=check['id'], status=status, reason=reason,
                                     command=command, exit_code=code, log=str(log_path), log_sha256=digest(log_path)))
        print(check['id'] + ': ' + status + (' (' + reason + ')' if reason else ''), flush=True)
        (out / 'checks.json').write_text(json.dumps(report, indent=2) + '\n')
    report['complete'] = source_inputs(root) == before
    report['completed_at'] = int(time.time())
    (out / 'checks.json').write_text(json.dumps(report, indent=2) + '\n')
    return report


def evaluate(root, checks, evidence):
    registry = load_registry(root)
    reasons, covered, formal_covered = [], [], []
    try:
        validate_binding(checks, root)
        require(checks.get('kind') == 'checks', 'wrong checks evidence kind')
        entries = checks.get('checks', [])
        require(isinstance(entries, list) and all(isinstance(c, dict) for c in entries), 'malformed check results')
        unique_strings([c.get('id') for c in entries], 'check results')
        require({c['id'] for c in entries} == {c['id'] for c in registry['checks']}, 'missing required checks')
        expected = {c['id']: c for c in registry['checks']}
        for entry in entries:
            require(entry.get('status') == 'PASS' and entry.get('exit_code') == 0, 'required check failed: ' + entry['id'])
            path = Path(entry['log'])
            require(digest(path) == entry.get('log_sha256'), 'missing or changed check log')
            check = expected[entry['id']]
            command = [p.replace('{out}', str(path.parent.resolve())) for p in check['command']]
            require(entry.get('command') == command, 'unrecognized result command')
            if check['parser'] == 'rust': parse_rust(path.read_text())
            elif check['parser'] == 'unittest': parse_unittest(path.read_text(), 10)
            else: parse_formal(path.parent, registry, root)
        covered = [c['id'] for c in registry['contracts']]
        formal_covered = [c['id'] for c in registry['contracts'] if c['formal']]
    except (ValueError, KeyError, OSError, TypeError) as error:
        reasons.append('checks: ' + str(error))
    groups = {'fuzz': [], 'hunt': [], 'broad_mutation': []}
    for item in evidence:
        if not isinstance(item, dict) or item.get('kind') not in groups:
            reasons.append('unknown evidence kind')
        else:
            groups[item['kind']].append(item)
    for kind, validator, minimum in [('fuzz', validate_fuzz, 3), ('hunt', validate_hunt, 3), ('broad_mutation', validate_mutation, 1)]:
        records = groups[kind]
        try:
            require(len(records) >= minimum, 'missing campaigns')
            unique_strings([r.get('run_id') for r in records], kind + ' run IDs')
            for record in records: validator(record, root)
            require(all(a['completed_at'] < b['completed_at'] for a, b in zip(records, records[1:])), 'campaigns not strictly chronological')
            # Caller supplies chronological history, and all supplied records must be current.
            if kind == 'fuzz':
                last = records[-3:]
                require(set(last[0]['failure_signatures']) == set(last[1]['failure_signatures']) == set(last[2]['failure_signatures']), 'last three fuzz campaigns not flat')
        except (ValueError, KeyError, TypeError, OSError) as error:
            reasons.append(kind + ': ' + str(error))
    return dict(schema_version=1, READY=not reasons, coverage=dict(covered=len(covered), registered=len(registry['contracts'])),
                formal_coverage=dict(covered=len(formal_covered), registered=sum(c['formal'] for c in registry['contracts'])),
                reasons=reasons, missing_families=registry['missing_families'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('mode', choices=['inventory', 'run', 'report'])
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--out', type=Path, default=ROOT / 'target/verification-coverage')
    parser.add_argument('--checks', type=Path)
    parser.add_argument('--evidence', type=Path, action='append', default=[])
    args = parser.parse_args()
    try:
        if args.mode == 'inventory': result = load_registry(args.root)
        elif args.mode == 'run':
            result = run_checks(args.root, args.out)
        else:
            checks = json.loads(args.checks.read_text()) if args.checks else {}
            result = evaluate(args.root, checks, [json.loads(p.read_text()) for p in args.evidence])
        print(json.dumps(result, indent=2))
        if args.mode == 'report': return 0 if result['READY'] else 1
        if args.mode == 'run': return 0 if result['complete'] and all(c['status'] == 'PASS' for c in result['checks']) else 1
        return 0
    except (ValueError, OSError, KeyError, TypeError) as error:
        print(json.dumps({'READY': False, 'error': str(error)}))
        return 1


if __name__ == '__main__':
    sys.exit(main())
