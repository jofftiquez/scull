#!/usr/bin/env python3
"""Self-test for the release packaging scripts (P2-REL-01): the CycloneDX SBOM generator
(packaging/sbom.py) and the Scoop manifest writer (packaging/scoop.py). Runs `cargo metadata`
(no build, no network beyond what Cargo needs to read the locked dependencies)."""

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load(name: str):
    spec = importlib.util.spec_from_file_location(name, HERE / f"{name}.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


sbom = load("sbom")
scoop = load("scoop")

LINUX = "x86_64-unknown-linux-gnu"
WINDOWS = "x86_64-pc-windows-msvc"
# Crates only the tests use: never in the shipped binary's SBOM.
DEV_ONLY = {"proptest", "jsonschema", "scull-fixtures", "scull-desktop-bridge"}


class Sbom(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        os.environ["SOURCE_DATE_EPOCH"] = "1793232000"
        cls.linux = sbom.sbom(LINUX)
        cls.windows = sbom.sbom(WINDOWS)

    def test_is_a_cyclonedx_1_5_document_for_the_scull_binary(self):
        d = self.linux
        self.assertEqual(d["bomFormat"], "CycloneDX")
        self.assertEqual(d["specVersion"], "1.5")
        self.assertRegex(d["serialNumber"], r"^urn:uuid:[0-9a-f-]{36}$")
        self.assertEqual(d["version"], 1)
        self.assertEqual(d["metadata"]["timestamp"], "2026-10-29T00:00:00Z")
        root = d["metadata"]["component"]
        self.assertEqual(root["name"], "scull")
        self.assertEqual(root["type"], "application")
        self.assertEqual(root["licenses"], [{"expression": "MIT OR Apache-2.0"}])
        props = {p["name"]: p["value"] for p in d["metadata"]["properties"]}
        self.assertEqual(props["cdx:rust:target"], LINUX)

    def test_lists_shipped_crates_with_hashes_and_licenses_and_no_dev_dependencies(self):
        comps = {c["name"]: c for c in self.linux["components"]}
        for name in ("clap", "rusqlite", "serde_json", "scull-core", "scull-engine", "scull-mcp"):
            self.assertIn(name, comps)
        self.assertFalse(DEV_ONLY & set(comps), DEV_ONLY & set(comps))
        self.assertNotIn("tauri", comps)
        for c in self.linux["components"]:
            self.assertTrue(c.get("licenses"), f"{c['name']} has no license")
            if c["purl"].startswith("pkg:cargo/"):
                self.assertRegex(c["hashes"][0]["content"], r"^[0-9a-f]{64}$", c["name"])
                self.assertEqual(c["hashes"][0]["alg"], "SHA-256")

    def test_build_time_crates_are_marked_excluded(self):
        comps = {c["name"]: c for c in self.linux["components"]}
        self.assertEqual(comps["serde_derive"].get("scope"), "excluded")  # proc macro
        self.assertEqual(comps["cc"].get("scope"), "excluded")  # build dependency
        self.assertNotIn("scope", comps["serde"])

    def test_dependency_graph_is_closed(self):
        refs = {c["bom-ref"] for c in self.linux["components"]}
        refs.add(self.linux["metadata"]["component"]["bom-ref"])
        self.assertEqual(len(refs), len(self.linux["components"]) + 1, "bom-refs are unique")
        for dep in self.linux["dependencies"]:
            self.assertIn(dep["ref"], refs)
            for d in dep["dependsOn"]:
                self.assertIn(d, refs)

    def test_per_target_dependencies(self):
        linux = {c["name"] for c in self.linux["components"]}
        windows = {c["name"] for c in self.windows["components"]}
        self.assertIn("windows-sys", windows)
        self.assertNotIn("windows-sys", linux)
        self.assertNotEqual(self.linux["serialNumber"], self.windows["serialNumber"])

    def test_deterministic(self):
        again = sbom.sbom(LINUX)
        self.assertEqual(json.dumps(again, sort_keys=True), json.dumps(self.linux, sort_keys=True))

    def test_cli_writes_one_file_per_target(self):
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(
                [sys.executable, str(HERE / "sbom.py"), "--target", LINUX, "--out-dir", tmp],
                check=True, capture_output=True,
            )
            out = Path(tmp) / f"scull-cli-{LINUX}.cdx.json"
            self.assertEqual(json.loads(out.read_text())["bomFormat"], "CycloneDX")


class Scoop(unittest.TestCase):
    HASH = "ab" * 32

    def test_manifest_points_at_the_release_archive(self):
        m = scoop.manifest("0.2.0", self.HASH.upper())
        url = "https://github.com/jofftiquez/scull/releases/download/v0.2.0/scull-cli-x86_64-pc-windows-msvc.zip"
        self.assertEqual(m["architecture"]["64bit"], {"url": url, "hash": self.HASH})
        self.assertEqual(m["bin"], "scull.exe")
        self.assertEqual(m["version"], "0.2.0")
        self.assertIn("$version", m["autoupdate"]["architecture"]["64bit"]["url"])
        self.assertEqual(m["autoupdate"]["hash"], {"url": "$url.sha256"})

    def test_rejects_bad_versions_and_hashes(self):
        for v, h in (("v1", self.HASH), ("1.2", self.HASH), ("1.2.3", "abc"), ("1.2.3", "z" * 64)):
            with self.assertRaises(ValueError):
                scoop.manifest(v, h)
        scoop.manifest("0.1.0-alpha.1", self.HASH)

    def test_reads_dists_sha256_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, scoop.ARCHIVE + ".sha256").write_text(f"{self.HASH}  {scoop.ARCHIVE}\n")
            out = Path(tmp) / "scull.json"
            r = subprocess.run(
                [sys.executable, str(HERE / "scoop.py"), "--version", "v0.3.0", "--distrib", tmp,
                 "--out", str(out)],
                capture_output=True, text=True,
            )
            self.assertEqual(r.returncode, 0, r.stderr)
            m = json.loads(out.read_text())
            self.assertEqual((m["version"], m["architecture"]["64bit"]["hash"]), ("0.3.0", self.HASH))
            r = subprocess.run(
                [sys.executable, str(HERE / "scoop.py"), "--version", "0.3.0", "--distrib",
                 str(Path(tmp, "missing")), "--out", str(out)],
                capture_output=True, text=True,
            )
            self.assertEqual(r.returncode, 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
