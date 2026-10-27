#!/usr/bin/env python3
"""Adversarial schema, parsing and source-binding tests; no external tools."""
import copy
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import verification_coverage as v


class CoverageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'repo'
        self.root.mkdir()
        (self.root / 'verification').mkdir()
        (self.root / 'verification/contracts.json').write_text((v.ROOT / 'verification/contracts.json').read_text())
        for name in ('src/numeric_semantics.rs', 'runtime/harc_thread_rt.h', 'runtime/harc_random_rt.h',
                     'proofs/arithmetic/run.py', 'proofs/arithmetic/u128.cpp', 'proofs/arithmetic/wide.cpp',
                     'proofs/arithmetic/rng.cpp', 'proofs/arithmetic/kani.rs', 'Cargo.toml'):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('fixture: ' + name)
        self.registry = v.load_registry(self.root)

    def bound(self, kind, n=1):
        return dict(schema_version=1, kind=kind, run_id=str(n), completed_at=n,
                    inputs=v.source_inputs(self.root), production_digest=v.production_digest(self.root), complete=True)

    def fuzz(self, n=1):
        binary = (Path(self.temp.name) / 'harc').resolve()
        binary.write_text('synthetic compiler')
        build_log = (Path(self.temp.name) / 'build.log').resolve()
        build_log.write_text('synthetic build log')
        return dict(self.bound('fuzz', n), seeds=[1, 2, 3], distinct_cases=10000,
                    case_ids=[hashlib.sha256(str(i).encode()).hexdigest() for i in range(10000)],
                    failures=[], failure_signatures=[], compiler=dict(path=str(binary), sha256=v.digest(binary),
                    version='test-version', build_command=['cargo', 'build', '--offline', '--bin', 'harc'],
                    build_log=str(build_log), build_log_sha256=v.digest(build_log)))

    def hunt(self, n=1):
        return dict(self.bound('hunt', n), scopes=[dict(id=str(i), area=area, investigator='agent'+str(i),
                    complete=True, summary='completed scoped review') for i, area in enumerate(sorted(v.AREAS))], candidates=[],
                    review=dict(accepted=True, reviewer='independent-reviewer', attestation='reviewed ledger and scopes'))

    def mutation(self):
        log = (Path(self.temp.name) / 'mutant.log').resolve()
        log.write_text('independent differential mismatch')
        return dict(self.bound('broad_mutation'), mutants=[dict(id=str(i), area=area,
                    status='killed', evidence=dict(command=['test'], log=str(log), log_sha256=v.digest(log), reviewer='verifier'))
                    for i, area in enumerate(sorted(v.AREAS) * 3)])

    def checks(self):
        out = (Path(self.temp.name) / 'logs').resolve()
        out.mkdir(exist_ok=True)
        (out / 'formal').mkdir(exist_ok=True)
        entries = []
        formal = dict(inputs=v.source_inputs(self.root), results=[])
        for name in self.registry['formal']['cbmc']:
            (out / 'formal' / (name + '.log')).write_text(json.dumps([
                dict(cProverStatus='success'), dict(result=[dict(property='main.assertion.1', status='SUCCESS')])]))
            formal['results'].append(dict(name=name, status='PASS'))
        (out / 'formal/rust.log').write_text('Complete - 12 successfully verified harnesses, 0 failures, 12 total\n')
        cmd = ['kani']
        for name in self.registry['formal']['kani']: cmd.extend(['--harness', name])
        formal['results'].append(dict(name='rust', status='PASS', command=cmd))
        (out / 'formal/report.json').write_text(json.dumps(formal))
        for check in self.registry['checks']:
            log = 'test result: ok. 1 passed; 0 failed; 0 ignored;\n'
            if check['parser'] == 'unittest': log = 'Ran 10 tests in 0.01s\n\nOK\n'
            if check['parser'] == 'formal': log = 'formal invocation complete\n'
            path = out / (check['id'] + '.log')
            path.write_text(log)
            entries.append(dict(id=check['id'], status='PASS', exit_code=0, log=str(path),
                                log_sha256=v.digest(path), command=[p.replace('{out}', str(out)) for p in check['command']]))
        return dict(self.bound('checks'), checks=entries)

    def evidence(self):
        return [self.fuzz(i) for i in range(1, 4)] + [self.hunt(i) for i in range(1, 4)] + [self.mutation()]

    def test_complete_synthetic_evidence_is_ready(self):
        report = v.evaluate(self.root, self.checks(), self.evidence())
        self.assertTrue(report['READY'], report['reasons'])
        self.assertEqual(report['coverage'], dict(covered=10, registered=10))
        self.assertEqual(report['formal_coverage'], dict(covered=1, registered=1))

    def test_missing_evidence_is_not_ready(self):
        self.assertFalse(v.evaluate(self.root, {}, [])['READY'])

    def test_stale_source_rejected(self):
        record = self.fuzz()
        (self.root / 'src/numeric_semantics.rs').write_text('changed')
        with self.assertRaisesRegex(ValueError, 'stale'): v.validate_fuzz(record, self.root)

    def test_added_source_rejected(self):
        record = self.fuzz()
        (self.root / 'src/new.rs').write_text('new')
        with self.assertRaises(ValueError): v.validate_fuzz(record, self.root)

    def test_manifest_is_bound(self):
        record = self.fuzz()
        with (self.root / 'verification/contracts.json').open('a') as stream: stream.write(' ')
        with self.assertRaises(ValueError): v.validate_fuzz(record, self.root)

    def test_incomplete_fuzz(self):
        record = self.fuzz(); record['complete'] = False
        with self.assertRaises(ValueError): v.validate_fuzz(record, self.root)

    def test_duplicate_cases(self):
        record = self.fuzz(); record['case_ids'][-1] = record['case_ids'][0]
        with self.assertRaises(ValueError): v.validate_fuzz(record, self.root)

    def test_duplicate_seeds(self):
        record = self.fuzz(); record['seeds'] = [1, 1, 2]
        with self.assertRaises(ValueError): v.validate_fuzz(record, self.root)

    def test_small_campaign_cannot_union_to_threshold(self):
        record = self.fuzz(); record['case_ids'] = record['case_ids'][:4000]; record['distinct_cases'] = 4000
        with self.assertRaises(ValueError): v.validate_fuzz(record, self.root)

    def test_fuzz_failure(self):
        record = self.fuzz(); record['failures'] = ['failure']
        with self.assertRaises(ValueError): v.validate_fuzz(record, self.root)

    def test_reused_campaign_not_consecutive(self):
        checks, evidence = self.checks(), self.evidence()
        evidence[1] = copy.deepcopy(evidence[0])
        self.assertFalse(v.evaluate(self.root, checks, evidence)['READY'])

    def test_reverse_campaign_order(self):
        checks, evidence = self.checks(), self.evidence()
        evidence[:3] = reversed(evidence[:3])
        self.assertFalse(v.evaluate(self.root, checks, evidence)['READY'])

    def test_nonflat_campaigns(self):
        checks, evidence = self.checks(), self.evidence()
        evidence[2]['failure_signatures'] = ['new']
        self.assertFalse(v.evaluate(self.root, checks, evidence)['READY'])

    def test_incomplete_hunt(self):
        record = self.hunt(); record['scopes'].pop()
        with self.assertRaises(ValueError): v.validate_hunt(record, self.root)

    def test_same_investigator_hunt(self):
        record = self.hunt(); record['scopes'][1]['investigator'] = 'agent0'
        with self.assertRaises(ValueError): v.validate_hunt(record, self.root)

    def test_candidate_without_independent_verifier(self):
        record = self.hunt(); record['candidates'] = [dict(id='x', scope_id='0', verifier='agent0', classification='invalid', evidence='repro')]
        with self.assertRaises(ValueError): v.validate_hunt(record, self.root)

    def test_fixed_bug_resets_clean_hunt(self):
        record = self.hunt(); record['candidates'] = [dict(id='x', scope_id='0', verifier='verifier', classification='proven_fixed', evidence='repro')]
        with self.assertRaises(ValueError): v.validate_hunt(record, self.root)

    def test_unresolved_candidate(self):
        record = self.hunt(); record['candidates'] = [dict(id='x', scope_id='0', verifier='verifier', classification='unresolved', evidence='repro')]
        with self.assertRaises(ValueError): v.validate_hunt(record, self.root)

    def test_unsigned_mutant(self):
        record = self.mutation(); record['mutants'][0]['evidence'].pop('reviewer')
        with self.assertRaises(ValueError): v.validate_mutation(record, self.root)

    def test_blocked_mutant(self):
        record = self.mutation(); record['mutants'][0]['status'] = 'blocked'
        with self.assertRaises(ValueError): v.validate_mutation(record, self.root)

    def test_arithmetic_only_not_broad(self):
        record = self.mutation()
        for mutant in record['mutants']: mutant['area'] = 'arithmetic'
        with self.assertRaises(ValueError): v.validate_mutation(record, self.root)

    def test_invalid_mutants_not_kills(self):
        record = self.mutation()

        for mutant in record['mutants'][:4]: mutant['status'] = 'invalid'
        with self.assertRaises(ValueError): v.validate_mutation(record, self.root)

    def test_vacuous_rust_summary(self):
        with self.assertRaises(ValueError): v.parse_rust('test result: ok. 0 passed; 0 failed; 0 ignored;')

    def test_ignored_rust_summary(self):
        with self.assertRaises(ValueError): v.parse_rust('test result: ok. 10 passed; 0 failed; 1 ignored;')

    def test_skipped_rust_test(self):
        with self.assertRaises(ValueError): v.parse_rust('skipping native test\ntest result: ok. 10 passed; 0 failed; 0 ignored;')

    def test_unknown_rust_summary(self):
        with self.assertRaises(ValueError): v.parse_rust('all tests succeeded')

    def test_unittest_exact_count(self):
        with self.assertRaises(ValueError): v.parse_unittest('Ran 9 tests in 0.1s\nOK\n', 10)

    def test_missing_formal(self):
        checks = self.checks()
        (Path(checks['checks'][0]['log']).parent / 'formal/report.json').unlink()
        self.assertFalse(v.evaluate(self.root, checks, self.evidence())['READY'])

    def test_duplicate_formal_obligations(self):
        checks = self.checks(); path = Path(checks['checks'][0]['log']).parent / 'formal/report.json'
        report = json.loads(path.read_text()); report['results'][-1] = report['results'][0]
        path.write_text(json.dumps(report))
        self.assertFalse(v.evaluate(self.root, checks, self.evidence())['READY'])

    def test_log_tampering(self):
        checks = self.checks(); Path(checks['checks'][0]['log']).write_text('tampered')
        self.assertFalse(v.evaluate(self.root, checks, self.evidence())['READY'])

    def test_edited_registry_command(self):
        path = self.root / 'verification/contracts.json'
        record = json.loads(path.read_text()); record['checks'][0]['command'] = ['echo', 'PASS']
        path.write_text(json.dumps(record))
        with self.assertRaises(ValueError): v.load_registry(self.root)

    def test_mutation_during_run(self):
        def mutate(*args):
            (self.root / 'src/numeric_semantics.rs').write_text('changed')
            return 0, 'test result: ok. 1 passed; 0 failed; 0 ignored;', None
        with patch.object(v, 'run_process', side_effect=mutate), patch.object(v.shutil, 'which', return_value='/bin/tool'), patch('builtins.print'):
            report = v.run_checks(self.root, Path(self.temp.name) / 'run')
        self.assertFalse(report['complete'])
        self.assertEqual(report['checks'][0]['status'], 'FAIL')


    def test_changed_compiler_rejected(self):
        record = self.fuzz()
        Path(record['compiler']['path']).write_text('changed binary')
        with self.assertRaises(ValueError): v.validate_fuzz(record, self.root)

    def test_changed_mutant_log_rejected(self):
        record = self.mutation()
        Path(record['mutants'][0]['evidence']['log']).write_text('changed log')
        with self.assertRaises(ValueError): v.validate_mutation(record, self.root)

    def test_missing_hunt_breadth(self):
        record = self.hunt()
        record['scopes'][1]['area'] = record['scopes'][0]['area']
        with self.assertRaises(ValueError): v.validate_hunt(record, self.root)

    def test_removed_contract_check_rejected(self):
        path = self.root / 'verification/contracts.json'
        record = json.loads(path.read_text())
        record['contracts'][0]['checks'] = ['rust-lib']
        path.write_text(json.dumps(record))
        with self.assertRaises(ValueError): v.load_registry(self.root)

    def test_changed_gate_threshold_rejected(self):
        path = self.root / 'verification/contracts.json'
        record = json.loads(path.read_text())
        record['stop_gate']['minimum_mutation_areas'] = 1
        path.write_text(json.dumps(record))
        with self.assertRaises(ValueError): v.load_registry(self.root)

    def test_source_current_but_failed_formal_raw_log(self):
        checks = self.checks()
        out = Path(checks['checks'][0]['log']).parent
        path = out / 'formal/u128-mask.log'
        path.write_text(json.dumps([dict(cProverStatus='success'),
            dict(result=[dict(property='main.assertion.1', status='FAILURE')])]))
        self.assertFalse(v.evaluate(self.root, checks, self.evidence())['READY'])

    def test_timeout_kills_process_group(self):
        code, log, error = v.run_process(['python3', '-c', 'import time; time.sleep(10)'], self.root, .05, {})
        self.assertEqual(error, 'timeout'); self.assertNotEqual(code, 0)


if __name__ == '__main__':
    unittest.main()
