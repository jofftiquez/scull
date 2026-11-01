#!/usr/bin/env python3
"""Self-test for check_inspect_only.py: each consent and metadata-only rule must fail on a
deliberate violation and pass on correct behavior. Command checks use tiny fake `scull`
executables in temp dirs (POSIX only: the fakes are shebang scripts)."""
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
            "nothing_to_do = args[1] in ('uninstall', 'disable', 'status')\n"
            "preview = '--dry-run' in args or '--show-request' in args\n"
            "sys.exit(0 if (preview or nothing_to_do) else 2)\n"
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
            "sys.exit(0 if args[1] in ('uninstall', 'disable', 'status') else 2)\n"
        )
        self.assertEqual(self.run_gate(body), 1)

    def test_sending_a_question_without_consent_fails(self):
        # `scull ask` succeeding (as if it had sent the question) while it is off.
        body = (
            "if args[0] == 'ask':\n"
            "    sys.exit(0)\n"
            "nothing_to_do = args[1] in ('uninstall', 'disable', 'status')\n"
            "sys.exit(0 if ('--dry-run' in args or nothing_to_do) else 2)\n"
        )
        self.assertEqual(self.run_gate(body), 1)

    def test_applying_without_consent_fails(self):
        self.assertEqual(self.run_gate("sys.exit(0)\n"), 1)

    def test_registering_a_schedule_without_consent_fails(self):
        # `scull schedule enable` writing a unit file under HOME without --yes.
        body = (
            "if args[:2] == ['schedule', 'enable'] and '--dry-run' not in args:\n"
            "    d = os.path.join(home, '.config', 'systemd', 'user')\n"
            "    os.makedirs(d, exist_ok=True)\n"
            "    open(os.path.join(d, 'scull-check.timer'), 'w').write('[Timer]')\n"
            "    sys.exit(2)\n"
            "nothing_to_do = args[1] in ('uninstall', 'disable', 'status')\n"
            "sys.exit(0 if ('--dry-run' in args or nothing_to_do) else 2)\n"
        )
        self.assertEqual(self.run_gate(body), 1)


def exit_code(fn) -> int:
    try:
        fn()
    except SystemExit as e:
        return int(e.code or 0)
    return 0


def tool(name: str, read_only, destructive) -> dict:
    ann = {}
    if read_only is not None:
        ann["readOnlyHint"] = read_only
    if destructive is not None:
        ann["destructiveHint"] = destructive
    return {"name": name, "annotations": ann}


class ToolAnnotations(unittest.TestCase):
    def check(self, tools) -> int:
        return exit_code(lambda: gate.check_tool_annotations(tools))

    def test_read_only_tools_and_metadata_only_register_pass(self):
        self.assertEqual(self.check([
            tool("get_status", True, False),
            tool("register_worktree", False, False),
        ]), 0)

    def test_destructive_metadata_tool_fails(self):
        self.assertEqual(self.check([tool("register_worktree", False, True)]), 1)

    def test_metadata_tool_without_destructive_hint_fails(self):
        self.assertEqual(self.check([tool("register_worktree", False, None)]), 1)

    def test_other_writing_tool_fails(self):
        self.assertEqual(self.check([tool("prepare_cleanup", False, False)]), 1)

    def test_read_only_tool_missing_hint_fails(self):
        self.assertEqual(self.check([tool("get_status", None, False)]), 1)


@unittest.skipIf(os.name == "nt", "fake executables are shebang scripts")
class ScullHomeOnly(unittest.TestCase):
    def run_gate(self, body: str) -> int:
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            home, canary = base / "home", base / "canary"
            home.mkdir()
            canary.mkdir()
            scull = fake_scull(base, body)
            return exit_code(
                lambda: gate.check_scull_home_only(scull, canary, home, gate.env_for(home))
            )

    def test_writing_under_scull_home_passes(self):
        body = (
            "d = os.path.join(home, 'scull-home', 'data')\n"
            "os.makedirs(d, exist_ok=True)\n"
            "open(os.path.join(d, 'worktree-ttl.jsonl'), 'a').write('{}\\n')\n"
        )
        self.assertEqual(self.run_gate(body), 0)

    def test_writing_elsewhere_in_home_fails(self):
        body = "open(os.path.join(home, '.worktree-ttl'), 'w').write('x')\n"
        self.assertEqual(self.run_gate(body), 1)

    def test_writing_into_the_canary_fails(self):
        body = "open('touched', 'w').write('x')\n"  # cwd is the canary tree
        self.assertEqual(self.run_gate(body), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
