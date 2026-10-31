#!/usr/bin/env python3
"""Self-test for check_inspect_only.py: each consent/metadata rule must fail on a deliberate
violation and pass on correct behavior. Uses tiny fake `scull` executables in temp dirs (POSIX
only: the fakes are shebang scripts)."""
import importlib.util, os, sys, tempfile, unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("gate", HERE / "check_inspect_only.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def fake_scull(dir: Path, body: str) -> str:
    """A fake `scull` running `body` (Python) with `args` = argv[1:] and `home` = $HOME."""
    p = dir / "fake-scull"
    p.write_text(
        f"#!{sys.executable}\nimport os, sys\nargs = sys.argv[1:]\nhome = os.environ['HOME']\n"
        + body
    )
    p.chmod(0o755)
    return str(p)


@unittest.skipIf(os.name == "nt", "fake executables are shebang scripts")
class ConsentGated(unittest.TestCase):
    def run_gate(self, body: str) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            home, canary = base / "home", base / "canary"
            home.mkdir()
            canary.mkdir()
            scull = fake_scull(base, body)
            try:
                gate.check_consent_gated(scull, canary, home, gate.env_for(home))
            except SystemExit as e:
                return int(e.code or 0)
            return 0

    def test_refusing_without_writing_passes(self):
        body = (
            "sys.exit(0 if ('--dry-run' in args or args[1] == 'uninstall') else 2)\n"
        )
        self.assertEqual(self.run_gate(body), 0)

    def test_writing_a_client_config_without_consent_fails(self):
        body = (
            "if args[1] == 'install' and '--dry-run' not in args:\n"
            "    open(os.path.join(home, '.claude.json'), 'w').write('{}')\n"
            "    sys.exit(2)\n"
            "sys.exit(0)\n"
        )
        self.assertEqual(self.run_gate(body), 1)

    def test_writing_anywhere_under_home_fails(self):
        body = (
            "if '--dry-run' in args:\n"
            "    os.makedirs(os.path.join(home, 'scull-home', 'config'), exist_ok=True)\n"
            "    open(os.path.join(home, 'scull-home', 'config', 'config.toml'), 'w').write('x')\n"
            "    sys.exit(0)\n"
            "sys.exit(0 if args[1] == 'uninstall' else 2)\n"
        )
        self.assertEqual(self.run_gate(body), 1)

    def test_applying_without_consent_fails(self):
        self.assertEqual(self.run_gate("sys.exit(0)\n"), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
