#!/usr/bin/env python3
"""Self-test for check_mvp_gates.py (#66): the fixture gate maps every registered test to a
cargo target and reads libtest output correctly, and the binary gate fails a binary that is
not gated (a fake `scull` that "succeeds" at everything). POSIX only for the fake binary."""
import importlib.util
import os
import re
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
spec = importlib.util.spec_from_file_location("gate", HERE / "check_mvp_gates.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class TestTargets(unittest.TestCase):
    def test_every_registered_test_file_maps_to_a_cargo_target(self):
        src = (ROOT / "crates/scull-fixtures/src/registry.rs").read_text()
        files = set(re.findall(r'"(crates/[^"]+\.rs)"', src))
        self.assertGreater(len(files), 30)
        for f in files:
            package, target = gate.test_target(f)
            self.assertTrue((ROOT / "crates" / package / "Cargo.toml").exists(), f)
            self.assertIn(target[0], ("--lib", "--test"), f)
        self.assertEqual(gate.test_target("crates/scull-cleaner/tests/quickclean/main.rs"),
                         ("scull-cleaner", ["--test", "quickclean"]))
        self.assertEqual(gate.test_target("crates/scull-safety/src/rubric.rs"), ("scull-safety", ["--lib"]))
        self.assertEqual(gate.test_target("crates/scull-fixtures/tests/fx_cls.rs"),
                         ("scull-fixtures", ["--test", "fx_cls"]))
        with self.assertRaises(ValueError):
            gate.test_target("apps/desktop/x.rs")


class ParseResults(unittest.TestCase):
    OUT = """running 5 tests
test fx_a ... ok
test tests::fx_b ... ok
test fx_c ... FAILED
test fx_d ... ignored
SKIPPED fx_e: no privileges (set SCULL_FIXTURES_ALLOW_SUDO=1)
test fx_e ... ok
test result: FAILED. 3 passed; 1 failed; 1 ignored
"""

    def test_outcomes(self):
        r = gate.parse_results(self.OUT, ["fx_a", "fx_b", "fx_c", "fx_d", "fx_e", "fx_f"], 101)
        self.assertEqual(r, {"fx_a": "ok", "fx_b": "ok", "fx_c": "FAILED", "fx_d": "ignored",
                             "fx_e": "skipped", "fx_f": "n/a"})

    def test_a_failing_binary_with_no_test_lines_fails_everything(self):
        r = gate.parse_results("error: could not compile\n", ["fx_a"], 101)
        self.assertEqual(r, {"fx_a": "FAILED"}, "a target that fails to build fails every test")
        r = gate.parse_results("test fx_a ... ok\n", ["fx_a"], 101)
        self.assertEqual(r, {"fx_a": "FAILED"}, "non-zero exit with every test ok is still a failure")


@unittest.skipIf(os.name == "nt", "the fake binary is a shebang script")
class BinaryGate(unittest.TestCase):
    def test_an_ungated_binary_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / "fake-scull"
            # Exits 0 for everything and prints nothing: no refusal is ever enforced.
            fake.write_text(f"#!{sys.executable}\nimport sys\nsys.exit(0)\n")
            fake.chmod(0o755)
            gate.FAILURES.clear()
            gate.check_clean_gating(str(fake))
            self.assertTrue(gate.FAILURES, "an ungated binary must fail the gate")
            gate.FAILURES.clear()
            gate.check_other_commands(str(fake))
            self.assertTrue(any("data purge" in f for f in gate.FAILURES), gate.FAILURES)
            gate.FAILURES.clear()


if __name__ == "__main__":
    unittest.main(verbosity=2)
