#!/usr/bin/env python3
"""One version for the CLI, MCP and desktop (P4-REL-01).

One tag releases everything, so every place that carries scull's version must agree:

* ``Cargo.toml``: ``[workspace.package] version`` (every root-workspace crate, incl. the CLI);
* ``apps/desktop/src-tauri/Cargo.toml``: ``[package] version`` (the Tauri app, its own workspace);
* ``apps/desktop/src-tauri/tauri.conf.json``: ``version`` (bundle and installer versions);
* ``apps/desktop/package.json``: ``version``.

``--check`` (CI) fails if they differ, and with ``--tag vX.Y.Z`` also if they differ from the
tag. ``bump_version.py X.Y.Z`` rewrites all four in place (only the version text changes) and
prints the follow-up commands (refresh both lock files, add the CHANGELOG section).

Usage::

    python3 scripts/release/bump_version.py --check [--tag v1.0.0]
    python3 scripts/release/bump_version.py 1.0.0 [--root DIR]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")

# (file, how to find the version): "toml:<section>" or "json".
FILES = [
    ("Cargo.toml", "toml:workspace.package"),
    ("apps/desktop/src-tauri/Cargo.toml", "toml:package"),
    ("apps/desktop/src-tauri/tauri.conf.json", "json"),
    ("apps/desktop/package.json", "json"),
]


def _toml_span(text: str, section: str) -> tuple[int, int, str]:
    """(start, end, value) of `version = "..."` directly inside `[section]`."""
    header = re.compile(rf"^\[{re.escape(section)}\]\s*$", re.M)
    m = header.search(text)
    if not m:
        raise ValueError(f"no [{section}] section")
    nxt = re.compile(r"^\[", re.M).search(text, m.end())
    body_end = nxt.start() if nxt else len(text)
    v = re.compile(r'^version\s*=\s*"([^"]*)"', re.M).search(text, m.end(), body_end)
    if not v:
        raise ValueError(f"no version in [{section}]")
    return v.start(1), v.end(1), v.group(1)


def _json_span(text: str) -> tuple[int, int, str]:
    """(start, end, value) of the top-level "version" string, edited in place (formatting kept)."""
    doc = json.loads(text)
    value = doc.get("version")
    if not isinstance(value, str):
        raise ValueError("no top-level \"version\" string")
    # The first "version" key at indentation level one is the top-level one in these files.
    m = re.compile(r'^(?: {2}|\t)"version"\s*:\s*"([^"]*)"', re.M).search(text)
    if not m or m.group(1) != value:
        raise ValueError("could not locate the top-level \"version\" key")
    return m.start(1), m.end(1), value


def read_versions(root: Path) -> dict[str, str]:
    out = {}
    for rel, kind in FILES:
        text = (root / rel).read_text()
        span = _json_span(text) if kind == "json" else _toml_span(text, kind.split(":", 1)[1])
        out[rel] = span[2]
    return out


def set_version(root: Path, version: str) -> list[str]:
    """Rewrite every file to `version`; returns the files that changed."""
    if not SEMVER.match(version):
        raise ValueError(f"not a SemVer version: {version!r}")
    changed = []
    for rel, kind in FILES:
        p = root / rel
        text = p.read_text()
        start, end, old = _json_span(text) if kind == "json" else _toml_span(text, kind.split(":", 1)[1])
        if old != version:
            p.write_text(text[:start] + version + text[end:])
            changed.append(rel)
    return changed


def changelog_has(root: Path, version: str) -> bool:
    p = root / "CHANGELOG.md"
    return p.exists() and re.search(rf"^## {re.escape(version)}\s*$", p.read_text(), re.M) is not None


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("version", nargs="?", help="new version, e.g. 1.0.0 (no leading v)")
    ap.add_argument("--check", action="store_true", help="only check that every version agrees")
    ap.add_argument("--tag", help="with --check: the release tag the versions must match (vX.Y.Z)")
    ap.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    a = ap.parse_args()
    try:
        if a.check or not a.version:
            versions = read_versions(a.root)
            for rel, v in versions.items():
                print(f"{v:<16} {rel}")
            distinct = set(versions.values())
            if len(distinct) != 1:
                print("FAIL: versions differ; run scripts/release/bump_version.py <version>", file=sys.stderr)
                return 1
            (v,) = distinct
            if a.tag and a.tag.removeprefix("v") != v:
                print(f"FAIL: tag {a.tag} does not match version {v}", file=sys.stderr)
                return 1
            if a.tag and not changelog_has(a.root, v):
                print(f"FAIL: CHANGELOG.md has no '## {v}' section", file=sys.stderr)
                return 1
            print(f"ok: every version is {v}")
            return 0
        version = a.version.removeprefix("v")
        changed = set_version(a.root, version)
        for rel in changed:
            print(f"updated {rel}")
        print(f"\nNext:\n  cargo update --workspace          # refresh Cargo.lock\n"
              f"  (cd apps/desktop/src-tauri && cargo update -p scull-desktop)\n"
              f"  python3 scripts/release/changelog.py --version {version} --since <previous tag>\n"
              f"  python3 scripts/release/bump_version.py --check --tag v{version}")
        if not changelog_has(a.root, version):
            print(f"\nCHANGELOG.md has no '## {version}' section yet.")
        return 0
    except (OSError, ValueError, json.JSONDecodeError) as e:
        print(f"bump_version.py: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
