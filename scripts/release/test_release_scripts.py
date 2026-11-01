#!/usr/bin/env python3
"""Self-tests for the v1.0 release scripts (P4-REL-01): bump_version.py and changelog.py.
Everything runs in temporary copies or temporary Git repositories."""

import importlib.util
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / f"{name}.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


bump = load("bump_version")
changelog = load("changelog")


class BumpVersion(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        for rel, _ in bump.FILES:
            (self.root / rel).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(ROOT / rel, self.root / rel)
        (self.root / "CHANGELOG.md").write_text("# Changelog\n\n## 1.0.0\n\nfirst\n")

    def tearDown(self):
        self.tmp.cleanup()

    def run_cli(self, *args):
        return subprocess.run(
            [sys.executable, str(HERE / "bump_version.py"), "--root", str(self.root), *args],
            capture_output=True, text=True,
        )

    def test_the_repository_versions_agree(self):
        self.assertEqual(len(set(bump.read_versions(ROOT).values())), 1)

    def test_bump_rewrites_only_the_version_text(self):
        before = {rel: (self.root / rel).read_text() for rel, _ in bump.FILES}
        old = bump.read_versions(self.root)["Cargo.toml"]
        changed = bump.set_version(self.root, "1.0.0")
        self.assertEqual(sorted(changed), sorted(rel for rel, _ in bump.FILES))
        self.assertEqual(set(bump.read_versions(self.root).values()), {"1.0.0"})
        for rel, text in before.items():
            after = (self.root / rel).read_text()
            self.assertEqual(after.replace('"1.0.0"', f'"{old}"', 1), text, rel)
        # Other "version" keys are untouched (dependencies, rust-version, JSON nested keys).
        cargo = (self.root / "Cargo.toml").read_text()
        self.assertIn('rust-version = "1.97"', cargo)
        pkg = json.loads((self.root / "apps/desktop/package.json").read_text())
        self.assertEqual(pkg["version"], "1.0.0")

    def test_check_detects_drift_tag_mismatch_and_missing_changelog(self):
        self.assertEqual(self.run_cli("--check").returncode, 0)
        bump.set_version(self.root, "1.0.0")
        r = self.run_cli("--check", "--tag", "v1.0.0")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.run_cli("--check", "--tag", "v1.0.1").returncode, 1)
        p = self.root / "apps/desktop/package.json"
        p.write_text(p.read_text().replace('"version": "1.0.0"', '"version": "1.0.1"', 1))
        r = self.run_cli("--check")
        self.assertEqual(r.returncode, 1)
        self.assertIn("versions differ", r.stderr)
        bump.set_version(self.root, "2.0.0")
        r = self.run_cli("--check", "--tag", "v2.0.0")
        self.assertEqual(r.returncode, 1)
        self.assertIn("CHANGELOG.md has no '## 2.0.0'", r.stderr)

    def test_rejects_non_semver(self):
        with self.assertRaises(ValueError):
            bump.set_version(self.root, "1.0")
        self.assertEqual(self.run_cli("v1").returncode, 1)


class Changelog(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name)
        git = lambda *a: subprocess.run(["git", *a], cwd=self.repo, check=True, capture_output=True)
        git("init", "-q")
        git("config", "user.email", "t@example.invalid")
        git("config", "user.name", "t")
        self.git = git
        self.commit("chore: initial")
        git("tag", "v0.1.0")
        for s in [
            "[P3-UI-02] Storage Explorer: treemap drill-down (#101)",
            "[P2-SAFE-01] Executor revalidates every item before deleting (#102)",
            "[P2-SAFE-01] evidence [skip ci]",
            "[P4-MCP-02] Approval broker for MCP plans (#12) (#103)",
            "[P1-WIN-CI] Fix Windows path handling (#104)",
            "ci: cache cargo registry (#105)",
            "[P9-NEW] Something new (#106)",
            "[P5-CLI] Flaky thing (#107)",
            'Revert "[P5-CLI] Flaky thing (#107)"',
            "Plain subject without a key",
        ]:
            self.commit(s)

    def tearDown(self):
        self.tmp.cleanup()

    def commit(self, subject):
        self.git("commit", "-q", "--allow-empty", "-m", subject)

    def test_groups_links_and_filters(self):
        subjects = changelog.git_subjects("v0.1.0", "HEAD", self.repo)
        out = changelog.render("1.0.0", changelog.entries(subjects))
        self.assertTrue(out.startswith("## 1.0.0\n"))
        url = changelog.REPO_URL
        self.assertIn(f"### Safety\n\n- Executor revalidates every item before deleting ([#102]({url}/pull/102))", out)
        self.assertIn(f"### Desktop app\n\n- Storage Explorer: treemap drill-down ([#101]({url}/pull/101))", out)
        # Only the trailing (#n) is the PR; earlier ones stay in the text.
        self.assertIn(f"- Approval broker for MCP plans (#12) ([#103]({url}/pull/103))", out)
        self.assertIn("### Scanning and detection\n\n- Fix Windows path handling", out)
        self.assertIn("### Release, packaging and CI\n\n- cache cargo registry", out)
        self.assertIn("- Something new", out.split("### Other")[1])
        self.assertIn("- Plain subject without a key", out)
        self.assertNotIn("skip ci", out)
        self.assertNotIn("Flaky thing", out)  # reverted in the same range
        self.assertNotIn("initial", out)  # before --since
        self.assertIn("### Known limitations", out)

    def test_cli(self):
        r = subprocess.run(
            [sys.executable, str(HERE / "changelog.py"), "--root", str(self.repo), "--version", "v1.0.0",
             "--since", "v0.1.0"],
            capture_output=True, text=True,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(r.stdout.startswith("## 1.0.0"))
        r = subprocess.run(
            [sys.executable, str(HERE / "changelog.py"), "--root", str(self.repo), "--version", "1.0.0",
             "--since", "no-such-tag"],
            capture_output=True, text=True,
        )
        self.assertEqual(r.returncode, 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
