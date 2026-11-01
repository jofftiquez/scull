#!/usr/bin/env python3
"""SAFE-10 on the desktop's resolved dependency graph (P3-EXEC-01), metadata only (no build,
no WebKitGTK): the PRD rule is "deletion is only possible through the core Executor API;
Desktop and MCP have no direct filesystem-deletion code paths".

Checks, with `cargo metadata --locked` (normal dependencies, every target):

* apps/desktop/src-tauri: the only package depending on `scull-cleaner` is
  `scull-desktop-bridge` (the Tauri app crate reaches it only through the bridge, whose
  `src/cleanup.rs` is the one module allowed to use it; bridge/tests/safe10.rs checks that);
* the root workspace: `scull-mcp` reaches neither `scull-cleaner` nor `scull-desktop-bridge`;
* both: no normal or build dependency enables `scull-cleaner`'s `bare-api` feature (the
  Executor's bare delete/trash methods, which skip the operation, journal and revalidation, are
  public only to scull-cleaner's and scull-fixtures' own tests).

Renamed dependencies (`foo = { package = "scull-cleaner" }`) are resolved: metadata names
packages, not the keys they were declared under.

Usage: python3 scripts/ci/desktop_safe10.py [--root DIR]
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path


def metadata(manifest: Path) -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--manifest-path", str(manifest)],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(out.stdout)


def graph(meta: dict) -> tuple[dict, dict]:
    """(package id -> name, package id -> ids of its normal dependencies)."""
    names = {p["id"]: p["name"] for p in meta["packages"]}
    deps = {}
    for node in meta["resolve"]["nodes"]:
        deps[node["id"]] = [
            d["pkg"]
            for d in node["deps"]
            if any(k.get("kind") in (None, "normal") for k in d["dep_kinds"])
        ]
    return names, deps


def dependents_of(name: str, names: dict, deps: dict) -> set[str]:
    return {names[i] for i, ds in deps.items() if any(names[d] == name for d in ds)}


def reaches(start: str, target: str, names: dict, deps: dict) -> bool:
    todo = [i for i, n in names.items() if n == start]
    seen = set()
    while todo:
        i = todo.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in deps.get(i, []):
            if names[d] == target:
                return True
            todo.append(d)
    return False


def bare_api_users(meta: dict) -> set[str]:
    """Packages that enable scull-cleaner's `bare-api` through a non-dev dependency."""
    out = set()
    for p in meta["packages"]:
        for d in p["dependencies"]:
            if d["name"] == "scull-cleaner" and d.get("kind") != "dev" and "bare-api" in d["features"]:
                out.add(p["name"])
    return out


def check(root: Path) -> list[str]:
    problems = []
    desktop = metadata(root / "apps/desktop/src-tauri/Cargo.toml")
    workspace = metadata(root / "Cargo.toml")
    for meta in (desktop, workspace):
        for user in sorted(bare_api_users(meta)):
            problems.append(f"{user} enables scull-cleaner/bare-api outside its tests (SAFE-10)")
    names, deps = graph(desktop)
    if not any(n == "scull-cleaner" for n in names.values()):
        problems.append("the desktop graph has no scull-cleaner: is the bridge wired? (P3-EXEC-01)")
    users = dependents_of("scull-cleaner", names, deps)
    if users and users != {"scull-desktop-bridge"}:
        problems.append(
            f"scull-cleaner is used by {sorted(users)} in the desktop graph; only "
            "scull-desktop-bridge may use it (SAFE-10)"
        )
    names, deps = graph(workspace)
    for target in ("scull-cleaner", "scull-desktop-bridge"):
        if reaches("scull-mcp", target, names, deps):
            problems.append(f"scull-mcp reaches {target} (SAFE-10: MCP never deletes)")
    return problems


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    args = ap.parse_args()
    problems = check(args.root)
    for p in problems:
        print(f"desktop-safe10: {p}", file=sys.stderr)
    print(f"desktop-safe10: {len(problems)} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
