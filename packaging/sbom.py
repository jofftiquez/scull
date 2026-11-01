#!/usr/bin/env python3
"""CycloneDX 1.5 SBOM of the shipped `scull` binary (P2-REL-01), one per target triple.

Stdlib only (Python 3.11+), so it runs unchanged on any release runner with no tool to
download. The source of truth is Cargo: ``cargo metadata --locked --filter-platform <target>``
gives the resolved graph for that target, and ``Cargo.lock`` gives each registry crate's
SHA-256. Only what ends up in the binary is listed: crates reachable from ``scull-cli`` through
normal and build dependencies (build scripts and proc macros run at build time and are kept,
marked ``scope: excluded``); dev-dependencies are not.

The output is deterministic for a given commit and target: the timestamp comes from
``SOURCE_DATE_EPOCH`` (or the commit time), and the serial number is a UUIDv5 of the version,
target and Cargo.lock contents.

Usage::

    python3 packaging/sbom.py --target x86_64-unknown-linux-gnu --out scull.cdx.json
    python3 packaging/sbom.py --all-targets --out-dir target/distrib   # dist-workspace targets
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import subprocess
import sys
import tomllib
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROOT_PACKAGE = "scull-cli"
BINARY = "scull"
REPOSITORY = "https://github.com/jofftiquez/scull"
NAMESPACE = uuid.UUID("5d2c39e4-6a8e-4b8f-9a4c-2f61c0c7e4b1")  # fixed: serial numbers are stable
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"


def dist_targets() -> list[str]:
    """The release targets from dist-workspace.toml."""
    cfg = tomllib.loads((ROOT / "dist-workspace.toml").read_text())
    return list(cfg["dist"]["targets"])


def cargo_metadata(target: str) -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def lock_checksums() -> dict[tuple[str, str], str]:
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    return {
        (p["name"], p["version"]): p["checksum"]
        for p in lock.get("package", [])
        if "checksum" in p
    }


def timestamp() -> str:
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    if not epoch:
        try:
            epoch = subprocess.run(
                ["git", "log", "-1", "--format=%ct"], cwd=ROOT, check=True,
                capture_output=True, text=True,
            ).stdout.strip()
        except (OSError, subprocess.CalledProcessError):
            epoch = "0"
    t = dt.datetime.fromtimestamp(int(epoch or 0), tz=dt.timezone.utc)
    return t.strftime("%Y-%m-%dT%H:%M:%SZ")


def purl(pkg: dict) -> str:
    if pkg.get("source") == CRATES_IO:
        return f"pkg:cargo/{pkg['name']}@{pkg['version']}"
    if pkg.get("source") is None:
        # Workspace crate: part of scull itself, not published separately.
        return f"pkg:github/jofftiquez/scull@{pkg['version']}#crates/{pkg['name']}"
    return f"pkg:cargo/{pkg['name']}@{pkg['version']}?repository_url={pkg['source']}"


def reachable(meta: dict) -> tuple[dict[str, set[str]], set[str], str]:
    """(edges, build-time-only ids, root id): the normal/build graph from scull-cli."""
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    pkgs = {p["id"]: p for p in meta["packages"]}
    root = next(p["id"] for p in meta["packages"] if p["name"] == ROOT_PACKAGE and p["source"] is None)
    edges: dict[str, set[str]] = {}
    runtime: set[str] = set()
    seen: set[tuple[str, bool]] = set()
    todo = [(root, True)]
    while todo:
        pid, at_runtime = todo.pop()
        if (pid, at_runtime) in seen:
            continue
        seen.add((pid, at_runtime))
        if at_runtime:
            runtime.add(pid)
        edges.setdefault(pid, set())
        is_proc_macro = any("proc-macro" in t["kind"] for t in pkgs[pid]["targets"])
        for dep in nodes[pid]["deps"]:
            kinds = {k["kind"] for k in dep["dep_kinds"]}
            if kinds <= {"dev"}:
                continue
            edges[pid].add(dep["pkg"])
            # Normal deps of a runtime crate are in the binary; build deps and anything a proc
            # macro uses only run on the build machine.
            dep_runtime = at_runtime and not is_proc_macro and None in kinds
            todo.append((dep["pkg"], dep_runtime))
    build_only = set(edges) - runtime
    # A proc-macro crate itself never ends up in the binary either.
    for pid in list(runtime):
        if any("proc-macro" in t["kind"] for t in pkgs[pid]["targets"]):
            build_only.add(pid)
            runtime.discard(pid)
    return edges, build_only, root


def licenses(pkg: dict) -> list[dict]:
    if pkg.get("license"):
        return [{"expression": pkg["license"]}]
    if pkg.get("license_file"):
        return [{"license": {"name": "see license_file", "url": pkg.get("repository") or ""}}]
    return []


def sbom(target: str) -> dict:
    meta = cargo_metadata(target)
    sums = lock_checksums()
    pkgs = {p["id"]: p for p in meta["packages"]}
    edges, build_only, root = reachable(meta)
    ref = {pid: purl(pkgs[pid]) for pid in edges}
    root_pkg = pkgs[root]
    version = root_pkg["version"]

    components = []
    for pid in sorted(edges, key=lambda i: (pkgs[i]["name"], pkgs[i]["version"])):
        if pid == root:
            continue
        p = pkgs[pid]
        c: dict = {
            "type": "library",
            "bom-ref": ref[pid],
            "name": p["name"],
            "version": p["version"],
            "purl": ref[pid],
        }
        if p.get("description"):
            c["description"] = " ".join(p["description"].split())
        lic = licenses(p)
        if lic:
            c["licenses"] = lic
        if pid in build_only:
            c["scope"] = "excluded"  # build scripts and proc macros: not in the binary
        checksum = sums.get((p["name"], p["version"]))
        if checksum and p.get("source") == CRATES_IO:
            c["hashes"] = [{"alg": "SHA-256", "content": checksum}]
        refs = []
        if p.get("repository"):
            refs.append({"type": "vcs", "url": p["repository"]})
        if p.get("source") == CRATES_IO:
            refs.append({"type": "distribution", "url": f"https://crates.io/crates/{p['name']}/{p['version']}"})
        if refs:
            c["externalReferences"] = refs
        components.append(c)

    lock_digest = hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest()
    return {
        "$schema": "http://cyclonedx.org/schema/bom-1.5.schema.json",
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(NAMESPACE, f'{version}|{target}|{lock_digest}')}",
        "version": 1,
        "metadata": {
            "timestamp": timestamp(),
            "tools": {
                "components": [
                    {"type": "application", "name": "packaging/sbom.py", "version": version,
                     "description": "scull's stdlib SBOM generator over cargo metadata"}
                ]
            },
            "component": {
                "type": "application",
                "bom-ref": ref[root],
                "name": BINARY,
                "version": version,
                "description": root_pkg.get("description") or "",
                "licenses": licenses(root_pkg),
                "purl": f"pkg:github/jofftiquez/scull@v{version}",
                "externalReferences": [{"type": "vcs", "url": REPOSITORY}],
            },
            "properties": [
                {"name": "cdx:rust:target", "value": target},
                {"name": "cdx:rust:cargo-lock-sha256", "value": lock_digest},
            ],
        },
        "components": components,
        "dependencies": [
            {"ref": ref[pid], "dependsOn": sorted(ref[d] for d in edges[pid])}
            for pid in sorted(edges, key=lambda i: ref[i])
        ],
    }


def write(doc: dict, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(doc, indent=2, sort_keys=False) + "\n")
    print(f"wrote {path} ({len(doc['components'])} components)")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    group = ap.add_mutually_exclusive_group(required=True)
    group.add_argument("--target", help="one target triple")
    group.add_argument("--all-targets", action="store_true", help="every dist-workspace target")
    ap.add_argument("--out", type=Path, help="output file (with --target)")
    ap.add_argument("--out-dir", type=Path, help="output folder: scull-cli-<target>.cdx.json")
    a = ap.parse_args()
    targets = dist_targets() if a.all_targets else [a.target]
    for t in targets:
        doc = sbom(t)
        if a.out and not a.all_targets:
            write(doc, a.out)
        else:
            write(doc, (a.out_dir or Path(".")) / f"{ROOT_PACKAGE}-{t}.cdx.json")
    return 0


if __name__ == "__main__":
    sys.exit(main())
