#!/usr/bin/env python3
"""Scoop manifest for `scull` on Windows (P2-REL-01).

dist has no Scoop installer, so the release writes the manifest itself from the version and
the SHA-256 of the x86_64 Windows archive that dist built (``<archive>.zip.sha256``). The
manifest goes into the GitHub release (``scull.json``) and, when SCOOP_BUCKET_TOKEN is set, into
the bucket repository ``jofftiquez/scoop-scull`` (release-packages.yml). Its ``checkver`` and
``autoupdate`` sections let Scoop's own tooling follow later releases.

dist's Windows zips have no top-level folder: ``scull.exe`` sits at the root.

Usage::

    python3 packaging/scoop.py --version 0.1.0 --distrib target/distrib --out scull.json
    python3 packaging/scoop.py --version 0.1.0 --hash <sha256> --out scull.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPOSITORY = "https://github.com/jofftiquez/scull"
ARCHIVE = "scull-cli-x86_64-pc-windows-msvc.zip"
DESCRIPTION = "A safe developer storage analyzer: finds build outputs, dependency folders, caches and Git worktrees, and explains what is safe to remove."
SHA256 = re.compile(r"^[0-9a-f]{64}$")
# SemVer, including prerelease and build suffixes.
VERSION = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")


def read_hash(distrib: Path) -> str:
    """The hash from dist's ``<archive>.sha256`` (``<hex>  <name>`` or ``<hex> *<name>``)."""
    text = (distrib / f"{ARCHIVE}.sha256").read_text().split()
    if not text:
        raise ValueError(f"{ARCHIVE}.sha256 is empty")
    return text[0].lower()


def manifest(version: str, sha256: str) -> dict:
    if not VERSION.match(version):
        raise ValueError(f"not a SemVer version: {version!r}")
    sha256 = sha256.lower()
    if not SHA256.match(sha256):
        raise ValueError(f"not a SHA-256 hex digest: {sha256!r}")
    url = f"{REPOSITORY}/releases/download/v{version}/{ARCHIVE}"
    template = f"{REPOSITORY}/releases/download/v$version/{ARCHIVE}"
    return {
        "version": version,
        "description": DESCRIPTION,
        "homepage": REPOSITORY,
        "license": "MIT|Apache-2.0",
        "architecture": {"64bit": {"url": url, "hash": sha256}},
        "bin": "scull.exe",
        "checkver": {"github": REPOSITORY},
        "autoupdate": {
            "architecture": {"64bit": {"url": template}},
            "hash": {"url": "$url.sha256"},
        },
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--version", required=True, help="release version without the leading v")
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--distrib", type=Path, help=f"folder holding {ARCHIVE}.sha256")
    src.add_argument("--hash", help="SHA-256 of the Windows archive")
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    try:
        digest = a.hash if a.hash else read_hash(a.distrib)
        doc = manifest(a.version.removeprefix("v"), digest)
    except (OSError, ValueError) as e:
        print(f"scoop.py: {e}", file=sys.stderr)
        return 1
    a.out.parent.mkdir(parents=True, exist_ok=True)
    a.out.write_text(json.dumps(doc, indent=4) + "\n")
    print(f"wrote {a.out} ({doc['version']}, {doc['architecture']['64bit']['hash'][:12]}...)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
