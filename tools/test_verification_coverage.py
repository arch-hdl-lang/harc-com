#!/usr/bin/env python3
"""Adversarial schema, parsing and source-binding tests; no external tools.

Every negative test names the reason it expects, so a checker that rejects for
the wrong reason (or stops rejecting) fails here. Thresholds are tested at the
boundary: the fixture passes at each threshold and fails one step below it.
"""
import copy
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import verification_coverage as v

OWNER_FILES = sorted({f for owners in v.CONTRACT_OWNERS.values() for f in owners} | v.FORMAL_INPUTS | {'Cargo.toml'})
KILL_LOG = 'running 1 test\ntest mutant_probe_{} ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored;\n'


class Fixture(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.tmp = Path(self.temp.name).resolve()
        self.root = self.tmp / 'repo'
        (self.root / 'verification').mkdir(parents=True)
        (self.root / 'verification/contracts.json').write_text((v.ROOT / 'verification/contracts.json').read_text())
        for name in OWNER_FILES:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('fixture: ' + name)
        self.registry = v.load_registry(self.root)
        self.now = int(time.time()) - 1000

    def bound(self, kind, n=1):
        return dict(schema_version=v.SCHEMA, kind=kind, run_id=kind + '-' + str(n), completed_at=self.now + n,
                    inputs=v.source_inputs(self.root), production_digest=v.production_digest(self.root), complete=True)

    def compiler(self):
        binary = self.tmp / 'harc'
        binary.write_text('synthetic compiler')
        build_log = self.tmp / 'build.log'
        build_log.write_text('synthetic build log')
        return dict(path=str(binary), sha256=v.digest(binary), version='harc 0.3.0 (git 000000000000)',
                    build_command=list(v.COMPILER_BUILD), build_log=str(build_log), build_log_sha256=v.digest(build_log))

    def fuzz(self, n=1, cases=10000):
        start = n * 100000
        return dict(self.bound('fuzz', n), previous_run_id=None, previous_sha256=None,
                    seeds=[3 * n, 3 * n + 1, 3 * n + 2], distinct_cases=cases,
                    case_ids=[hashlib.sha256(str(i).encode()).hexdigest() for i in range(start, start + cases)],
                    failures=[], failure_signatures=[], compiler=self.compiler())

    def hunt(self, n=1):
        return dict(self.bound('hunt', n), previous_run_id=None, previous_sha256=None,
                    scopes=[dict(id=str(i), area=area, investigator='agent' + str(i), complete=True,
                                 summary='completed scoped review ' + str(n)) for i, area in enumerate(sorted(v.AREAS))],
                    candidates=[], review=dict(accepted=True, reviewer='independent-reviewer',
                                               attestation='reviewed ledger and scopes of hunt ' + str(n)))

    def mutant(self, i, area, status='killed'):
        log = self.tmp / 'mutants' / (str(i) + '.log')
        log.parent.mkdir(exist_ok=True)
        log.write_text(KILL_LOG.format(i) if status == 'killed' else 'mutant {}\ntest result: ok. 3 passed; 0 failed; 0 ignored;\n'.format(i))
        return dict(id='m' + str(i), area=area, status=status,
                    evidence=dict(command=['cargo', 'test'], log=str(log), log_sha256=v.digest(log), reviewer='verifier'))

    def mutation(self, killed=27, survived=3, areas=None):
        """At threshold: 30 mutants, 90% killed, all ten areas."""
        areas = sorted(v.AREAS) if areas is None else areas
        statuses = ['killed'] * killed + ['survived'] * survived
        return dict(self.bound('broad_mutation'),
                    mutants=[self.mutant(i, areas[i % len(areas)], s) for i, s in enumerate(statuses)])

    @staticmethod
    def chain(records):
        for previous, record in zip(records, records[1:]):
            record.update(previous_run_id=previous['run_id'], previous_sha256=v.record_digest(previous))
        return records

    def formal(self, out):
        """Formal report in the exact shape proofs/arithmetic/run.py writes."""
        formal_dir = out / 'formal'
        formal_dir.mkdir(exist_ok=True)
        report = dict(scope='fixture', inputs={k: s for k, s in v.source_inputs(self.root).items() if k in v.FORMAL_INPUTS},
                      tools={}, results=[])
        for name in self.registry['formal']['cbmc']:
            unit = formal_dir / (name + '.cpp')
            (formal_dir / (name + '.log')).write_text(json.dumps([
                dict(program='CBMC'), dict(cProverStatus='success'),
                dict(result=[dict(property='main.assertion.1', status='SUCCESS', sourceLocation=dict(file=str(unit)))])]))
            report['results'].append(dict(name=name, status='passed', seconds=0.1,
                                          command=['cbmc', str(unit), '--cpp11', '--json-ui']))
        (formal_dir / 'rust.log').write_text('Complete - 12 successfully verified harnesses, 0 failures, 12 total\n')
        command = ['kani', 'kani.rs', '--target-dir', str(formal_dir / 'kani-build')]
        for name in self.registry['formal']['kani']: command += ['--harness', name]
        report['results'].append(dict(name='rust', status='passed', seconds=1.0, command=command))
        (formal_dir / 'report.json').write_text(json.dumps(report))

    def checks(self):
        out = self.tmp / 'logs'
        out.mkdir(exist_ok=True)
        self.formal(out)
        entries = []
        for check in self.registry['checks']:
            log = 'test result: ok. 1 passed; 0 failed; 0 ignored;\n'
            if check['parser'] == 'unittest': log = 'Ran 10 tests in 0.01s\n\nOK\n'
            if check['parser'] == 'formal': log = 'formal invocation complete\n'
            path = out / (check['id'] + '.log')
            path.write_text(log)
            entries.append(dict(id=check['id'], status='PASS', exit_code=0, log=str(path),
                                log_sha256=v.digest(path), command=[p.replace('{out}', str(out)) for p in check['command']]))
            if check['parser'] == 'formal': entries[-1]['artifacts'] = v.formal_artifacts(out)
        return dict(self.bound('checks'), checks=entries)

    def evidence(self):
        return self.chain([self.fuzz(i) for i in range(1, 4)]) + self.chain([self.hunt(i) for i in range(1, 4)]) + [self.mutation()]

    def write_ledger(self, kind, records, aborted_after=None):
        """Ledger as verification_fuzz.py writes it: a start line, then a finish line, per campaign."""
        ledger = self.tmp / 'ledger' / (kind + '.jsonl')
        ledger.parent.mkdir(exist_ok=True)
        lines = []
        for record in records:
            lines.append(dict(run_id=record['run_id'], sha256=None, report='r.json'))
            lines.append(dict(run_id=record['run_id'], sha256=v.record_digest(record), report='r.json'))
            if aborted_after == record['run_id']:
                lines.append(dict(run_id='aborted', sha256=None, report='r.json'))
        ledger.write_text(''.join(json.dumps(line) + '\n' for line in lines))
        return ledger

    def report(self, checks=None, evidence=None, ledgers=None):
        evidence = self.evidence() if evidence is None else evidence
        if ledgers is None:
            ledgers = {kind: self.write_ledger(kind, [r for r in evidence if isinstance(r, dict) and r.get('kind') == kind])
                       for kind in ('fuzz', 'hunt')}
        return v.evaluate(self.root, self.checks() if checks is None else checks, evidence, ledgers)

    def assertReady(self, checks=None, evidence=None):
        result = self.report(checks, evidence)
        self.assertTrue(result['READY'], result['reasons'])
        return result

    def assertNotReady(self, reason, checks=None, evidence=None, ledgers=None):
        result = self.report(checks, evidence, ledgers)
        self.assertFalse(result['READY'])
        self.assertTrue(any(reason in r for r in result['reasons']), (reason, result['reasons']))

    def assertRejects(self, validator, record, reason):
        with self.assertRaisesRegex(ValueError, re.escape(reason)):
            validator(record, self.root)


class GateTests(Fixture):
    def test_complete_synthetic_evidence_is_ready(self):
        result = self.assertReady()
        self.assertEqual(result['contracts'], dict(passing=sorted(v.CONTRACT_CHECKS), registered=10))
        self.assertEqual([c['id'] for c in result['formal_contracts']], ['arithmetic-kernels'])
        self.assertIn('registered contracts only', result['scope'])
        self.assertEqual(result['missing_families'], v.MISSING_FAMILIES)
        self.assertEqual(sorted(result['ledgers']), ['fuzz', 'hunt'])
        fuzz = result['ledgers']['fuzz']
        self.assertEqual((fuzz['sha256'], fuzz['lines']), (v.digest(fuzz['path']), 6))

    def test_missing_evidence_is_not_ready(self):
        result = v.evaluate(self.root, {}, [], {})
        self.assertFalse(result['READY'])
        self.assertEqual(result['contracts']['passing'], [])
        for kind in ('fuzz', 'hunt', 'broad_mutation'):
            self.assertIn(kind + ': missing campaigns', result['reasons'])

    def test_unknown_evidence_kind(self):
        self.assertNotReady('unknown evidence kind', evidence=self.evidence() + [dict(kind='other')])

    def test_failed_check_only_uncovers_its_contracts(self):
        checks = self.checks()
        entry = next(c for c in checks['checks'] if c['id'] == 'native_ral_assignment_e2e')
        entry['status'] = 'FAIL'
        result = self.report(checks=checks)
        self.assertFalse(result['READY'])
        self.assertIn('required check native_ral_assignment_e2e: failed', result['reasons'])
        self.assertEqual(result['contracts']['passing'], sorted(set(v.CONTRACT_CHECKS) - {'ral-assignment'}))


class BindingTests(Fixture):
    def test_stale_source_rejected(self):
        record = self.fuzz()
        (self.root / 'src/numeric_semantics.rs').write_text('changed')
        self.assertRejects(v.validate_fuzz, record, 'stale or incomplete source inputs')

    def test_added_source_rejected(self):
        record = self.fuzz()
        (self.root / 'src/new.rs').write_text('new')
        self.assertRejects(v.validate_fuzz, record, 'stale or incomplete source inputs')

    def test_production_digest_checked(self):
        record = self.fuzz(); record['production_digest'] = '0' * 64
        self.assertRejects(v.validate_fuzz, record, 'stale production digest')

    def test_manifest_is_bound(self):
        record = self.fuzz()
        with (self.root / 'verification/contracts.json').open('a') as stream: stream.write(' ')
        self.assertRejects(v.validate_fuzz, record, 'stale or incomplete source inputs')

    def test_root_build_version_is_bound(self):
        (self.root / 'build_version.rs').write_text('v1')
        self.assertIn('build_version.rs', v.source_inputs(self.root))
        record = self.fuzz()
        (self.root / 'build_version.rs').write_text('v2')
        self.assertRejects(v.validate_fuzz, record, 'stale or incomplete source inputs')

    def test_symlinked_source_directory_rejected(self):
        external = self.tmp / 'external'; external.mkdir(); (external / 'lib.rs').write_text('v1')
        os.symlink(external, self.root / 'src/linked')
        with self.assertRaisesRegex(ValueError, 'symlink in source inputs: src/linked'):
            v.source_inputs(self.root)

    def test_symlinked_root_file_rejected(self):
        (self.tmp / 'real.rs').write_text('fn main() {}')
        os.symlink(self.tmp / 'real.rs', self.root / 'build.rs')
        with self.assertRaisesRegex(ValueError, 'symlink in source inputs: build.rs'):
            v.source_inputs(self.root)

    def test_incomplete_record(self):
        record = self.fuzz(); record['complete'] = False
        self.assertRejects(v.validate_fuzz, record, 'incomplete evidence')

    def test_bool_and_float_types_rejected(self):
        record = self.fuzz(); record['schema_version'] = 2.0
        self.assertRejects(v.validate_fuzz, record, 'unknown evidence schema')
        record = self.fuzz(); record['completed_at'] = True
        self.assertRejects(v.validate_fuzz, record, 'missing completion timestamp')

    def test_old_schema_rejected(self):
        record = self.fuzz(); record['schema_version'] = 1
        self.assertRejects(v.validate_fuzz, record, 'unknown evidence schema')

    def test_future_timestamp_rejected(self):
        record = self.fuzz(); record['completed_at'] = int(time.time()) + 10 * v.CLOCK_SKEW
        self.assertRejects(v.validate_fuzz, record, 'completion timestamp in the future')

    def test_missing_run_id(self):
        record = self.fuzz(); record['run_id'] = ' '
        self.assertRejects(v.validate_fuzz, record, 'missing run_id')

    def test_duplicate_json_keys_rejected(self):
        path = self.tmp / 'dup.json'
        path.write_text('{"complete": false, "complete": true}')
        with self.assertRaisesRegex(ValueError, 'duplicate JSON key'): v.load_json(path)

    def test_mutation_during_run(self):
        def mutate(*args):
            (self.root / 'src/numeric_semantics.rs').write_text('changed')
            return 0, 'test result: ok. 1 passed; 0 failed; 0 ignored;', None
        with patch.object(v, 'run_process', side_effect=mutate), patch.object(v.shutil, 'which', return_value='/bin/tool'), patch('builtins.print'):
            report = v.run_checks(self.root, self.tmp / 'run')
        self.assertFalse(report['complete'])
        self.assertEqual(report['checks'][0]['status'], 'FAIL')
        self.assertEqual(report['checks'][0]['reason'], 'source changed during check')

    def test_missing_tool_fails_check(self):
        def ok(*args):
            return 0, 'test result: ok. 1 passed; 0 failed; 0 ignored;', None
        with patch.object(v, 'run_process', side_effect=ok), patch.object(v.shutil, 'which', return_value=None), patch('builtins.print'):
            report = v.run_checks(self.root, self.tmp / 'run')
        native = next(c for c in report['checks'] if c['id'] == 'check_cli')
        self.assertEqual((native['status'], native['reason']), ('FAIL', 'unavailable tool'))

    def test_output_inside_any_bound_input_rejected(self):
        for name in v.BOUND_DIRS + ('Cargo.lock',):
            with self.assertRaisesRegex(ValueError, 'output overlaps source inputs'):
                v.run_checks(self.root, self.root / name / 'out')
        with self.assertRaisesRegex(ValueError, 'output overlaps source inputs'):
            v.run_checks(self.root, self.root)

    def test_timeout_kills_process_group(self):
        code, log, error = v.run_process([sys.executable, '-c', 'import time; time.sleep(10)'], self.root, .05, {})
        self.assertEqual(error, 'timeout'); self.assertNotEqual(code, 0)


class RegistryTests(Fixture):
    def edit(self, change):
        path = self.root / 'verification/contracts.json'
        record = json.loads(path.read_text()); change(record)
        path.write_text(json.dumps(record))

    def assertRegistryRejects(self, change, reason):
        self.edit(change)
        with self.assertRaisesRegex(ValueError, re.escape(reason)): v.load_registry(self.root)

    def test_edited_check_command(self):
        self.assertRegistryRejects(lambda r: r['checks'][0].update(command=['echo', 'PASS']), 'unrecognized check command')

    def test_removed_check(self):
        self.assertRegistryRejects(lambda r: r['checks'].pop(), 'changed required checks')

    def test_dropped_tool_requirement(self):
        self.assertRegistryRejects(lambda r: [c.pop('requires', None) for c in r['checks']], 'changed check tool requirements')

    def test_removed_contract_check(self):
        self.assertRegistryRejects(lambda r: r['contracts'][0].update(checks=['rust-lib']), 'changed contract check ownership')

    def test_changed_owner(self):
        self.assertRegistryRejects(lambda r: r['contracts'][2].update(owners=['src/codegen/native/constraint_solver_renderer.rs']),
                                   'changed contract source owners')

    def test_owner_must_exist(self):
        (self.root / 'src/ir/lower/regblock.rs').unlink()
        with self.assertRaisesRegex(ValueError, 'contract owner is not a bound source file'): v.load_registry(self.root)

    def test_changed_gate_threshold(self):
        self.assertRegistryRejects(lambda r: r['stop_gate'].update(minimum_mutation_areas=1), 'changed stop-gate thresholds')

    def test_emptied_missing_families(self):
        self.assertRegistryRejects(lambda r: r.update(missing_families=[]), 'changed missing families requires checker review')

    def test_removed_missing_families_key(self):
        self.assertRegistryRejects(lambda r: r.pop('missing_families'), 'changed missing families requires checker review')

    def test_formal_inventory_drift(self):
        self.assertRegistryRejects(lambda r: r['formal']['cbmc'].pop(), 'formal inventory drift')

    def test_changed_formal_classification(self):
        self.assertRegistryRejects(lambda r: r['contracts'][0].update(formal=True), 'changed formal classification')

    def test_runner_count_drift(self):
        self.assertRegistryRejects(lambda r: next(c for c in r['checks'] if c['id'] == 'proof-runner').update(expected_tests=9),
                                   'runner test count drift')

    def test_contract_inventory_change(self):
        self.assertRegistryRejects(lambda r: r['contracts'].pop(), 'changed contract inventory requires checker review')

    def test_unrequired_contract(self):
        self.assertRegistryRejects(lambda r: r['contracts'][0].update(required=False), 'incomplete contract')


class FuzzTests(Fixture):
    def test_seed_threshold(self):
        record = self.fuzz(); record['seeds'] = record['seeds'][:2]
        self.assertRejects(v.validate_fuzz, record, 'invalid fuzz seeds')

    def test_duplicate_seeds(self):
        record = self.fuzz(); record['seeds'] = [1, 1, 2]
        self.assertRejects(v.validate_fuzz, record, 'invalid fuzz seeds')

    def test_duplicate_cases(self):
        record = self.fuzz(); record['case_ids'][-1] = record['case_ids'][0]
        self.assertRejects(v.validate_fuzz, record, 'duplicate fuzz case IDs')

    def test_case_ids_must_be_hashes(self):
        record = self.fuzz(); record['case_ids'][0] = 'case-0'
        self.assertRejects(v.validate_fuzz, record, 'case IDs must hash canonical tree and environment')

    def test_case_count_mismatch(self):
        record = self.fuzz(); record['distinct_cases'] += 1
        self.assertRejects(v.validate_fuzz, record, 'distinct case count mismatch')

    def test_case_threshold(self):
        v.validate_fuzz(self.fuzz(cases=10000), self.root)
        self.assertRejects(v.validate_fuzz, self.fuzz(cases=9999), 'fuzz campaign below distinct case threshold')

    def test_failure(self):
        record = self.fuzz(); record['failures'] = [dict(signature='x')]
        self.assertRejects(v.validate_fuzz, record, 'fuzz failures or missing failures')

    def test_known_failure_signatures_are_not_flat(self):
        evidence = self.evidence()
        for record in evidence[:3]: record['failure_signatures'] = ['known-miscompile']
        self.assertNotReady('fuzz: fuzz failure signatures or missing signatures', evidence=evidence)

    def test_wrong_build_command(self):
        record = self.fuzz(); record['compiler']['build_command'] = ['make']
        self.assertRejects(v.validate_fuzz, record, 'unknown compiler build command')

    def test_changed_compiler(self):
        record = self.fuzz(); Path(record['compiler']['path']).write_text('changed binary')
        self.assertRejects(v.validate_fuzz, record, 'missing or changed compiler artifact')

    def test_compiler_must_be_harc(self):
        record = self.fuzz(); shell = self.tmp / 'sh'; shell.write_text('shell')
        record['compiler'].update(path=str(shell), sha256=v.digest(shell))
        self.assertRejects(v.validate_fuzz, record, 'compiler artifact is not a harc binary')
        record = self.fuzz(); record['compiler']['version'] = 'GNU bash, version 5.2'
        self.assertRejects(v.validate_fuzz, record, 'compiler version is not a harc version')

    def test_wrong_kind(self):
        record = self.fuzz(); record['kind'] = 'hunt'
        self.assertRejects(v.validate_fuzz, record, 'wrong fuzz kind')

    def test_missing_link(self):
        record = self.fuzz(); record.pop('previous_run_id')
        self.assertRejects(v.validate_fuzz, record, 'missing campaign ledger link')
        record = self.fuzz(); record['previous_run_id'] = 'x'
        self.assertRejects(v.validate_fuzz, record, 'malformed campaign ledger link')

    def test_campaign_threshold(self):
        evidence = self.evidence(); del evidence[2]
        self.assertNotReady('fuzz: missing campaigns', evidence=evidence)

    def test_identical_campaigns(self):
        evidence = self.evidence()
        evidence[1] = dict(copy.deepcopy(evidence[0]), run_id='fuzz-copy', completed_at=evidence[1]['completed_at'])
        self.chain(evidence[:3])
        self.assertNotReady('fuzz: fuzz campaigns share seeds', evidence=evidence)

    def test_shared_seed(self):
        evidence = self.evidence()
        evidence[2]['seeds'][0] = evidence[0]['seeds'][1]
        self.chain(evidence[:3])
        self.assertNotReady('fuzz: fuzz campaigns share seeds', evidence=evidence)

    def test_identical_case_sets(self):
        evidence = self.evidence()
        evidence[2]['case_ids'] = list(evidence[0]['case_ids'])
        self.chain(evidence[:3])
        self.assertNotReady('fuzz: fuzz campaigns have identical case sets', evidence=evidence)

    def test_overlapping_case_sets_allowed(self):
        evidence = self.evidence()
        evidence[2]['case_ids'] = evidence[0]['case_ids'][:5000] + evidence[2]['case_ids'][:5000]
        self.chain(evidence[:3])
        self.assertReady(evidence=evidence)

    def test_omitted_campaign_breaks_chain(self):
        records = self.chain([self.fuzz(i) for i in range(1, 5)])
        evidence = [records[0], records[1], records[3]] + self.evidence()[3:]
        self.assertNotReady('fuzz: campaigns not consecutive in the ledger', evidence=evidence)

    def ledgers(self, fuzz, evidence, **options):
        return dict(fuzz=self.write_ledger('fuzz', fuzz, **options),
                    hunt=self.write_ledger('hunt', [r for r in evidence if r['kind'] == 'hunt']))

    def test_relinked_records_cannot_splice_out_a_failed_campaign(self):
        # Ledger order A, F (failed), B, C; the supplied records are relinked A -> B -> C.
        a, failed, b, c = self.chain([self.fuzz(i) for i in range(1, 5)])
        failed['failures'] = [dict(signature='x')]
        self.chain([a, failed, b, c])
        ledger_records = [a, failed, b, c]
        evidence = [copy.deepcopy(r) for r in (a, b, c)]
        self.chain(evidence)
        evidence += self.evidence()[3:]
        self.assertNotReady('fuzz: campaign not finished in the ledger as supplied', evidence=evidence,
                            ledgers=self.ledgers(ledger_records, evidence))

    def test_supplied_campaigns_must_be_adjacent_in_the_ledger(self):
        a, failed, b, c = (self.fuzz(i) for i in range(1, 5))
        evidence = self.chain([a, b, c]) + self.evidence()[3:]
        self.assertNotReady('fuzz: campaigns not consecutive in the ledger', evidence=evidence,
                            ledgers=self.ledgers([a, failed, b, c], evidence))

    def test_aborted_campaign_in_between_breaks_the_streak(self):
        evidence = self.evidence()
        self.assertNotReady('fuzz: campaigns not consecutive in the ledger', evidence=evidence,
                            ledgers=self.ledgers(evidence[:3], evidence, aborted_after=evidence[0]['run_id']))

    def test_later_campaign_must_be_supplied(self):
        evidence = self.evidence()
        later = self.fuzz(9)
        self.assertNotReady('fuzz: a later campaign in the ledger was not supplied', evidence=evidence,
                            ledgers=self.ledgers(evidence[:3] + [later], evidence))

    def test_missing_ledger(self):
        evidence = self.evidence()
        result = v.evaluate(self.root, self.checks(), evidence, dict(hunt=self.write_ledger('hunt', evidence[3:6])))
        self.assertIn('fuzz: missing campaign ledger', result['reasons'])

    def test_malformed_ledger(self):
        ledger = self.tmp / 'bad.jsonl'; ledger.write_text('{"run_id": "a", "sha256": "nothex"}\n')
        with self.assertRaisesRegex(ValueError, 'malformed campaign ledger'): v.read_ledger(ledger)

    def test_edited_predecessor_breaks_chain(self):
        evidence = self.evidence()
        evidence[0]['failure_signatures'] = []
        evidence[0]['oracle_observations'] = 1
        self.assertNotReady('fuzz: campaigns not consecutive in the ledger', evidence=evidence)

    def test_reverse_order(self):
        evidence = self.evidence(); evidence[:3] = reversed(evidence[:3])
        self.assertNotReady('fuzz: campaigns not strictly chronological', evidence=evidence)

    def test_reused_run_id(self):
        evidence = self.evidence(); evidence[1]['run_id'] = evidence[0]['run_id']
        self.assertNotReady('fuzz: duplicate fuzz run IDs', evidence=evidence)


class HuntTests(Fixture):
    def candidate(self, **fields):
        return dict(dict(id='x', scope_id='0', verifier='verifier', classification='invalid', evidence='repro'), **fields)

    def test_scope_threshold(self):
        record = self.hunt(); record['scopes'].pop()
        self.assertRejects(v.validate_hunt, record, 'hunt needs ten scopes')

    def test_missing_area(self):
        record = self.hunt(); record['scopes'][1]['area'] = record['scopes'][0]['area']
        self.assertRejects(v.validate_hunt, record, 'hunt must cover all ten registered areas')

    def test_same_investigator(self):
        record = self.hunt(); record['scopes'][1]['investigator'] = 'agent0'
        self.assertRejects(v.validate_hunt, record, 'duplicate investigator IDs')

    def test_incomplete_scope(self):
        record = self.hunt(); record['scopes'][3]['complete'] = False
        self.assertRejects(v.validate_hunt, record, 'incomplete hunt scopes')

    def test_missing_summary(self):
        record = self.hunt(); record['scopes'][3]['summary'] = ''
        self.assertRejects(v.validate_hunt, record, 'missing scope evidence')

    def test_candidate_without_independent_verifier(self):
        record = self.hunt(); record['candidates'] = [self.candidate(verifier='agent0')]
        self.assertRejects(v.validate_hunt, record, 'candidate needs independent verifier')

    def test_candidate_in_unknown_scope(self):
        record = self.hunt(); record['candidates'] = [self.candidate(scope_id='99')]
        self.assertRejects(v.validate_hunt, record, 'unknown candidate scope')

    def test_resolved_candidate_is_clean(self):
        record = self.hunt(); record['candidates'] = [self.candidate(classification='rejected')]
        v.validate_hunt(record, self.root)

    def test_fixed_bug_is_not_clean(self):
        record = self.hunt(); record['candidates'] = [self.candidate(classification='proven_fixed')]
        self.assertRejects(v.validate_hunt, record, 'hunt found a proven bug')

    def test_unresolved_candidate(self):
        record = self.hunt(); record['candidates'] = [self.candidate(classification='unresolved')]
        self.assertRejects(v.validate_hunt, record, 'unresolved candidate')

    def test_candidate_without_evidence(self):
        record = self.hunt(); record['candidates'] = [self.candidate(evidence=' ')]
        self.assertRejects(v.validate_hunt, record, 'missing candidate evidence')

    def test_review_required(self):
        for change in (dict(accepted=False), dict(reviewer='agent3'), dict(attestation='')):
            record = self.hunt(); record['review'].update(change)
            self.assertRejects(v.validate_hunt, record, 'missing independent hunt review')

    def test_campaign_threshold(self):
        evidence = self.evidence(); del evidence[5]
        self.assertNotReady('hunt: missing campaigns', evidence=evidence)

    def test_copied_hunt_is_not_another_hunt(self):
        evidence = self.evidence()
        for i in (4, 5):
            evidence[i] = dict(copy.deepcopy(evidence[3]), run_id='hunt-copy-' + str(i), completed_at=evidence[i]['completed_at'])
        self.chain(evidence[3:6])
        self.assertNotReady('hunt: hunt campaigns have identical scopes, candidates and review', evidence=evidence)

    def test_hunts_differing_only_in_review_are_distinct(self):
        evidence = self.evidence()
        for i in (4, 5):
            evidence[i].update(scopes=copy.deepcopy(evidence[3]['scopes']), candidates=[])
        self.chain(evidence[3:6])
        self.assertReady(evidence=evidence)

    def test_omitted_hunt_breaks_chain(self):
        hunts = self.chain([self.hunt(i) for i in range(1, 6)])
        evidence = self.evidence()[:3] + [hunts[0], hunts[2], hunts[4]] + self.evidence()[6:]
        self.assertNotReady('hunt: campaigns not consecutive in the ledger', evidence=evidence)

    def link_cli(self, ledger, record_path):
        return subprocess.run([sys.executable, str(v.ROOT / 'tools/verification_coverage.py'), 'link',
                               '--hunt-ledger', str(ledger), '--record', str(record_path)],
                              capture_output=True, text=True, timeout=60)

    def test_link_cli_chains_hunts_from_the_ledger(self):
        ledger = self.tmp / 'hunt.jsonl'
        paths = []
        for i in range(1, 4):
            path = self.tmp / ('hunt-' + str(i) + '.json'); path.write_text(json.dumps(self.hunt(i)))
            result = self.link_cli(ledger, path)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            paths.append(path)
        hunts = [v.load_json(p) for p in paths]
        self.assertEqual(hunts[0]['previous_run_id'], None)
        self.assertEqual((hunts[2]['previous_run_id'], hunts[2]['previous_sha256']), (hunts[1]['run_id'], v.record_digest(hunts[1])))
        evidence = self.evidence()[:3] + hunts + self.evidence()[6:]
        ledgers = dict(fuzz=self.write_ledger('fuzz', evidence[:3]), hunt=ledger)
        self.assertTrue(self.report(evidence=evidence, ledgers=ledgers)['READY'])
        again = self.link_cli(ledger, paths[1])
        self.assertNotEqual(again.returncode, 0)
        self.assertIn('campaign already in the ledger', again.stdout)

    def test_link_refuses_fuzz_records(self):
        path = self.tmp / 'fuzz.json'; path.write_text(json.dumps(self.fuzz()))
        result = self.link_cli(self.tmp / 'fuzz.jsonl', path)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('link only records hunt campaigns', result.stdout)
        self.assertFalse((self.tmp / 'fuzz.jsonl').exists())


class MutationTests(Fixture):
    def test_at_thresholds_passes(self):
        v.validate_mutation(self.mutation(killed=27, survived=3), self.root)
        v.validate_mutation(self.mutation(killed=30, survived=0, areas=sorted(v.AREAS)[:8]), self.root)
        v.validate_mutation(self.mutation(killed=90, survived=10), self.root)

    def test_count_threshold(self):
        self.assertRejects(v.validate_mutation, self.mutation(killed=29, survived=0), 'need at least 30 mutant records')

    def test_area_threshold(self):
        self.assertRejects(v.validate_mutation, self.mutation(killed=30, survived=0, areas=sorted(v.AREAS)[:7]),
                           'mutation campaign needs at least eight areas')

    def test_percent_threshold(self):
        self.assertRejects(v.validate_mutation, self.mutation(killed=89, survived=11), 'mutation detection below 90 percent')

    def test_invalid_mutants_not_kills(self):
        record = self.mutation()
        record['mutants'][0]['status'] = 'invalid'
        self.assertRejects(v.validate_mutation, record, 'mutation detection below 90 percent')

    def test_blocked_mutant(self):
        record = self.mutation(killed=30, survived=0); record['mutants'][0]['status'] = 'blocked'
        self.assertRejects(v.validate_mutation, record, 'blocked mutation evidence')

    def test_unknown_status(self):
        record = self.mutation(); record['mutants'][0]['status'] = 'probably-killed'
        self.assertRejects(v.validate_mutation, record, 'unknown mutant status')

    def test_unknown_area(self):
        record = self.mutation(); record['mutants'][0]['area'] = 'misc'
        self.assertRejects(v.validate_mutation, record, 'unknown mutant area')

    def test_duplicate_ids(self):
        record = self.mutation(); record['mutants'][1]['id'] = record['mutants'][0]['id']
        self.assertRejects(v.validate_mutation, record, 'duplicate mutant IDs')

    def test_unreviewed_mutant(self):
        record = self.mutation(); record['mutants'][0]['evidence'].pop('reviewer')
        self.assertRejects(v.validate_mutation, record, 'unattested mutant')

    def test_missing_command(self):
        record = self.mutation(); record['mutants'][0]['evidence']['command'] = []
        self.assertRejects(v.validate_mutation, record, 'missing mutant command')

    def test_changed_log(self):
        record = self.mutation(); Path(record['mutants'][0]['evidence']['log']).write_text('changed log')
        self.assertRejects(v.validate_mutation, record, 'missing or changed mutant log')

    def test_kill_must_show_in_log(self):
        record = self.mutation(); log = Path(record['mutants'][0]['evidence']['log'])
        log.write_text('MUTANT SURVIVED: no test failed\n')
        record['mutants'][0]['evidence']['log_sha256'] = v.digest(log)
        self.assertRejects(v.validate_mutation, record, 'killed mutant log shows no failing test')

    def test_unittest_failure_counts_as_kill(self):
        self.assertTrue(v.killed_log('Ran 4 tests in 0.1s\n\nFAILED (failures=1)\n'))
        self.assertFalse(v.killed_log('test result: FAILED. 3 passed; 0 failed; 0 ignored;'))

    def test_copied_log_contents(self):
        record = self.mutation(); first = record['mutants'][0]['evidence']
        for mutant in record['mutants']:
            Path(mutant['evidence']['log']).write_text(KILL_LOG.format(0))
            mutant['evidence']['log_sha256'] = first['log_sha256'] if mutant is record['mutants'][0] else v.digest(mutant['evidence']['log'])
        self.assertRejects(v.validate_mutation, record, 'duplicate mutant log contents')

    def test_shared_log(self):
        record = self.mutation(); first = record['mutants'][0]['evidence']
        for mutant in record['mutants']: mutant['evidence'].update(log=first['log'], log_sha256=first['log_sha256'])
        self.assertRejects(v.validate_mutation, record, 'duplicate mutant logs')

    def test_wrong_kind(self):
        record = self.mutation(); record['kind'] = 'mutation'
        self.assertRejects(v.validate_mutation, record, 'wrong mutation kind')

    def test_missing_campaign(self):
        self.assertNotReady('broad_mutation: missing campaigns', evidence=self.evidence()[:6])


class CheckTests(Fixture):
    def test_vacuous_rust_summary(self):
        with self.assertRaisesRegex(ValueError, 'vacuous'): v.parse_rust('test result: ok. 0 passed; 0 failed; 0 ignored;')

    def test_ignored_rust_summary(self):
        with self.assertRaisesRegex(ValueError, 'ignored'): v.parse_rust('test result: ok. 10 passed; 0 failed; 1 ignored;')

    def test_skipped_rust_test(self):
        for line in ('skipping native test', 'SKIP trace_merge_e2e: verilator not on PATH', 'tool unavailable'):
            with self.assertRaisesRegex(ValueError, 'skipped or unavailable'):
                v.parse_rust(line + '\ntest result: ok. 10 passed; 0 failed; 0 ignored;')

    def test_skip_in_test_name_is_not_a_skip(self):
        v.parse_rust('test parser::skip_ws_ok ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;')

    def test_unknown_rust_summary(self):
        with self.assertRaises(ValueError): v.parse_rust('all tests succeeded')

    def test_unittest_exact_count(self):
        with self.assertRaisesRegex(ValueError, 'unexpected unittest count'): v.parse_unittest('Ran 9 tests in 0.1s\nOK\n', 10)
        with self.assertRaisesRegex(ValueError, 'did not pass cleanly'): v.parse_unittest('Ran 10 tests in 0.1s\nOK (skipped=1)\n', 10)
        v.parse_unittest('Ran 10 tests in 0.1s\n\nOK\n', 10)

    def test_check_kind(self):
        checks = self.checks(); checks['kind'] = 'fuzz'
        self.assertNotReady('checks: wrong checks evidence kind', checks=checks)

    def test_missing_required_check(self):
        checks = self.checks(); checks['checks'].pop()
        self.assertNotReady('checks: missing required checks', checks=checks)

    def test_nonzero_exit(self):
        checks = self.checks(); checks['checks'][0]['exit_code'] = False
        self.assertNotReady('required check rust-lib: failed', checks=checks)

    def test_edited_result_command(self):
        checks = self.checks(); checks['checks'][0]['command'] = ['echo', 'PASS']
        self.assertNotReady('required check rust-lib: unrecognized result command', checks=checks)

    def test_log_tampering(self):
        checks = self.checks(); Path(checks['checks'][0]['log']).write_text('tampered')
        self.assertNotReady('required check rust-lib: missing or changed check log', checks=checks)

    def test_stale_checks(self):
        checks = self.checks(); (self.root / 'src/parser.rs').write_text('changed')
        result = v.evaluate(self.root, checks, [], {})
        self.assertIn('checks: stale or incomplete source inputs', result['reasons'])


class FormalTests(Fixture):
    def formal_dir(self, checks):
        return Path(checks['checks'][0]['log']).parent / 'formal'

    def edit_report(self, checks, change):
        path = self.formal_dir(checks) / 'report.json'
        report = json.loads(path.read_text()); change(report)
        path.write_text(json.dumps(report))

    def write_cbmc(self, checks, name, messages):
        (self.formal_dir(checks) / (name + '.log')).write_text(json.dumps(messages))

    def assertFormalRejects(self, checks, reason):
        # Re-pin the artifacts, as if the run itself had produced this output.
        entry = next(c for c in checks['checks'] if c['id'] == 'arithmetic-formal')
        entry['artifacts'] = v.formal_artifacts(self.formal_dir(checks).parent)
        self.assertNotReady('required check arithmetic-formal: ' + reason, checks=checks)

    def test_artifact_changed_after_run(self):
        checks = self.checks(); (self.formal_dir(checks) / 'u128-mask.log').write_text('[]')
        self.assertNotReady('required check arithmetic-formal: missing or changed formal artifacts', checks=checks)

    def test_missing_formal(self):
        checks = self.checks(); (self.formal_dir(checks) / 'report.json').unlink()
        self.assertFormalRejects(checks, 'missing or changed formal artifacts')

    def test_duplicate_obligations(self):
        checks = self.checks()
        self.edit_report(checks, lambda r: r['results'].__setitem__(-2, r['results'][0]))
        self.assertFormalRejects(checks, 'missing formal obligations')

    def test_failed_and_timeout_status(self):
        for status in ('failed', 'timeout', 'PASS'):
            checks = self.checks()
            self.edit_report(checks, lambda r: r['results'][0].update(status=status))
            self.assertFormalRejects(checks, 'formal failure')

    def test_failed_raw_log(self):
        checks = self.checks(); unit = str(self.formal_dir(checks) / 'u128-mask.cpp')
        self.write_cbmc(checks, 'u128-mask', [dict(cProverStatus='success'),
                        dict(result=[dict(property='main.assertion.1', status='FAILURE', sourceLocation=dict(file=unit))])])
        self.assertFormalRejects(checks, 'incomplete CBMC result')

    def test_vacuous_raw_log(self):
        checks = self.checks(); unit = str(self.formal_dir(checks) / 'u128-mask.cpp')
        self.write_cbmc(checks, 'u128-mask', [dict(cProverStatus='success'),
                        dict(result=[dict(property='main.undefined-shift.1', status='SUCCESS', sourceLocation=dict(file=unit))])])
        self.assertFormalRejects(checks, 'incomplete CBMC result')

    def test_non_string_property(self):
        checks = self.checks()
        self.write_cbmc(checks, 'u128-mask', [dict(cProverStatus='success'), dict(result=[dict(property=5, status='SUCCESS')])])
        self.assertFormalRejects(checks, 'unknown CBMC property format')

    def test_log_copied_from_another_obligation(self):
        checks = self.checks(); formal = self.formal_dir(checks)
        (formal / 'u128-compare.log').write_text((formal / 'u128-mask.log').read_text())
        self.assertFormalRejects(checks, 'CBMC log is not for its obligation')

    def test_command_for_another_obligation(self):
        checks = self.checks(); unit = str(self.formal_dir(checks) / 'u128-mask.cpp')
        self.edit_report(checks, lambda r: r['results'][1].update(command=['cbmc', unit]))
        self.assertFormalRejects(checks, 'CBMC command is not for its obligation')

    def test_kani_count(self):
        checks = self.checks()
        (self.formal_dir(checks) / 'rust.log').write_text('Complete - 11 successfully verified harnesses, 0 failures, 11 total\n')
        self.assertFormalRejects(checks, 'incomplete Kani result')

    def test_kani_harness_list(self):
        checks = self.checks()
        self.edit_report(checks, lambda r: r['results'][-1]['command'].__setitem__(-1, 'const_mul'))
        self.assertFormalRejects(checks, 'wrong Kani harnesses')

    def test_kani_from_another_run(self):
        checks = self.checks()
        self.edit_report(checks, lambda r: r['results'][-1]['command'].__setitem__(3, '/elsewhere/kani-build'))
        self.assertFormalRejects(checks, 'Kani result not from this run')

    def test_stale_formal_input(self):
        checks = self.checks()
        self.edit_report(checks, lambda r: r['inputs'].update({'runtime/harc_thread_rt.h': '0' * 64}))
        self.assertFormalRejects(checks, 'stale formal input')

    def test_incomplete_formal_binding(self):
        checks = self.checks()
        self.edit_report(checks, lambda r: r['inputs'].pop('proofs/arithmetic/kani.rs'))
        self.assertFormalRejects(checks, 'incomplete formal source binding')


class FuzzLedgerTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        self.tmp = Path(temp.name)
        self.ledger = self.tmp / 'fuzz.jsonl'

    def test_first_campaign_has_no_predecessor(self):
        self.assertEqual(v.append_ledger(self.ledger, dict(run_id='a'), 'r.json', finished=False), (None, None))

    def test_finished_campaign_links_by_record_digest(self):
        record = dict(run_id='a', complete=True)
        v.append_ledger(self.ledger, dict(record, complete=False), 'r.json', finished=False)
        v.append_ledger(self.ledger, record, 'r.json')
        self.assertEqual(v.ledger_link(v.read_ledger(self.ledger)), ('a', v.record_digest(record)))

    def test_aborted_campaign_cannot_be_linked_across(self):
        record = dict(run_id='a', complete=False)
        v.append_ledger(self.ledger, record, 'r.json', finished=False)
        run_id, sha = v.ledger_link(v.read_ledger(self.ledger))
        self.assertEqual(run_id, 'a')
        self.assertTrue(v.SHA.fullmatch(sha))
        self.assertNotEqual(sha, v.record_digest(record))
        self.assertNotEqual(sha, v.record_digest(dict(record, complete=True)))

    def test_fuzz_script_requires_seeds(self):
        result = subprocess.run([sys.executable, str(v.ROOT / 'tools/verification_fuzz.py'), '--out', str(self.tmp / 'out')],
                                capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 2)
        self.assertIn('--seeds', result.stderr)
        self.assertFalse((self.tmp / 'out').exists())


STUB_CBMC = '''#!/usr/bin/env python3
import json, sys
if '--version' in sys.argv: print('6.11.0 (stub)'); sys.exit(0)
unit = sys.argv[1]
print(json.dumps([{"program": "CBMC stub"}, {"cProverStatus": "success"},
    {"result": [{"property": "main.assertion.1", "status": "SUCCESS", "sourceLocation": {"file": unit}}]}]))
'''
STUB_KANI = '''#!/usr/bin/env python3
import sys
if '--version' in sys.argv: print('kani 0.0.0 (stub)'); sys.exit(0)
n = sys.argv.count('--harness')
print(f'Complete - {n} successfully verified harnesses, 0 failures, {n} total')
'''
STUB_Z3 = '''#!/usr/bin/env python3
print('Z3 version 0.0.0 (stub)')
'''


class RunnerShapeTests(unittest.TestCase):
    """The checker must accept what proofs/arithmetic/run.py really writes."""

    def test_checker_accepts_real_runner_output(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp).resolve()
            stubs = temp / 'bin'; stubs.mkdir()
            for name, body in (('cbmc', STUB_CBMC), ('kani', STUB_KANI), ('z3', STUB_Z3)):
                (stubs / name).write_text(body); (stubs / name).chmod(0o755)
            env = dict(os.environ, PATH=str(stubs) + os.pathsep + os.environ.get('PATH', ''))
            out = temp / 'out'
            result = subprocess.run([sys.executable, str(v.ROOT / 'proofs/arithmetic/run.py'), '--out', str(out / 'formal')],
                                    env=env, capture_output=True, text=True, timeout=300)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            report = v.parse_formal(out, v.load_registry(v.ROOT), v.ROOT)
            self.assertEqual({r['status'] for r in report['results']}, {'passed'})


if __name__ == '__main__':
    unittest.main()
