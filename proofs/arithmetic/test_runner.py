#!/usr/bin/env python3
"""Exercise failure classification and prove that injected defects are caught."""
import json
from pathlib import Path
import shutil
import tempfile
import unittest

import run as proof


class RunnerTests(unittest.TestCase):
    def test_success_requires_real_assertions_and_complete_results(self):
        result = {"property": "main.assertion.1", "status": "SUCCESS"}
        success = [{"result": [result]}, {"cProverStatus": "success"}]
        self.assertTrue(proof.cbmc_passed(0, json.dumps(success)))
        self.assertFalse(proof.cbmc_passed(10, json.dumps(success)))
        self.assertFalse(proof.cbmc_passed(0, "VERIFICATION SUCCESSFUL"))
        self.assertFalse(proof.cbmc_passed(0, json.dumps(success[1:])))
        result["status"] = "FAILURE"
        self.assertFalse(proof.cbmc_passed(0, json.dumps(success)))
        result["status"] = "UNKNOWN"
        self.assertFalse(proof.cbmc_passed(0, json.dumps(success)))

    def test_missing_duplicate_or_reversed_source_markers_fail(self):
        for source in ("", "// END FORMAL WORD KERNELS\n// BEGIN FORMAL WORD KERNELS",
                       "// BEGIN FORMAL WORD KERNELS" * 2 + "// END FORMAL WORD KERNELS"):
            with self.assertRaises(ValueError):
                proof.slice_source(source, "WORD")

    def test_cpp_mutations_are_counterexamples(self):
        cbmc = shutil.which("cbmc")
        self.assertIsNotNone(cbmc, "CBMC required: mutation checks must execute")
        runtime = (proof.ROOT / "runtime/harc_thread_rt.h").read_text()
        cases = [
            ("compare", "U128", "u128", "PROOF_COMPARE", [], 1,
             "return lhs_negative;", "return rhs_negative;"),
            ("carry", "WORD", "wide", "PROOF_WIDE_ARITH", ["-DWORDS=2", "-DPACK_BITS=64"], 3,
             "carry = sum >> 32;", "carry = 0;"),
            ("sign", "WORD", "wide", "PROOF_WIDE_EXTEND", ["-DWORDS=2", "-DPACK_BITS=64"], 3,
             "if (neg) {", "if (false) {"),
        ]
        out = proof.ROOT / "target/formal/mutations"
        out.mkdir(parents=True, exist_ok=True)
        for name, section, harness, macro, defines, unwind, old, new in cases:
            with self.subTest(name=name):
                body = proof.slice_source(runtime, section)
                self.assertEqual(body.count(old), 1)
                unit = out / f"{name}.cpp"
                unit.write_text(proof.PREFIX + body.replace(old, new)
                                + (proof.ROOT / f"proofs/arithmetic/{harness}.cpp").read_text())
                code, log = proof.run([cbmc, str(unit), "--cpp11", f"-D{macro}", *defines,
                                       "--unwind", str(unwind), "--unwinding-assertions", "--json-ui"], 60)
                (out / f"{name}.log").write_text(log)
                self.assertEqual(code, 10, log)
                results = [r for m in json.loads(log) for r in m.get("result", [])]
                self.assertTrue(any(r.get("property", "").startswith("main.assertion.")
                                    and r.get("status") == "FAILURE" for r in results), log)

    def test_rust_shift_mutation_is_detected(self):
        kani = shutil.which("kani")
        self.assertIsNotNone(kani, "Kani required: mutation checks must execute")
        source = (proof.ROOT / "src/numeric_semantics.rs").read_text()
        self.assertEqual(source.count("else if a.signed {"), 1)
        out = proof.ROOT / "target/formal/mutations"
        out.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="harc-formal-mutation-") as directory:
            temp = Path(directory)
            module = temp / "numeric_semantics.rs"
            module.write_text(source.replace("else if a.signed {", "else if false {"))
            harness = (proof.ROOT / "proofs/arithmetic/kani.rs").read_text()
            harness = harness.replace('../../src/numeric_semantics.rs', 'numeric_semantics.rs')
            entry = temp / "harness.rs"
            entry.write_text(harness)
            code, log = proof.run([kani, str(entry), "--harness", "const_shift", "--exact",
                                   "--default-unwind", "5", "--output-format", "terse",
                                   "--target-dir", str(temp / "build")], 90)
            (out / "rust-shift.log").write_text(log)
            self.assertIsNotNone(code, log)
            self.assertNotEqual(code, 0, log)
            self.assertIn("VERIFICATION:- FAILED", log)
            self.assertRegex(log, r"Failed Checks: assertion failed: a\.bits\s*==\s*b\.bits")

    def test_rust_bitwise_recanonicalization_mutation_is_detected(self):
        kani = shutil.which("kani")
        self.assertIsNotNone(kani, "Kani required: mutation checks must execute")
        source = (proof.ROOT / "src/numeric_semantics.rs").read_text()
        original = """            Scalar {
                bits,
                // Preserve the existing integral-promotion signedness for
                // Boolean-only operations, even when typed Boolean carriers
                // enter the kernel with `signed: false`.
                signed: if boolean { true } else { signed },
                boolean,
            }
"""
        mutated = """            Scalar {
                bits: if a.boolean && b.boolean { (bits != 0) as u64 } else { bits },
                // Preserve the existing integral-promotion signedness for
                // Boolean-only operations, even when typed Boolean carriers
                // enter the kernel with `signed: false`.
                signed: if boolean { true } else { signed },
                boolean,
            }
"""
        self.assertEqual(source.count(original), 1)
        out = proof.ROOT / "target/formal/mutations"
        out.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="harc-formal-mutation-") as directory:
            temp = Path(directory)
            module = temp / "numeric_semantics.rs"
            module.write_text(source.replace(original, mutated))
            harness = (proof.ROOT / "proofs/arithmetic/kani.rs").read_text()
            harness = harness.replace('../../src/numeric_semantics.rs', 'numeric_semantics.rs')
            entry = temp / "harness.rs"
            entry.write_text(harness)
            code, log = proof.run([kani, str(entry), "--harness", "const_bit_logic", "--exact",
                                   "--default-unwind", "5", "--output-format", "terse",
                                   "--target-dir", str(temp / "build")], 90)
            (out / "rust-bitwise-recanonicalization.log").write_text(log)
            self.assertIsNotNone(code, log)
            self.assertNotEqual(code, 0, log)
            self.assertIn("VERIFICATION:- FAILED", log)
            self.assertRegex(log, r"Failed Checks: assertion failed: a\.bits\s*==\s*b\.bits")


    def test_rng_extraction_requires_the_unique_production_signature(self):
        source = (proof.ROOT / "runtime/harc_random_rt.h").read_text()
        body = proof.rng_source(source)
        self.assertIn("span == 0 ? next()", body)
        for broken in ("", body + body, body.rsplit("\n}", 1)[0]):
            with self.assertRaises(ValueError):
                proof.rng_source(broken)

    def test_rng_full_domain_draw_mutation_is_detected(self):
        cbmc = shutil.which("cbmc")
        self.assertIsNotNone(cbmc, "CBMC required")
        body = proof.rng_source((proof.ROOT / "runtime/harc_random_rt.h").read_text())
        original = "span == 0 ? next() : next() % span"
        self.assertEqual(body.count(original), 1)
        out = proof.ROOT / "target/formal/mutations"
        out.mkdir(parents=True, exist_ok=True)
        unit = out / "rng-skipped-draw.cpp"
        unit.write_text(proof.RNG_PREFIX + body.replace(original, "span == 0 ? 0 : next() % span")
                        + (proof.ROOT / "proofs/arithmetic/rng.cpp").read_text())
        code, log = proof.run([cbmc, str(unit), "--cpp11", "-DPROOF_RNG_FULL",
                               "--unwind", "1", "--unwinding-assertions", "--json-ui"], 60)
        (out / "rng-skipped-draw.log").write_text(log)
        self.assertEqual(code, 10, log)
        results = [r for m in json.loads(log) for r in m.get("result", [])]
        self.assertTrue(any(r.get("property", "").startswith("main.assertion.")
                            and r.get("status") == "FAILURE" for r in results), log)

    def test_bitwise_identity_rejects_a_canonicalizing_implementation(self):
        kani = shutil.which("kani")
        self.assertIsNotNone(kani, "Kani required")
        source = (proof.ROOT / "src/numeric_semantics.rs").read_text()
        original = "BitAnd => a.bits & b.bits,"
        self.assertEqual(source.count(original), 1)
        mutant = "BitAnd => if a.boolean && b.boolean { ((a.bits & b.bits) != 0) as u64 } else { a.bits & b.bits },"
        out = proof.ROOT / "target/formal/mutations"
        out.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="harc-law-mutation-") as directory:
            temp = Path(directory)
            (temp / "numeric_semantics.rs").write_text(source.replace(original, mutant))
            harness = (proof.ROOT / "proofs/arithmetic/kani.rs").read_text()
            entry = temp / "harness.rs"
            entry.write_text(harness.replace('../../src/numeric_semantics.rs', 'numeric_semantics.rs'))
            code, log = proof.run([kani, str(entry), "--harness", "const_bitwise_laws", "--exact",
                                   "--default-unwind", "5", "--output-format", "terse",
                                   "--target-dir", str(temp / "build")], 90)
            (out / "rust-bitwise-identity.log").write_text(log)
            self.assertIsNotNone(code, log)
            self.assertNotEqual(code, 0, log)
            self.assertIn("VERIFICATION:- FAILED", log)
            self.assertRegex(log, r"Failed Checks: assertion failed: aa\.bits\s*==\s*a\.bits")


if __name__ == "__main__":
    unittest.main()
