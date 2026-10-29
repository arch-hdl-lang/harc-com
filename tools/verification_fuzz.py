#!/usr/bin/env python3
"""Seeded compiler/oracle campaign; count distinct trees and environments only."""
import argparse
import dataclasses
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tests/conformance'))
import expression_models as expressions


def case_id(tree, environment):
    value = {'tree': tree, 'environment': environment}
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def write_report(path, report):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(report, indent=2) + '\n')
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    # No default: three default runs would be identical campaigns, which the gate rejects.
    parser.add_argument('--seeds', nargs='+', type=lambda s: int(s, 0), required=True,
                        help='at least three seeds, disjoint from every other campaign\'s seeds')
    parser.add_argument('--ledger', type=Path, default=None,
                        help='append-only campaign ledger that links each run to its predecessor '
                             '(default: the one the stop-gate report reads)')
    parser.add_argument('--trees', type=int, default=1024, help='random trees per seed, before fixed controls')
    parser.add_argument('--batch', type=int, default=64)
    parser.add_argument('--min-cases', type=int, default=10000,
                        help='small exploratory runs never satisfy the independent stop gate')
    args = parser.parse_args()
    if not __debug__:
        parser.error('Python optimization disables oracle guards')
    if len(set(args.seeds)) != len(args.seeds) or len(args.seeds) < 3:
        parser.error('at least three distinct seeds are required')
    if min(args.trees, args.batch, args.min_cases) <= 0:
        parser.error('tree, batch and case counts must be positive')
    from verification_coverage import LEDGERS, SCHEMA, append_ledger, production_digest, source_inputs
    expressions.self_test()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    ledger = (args.ledger or LEDGERS['fuzz']).resolve()
    inputs = source_inputs(ROOT)
    report = dict(schema_version=SCHEMA, kind='fuzz', inputs=inputs,
                  production_digest=production_digest(ROOT), run_id=str(uuid.uuid4()),
                  previous_run_id=None, previous_sha256=None,
                  seeds=args.seeds, random_trees_per_seed=args.trees, batch_size=args.batch,
                  distinct_cases=0, case_ids=[], oracle_observations=0, batches=[],
                  failures=[], failure_signatures=[], complete=False)
    path = out / 'report.json'
    # Every campaign is logged when it starts and again when it finishes,
    # failed ones included, so the stop gate's consecutive-campaign check
    # cannot skip a failed or aborted campaign.
    previous_run_id, previous_sha256 = append_ledger(ledger, report, path, finished=False)
    report.update(previous_run_id=previous_run_id, previous_sha256=previous_sha256)
    write_report(path, report)
    build_command = ['cargo', 'build', '--offline', '--bin', 'harc']
    with (out / 'build.log').open('w') as log:
        built = subprocess.run(build_command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
    if built.returncode:
        report['failures'].append({'phase': 'build', 'exit': built.returncode})
        report['completed_at'] = int(time.time())
        write_report(path, report)
        append_ledger(ledger, report, path)
        return 1
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--offline', '--no-deps', '--format-version', '1'], cwd=ROOT, text=True))
    debug_binary = Path(metadata['target_directory']) / 'debug' / ('harc.exe' if os.name == 'nt' else 'harc')
    # Snapshot the binary: later rebuilds of the shared target/debug/harc must
    # not invalidate this campaign's provenance.
    harc = out / debug_binary.name
    shutil.copy2(debug_binary, harc)
    report['compiler'] = dict(path=str(harc.resolve()), sha256=hashlib.sha256(harc.read_bytes()).hexdigest(),
                              version=subprocess.check_output([harc, '--version'], text=True).strip(),
                              build_command=build_command, build_log=str(out / 'build.log'),
                              build_log_sha256=hashlib.sha256((out / 'build.log').read_bytes()).hexdigest())
    passed_ids, generated_ids, signatures = set(), set(), set()
    for seed in args.seeds:
        trees = expressions.trees(seed, args.trees)
        for offset in range(0, len(trees), args.batch):
            directory = out / f'seed-{seed}' / f'batch-{offset // args.batch:04d}'
            directory.mkdir(parents=True)
            (directory / 'dut.sv').write_text('module Top(input logic clk); endmodule\n')
            expressions.generate_expressions(directory, seed, trees[offset:offset + args.batch])
            cases = json.loads((directory / 'expression_cases.json').read_text())
            identities = {case_id(c['tree'], c['environment']) for c in cases}
            generated_ids.update(identities)
            row = dict(seed=seed, offset=offset, artifacts=str(directory), distinct_cases=len(identities))
            try:
                expressions.native_expressions(directory, harc, backends=('tbir',))
                passed_ids.update(identities)
                count = len(json.loads((directory / 'expression_expected.json').read_text()))
                report['oracle_observations'] += count
                row.update(status='passed', oracle_observations=count)
            except (AssertionError, RuntimeError, ValueError, OSError) as error:
                # Keep the original log, source and oracle: a failed batch is
                # never counted as exercised cases, nor silently filtered out.
                message = str(error)
                signature = hashlib.sha256((type(error).__name__ + ':' + message).encode()).hexdigest()
                signatures.add(signature)
                failure = dict(seed=seed, offset=offset, artifacts=str(directory),
                               signature=signature, error=message)
                report['failures'].append(failure)
                row.update(status='failed', signature=signature)
            report['batches'].append(row)
            report.update(case_ids=sorted(passed_ids), distinct_cases=len(passed_ids),
                          generated_distinct_cases=len(generated_ids), failure_signatures=sorted(signatures))
            write_report(path, report)
            print(json.dumps(row), flush=True)
    report.update(completed_at=int(time.time()), complete=source_inputs(ROOT) == inputs)
    if not report['complete']:
        report['failures'].append({'phase': 'source-binding', 'error': 'inputs changed during campaign'})
    write_report(path, report)
    append_ledger(ledger, report, path)
    return int(not report['complete'] or bool(report['failures']) or report['distinct_cases'] < args.min_cases)


if __name__ == '__main__':
    sys.exit(main())
