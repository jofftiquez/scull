#!/usr/bin/env python3
"""Release gates for the inspect-only alpha (P1-REL-01, PRD §22 Phase 1).

Two checks, both stdlib-only so they run unchanged on Linux, macOS and Windows:

``deps``
    ``cargo tree -p scull-cli`` (normal + build edges, every target) contains no Tauri, wry,
    tao or webview crates (HANDOVER §8.1: building the CLI must not pull in the desktop).

``binary <path-to-scull>``
    The built ``scull`` binary has no destructive path wired in:

    * every command that would delete, move, restore or purge (``clean`` in all its forms,
      ``clean caches --confirm --wipe``, ``restore``, ``approve``, ``data purge``,
      ``insights purge``, ...) exits 1 with "is not implemented yet";
    * ``scull mcp serve --allow-destructive`` lists only tools annotated
      ``readOnlyHint: true`` and ``destructiveHint: false``;
    * a canary tree that every command is pointed at is byte-for-byte unchanged afterwards.

    Everything runs inside a fresh temp directory with ``SCULL_HOME``/``HOME`` redirected
    there, so the real machine is never touched.

This gate applies to every release until Phase 2 (MVP) ships the Executor with its own
release gates (PRD §21.3); at that point the destructive list below must be revised by the
safety lane, not deleted.

Usage::

    python3 packaging/check_inspect_only.py deps
    python3 packaging/check_inspect_only.py binary target/release/scull
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

# Crates that mean the desktop shell leaked into the CLI build.
FORBIDDEN_CRATES = ("tauri", "wry", "tao", "webview2-com", "webkit2gtk", "javascriptcore-rs")

NOT_IMPLEMENTED = "is not implemented yet"

# Every invocation that could delete, move, restore, purge or otherwise mutate the user's
# files or system. In the alpha each must be refused as not implemented (exit 1). Paths are
# relative to the canary tree (the working directory).
DESTRUCTIVE = [
    ["clean"],
    ["clean", "--dry-run"],
    ["clean", "--dry-run", "--output", "plan.json"],
    ["clean", "--plan", "plan.json", "--confirm"],
    ["clean", "--plan", "plan.json", "--confirm", "--allow-permanent"],
    ["clean", "--plan", "plan.json", "--confirm", "--allow-permanent", "--allow-root"],
    ["clean", "caches"],
    ["clean", "caches", "--confirm"],
    ["clean", "caches", "--confirm", "--wipe"],
    ["restore", "op-0000"],
    ["approve", "plan-0000"],
    ["data", "purge"],
    ["insights", "purge"],
    ["worktrees", "register", "tree", "--ttl", "1d"],
    ["schedule", "enable"],
    ["schedule", "disable"],
    ["agent", "install", "claude-code"],
]


def fail(msg: str) -> None:
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def check_deps() -> None:
    cmd = [
        "cargo", "tree", "-p", "scull-cli", "--locked",
        "-e", "normal,build", "--target", "all", "--prefix", "none", "--format", "{p}",
    ]
    out = subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    names = sorted({line.split(" ", 1)[0] for line in out.splitlines() if line.strip()})
    bad = [n for n in names if any(n == f or n.startswith(f + "-") for f in FORBIDDEN_CRATES)]
    if bad:
        fail(f"scull-cli depends on desktop crates: {', '.join(bad)}")
    print(f"ok: scull-cli dependency tree ({len(names)} crates, all targets) has no Tauri/webview crates")


def snapshot(root: Path) -> dict[str, str]:
    """Relative path -> sha256 of contents (or a marker for directories)."""
    snap: dict[str, str] = {}
    for dirpath, dirnames, filenames in os.walk(root):
        for d in dirnames:
            p = Path(dirpath, d)
            snap[p.relative_to(root).as_posix() + "/"] = "dir"
        for f in filenames:
            p = Path(dirpath, f)
            snap[p.relative_to(root).as_posix()] = hashlib.sha256(p.read_bytes()).hexdigest()
    return snap


def make_canary(root: Path) -> None:
    """A small tree that looks like reclaimable developer junk, plus a canary file."""
    (root / "tree" / "src").mkdir(parents=True)
    (root / "tree" / "Cargo.toml").write_text('[package]\nname = "tree"\nversion = "0.1.0"\n')
    (root / "tree" / "src" / "main.rs").write_text("fn main() {}\n")
    (root / "tree" / "target" / "debug").mkdir(parents=True)
    (root / "tree" / "target" / "debug" / "big.bin").write_bytes(b"\0" * 65536)
    (root / "tree" / "node_modules" / "left-pad").mkdir(parents=True)
    (root / "tree" / "node_modules" / "left-pad" / "index.js").write_text("module.exports=1\n")
    (root / "tree" / "package.json").write_text('{"name":"tree"}\n')
    (root / "CANARY-DO-NOT-DELETE").write_text("scull must never touch this file\n")
    # A syntactically valid plan file, so `--plan plan.json` reaches the command itself.
    (root / "plan.json").write_text('{"schema_version": 1, "items": []}\n')


def env_for(home: Path) -> dict[str, str]:
    env = dict(os.environ)
    for k in ("SCULL_MCP", "CARGO_TARGET_DIR"):
        env.pop(k, None)
    env.update(
        {
            "SCULL_HOME": str(home / "scull-home"),
            "HOME": str(home),
            "USERPROFILE": str(home),
            "LOCALAPPDATA": str(home / "AppData" / "Local"),
            "APPDATA": str(home / "AppData" / "Roaming"),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local" / "share"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "XDG_STATE_HOME": str(home / ".local" / "state"),
            "NO_COLOR": "1",
            "TERM": "dumb",
        }
    )
    return env


def check_destructive(scull: str, canary: Path, env: dict[str, str]) -> None:
    for args in DESTRUCTIVE:
        p = subprocess.run(
            [scull, *args], cwd=canary, env=env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=60,
        )
        shown = "scull " + " ".join(args)
        if p.returncode != 1 or NOT_IMPLEMENTED not in p.stderr:
            fail(
                f"`{shown}` must be refused as not implemented in the alpha "
                f"(got exit {p.returncode}; stderr: {p.stderr.strip()!r})"
            )
        print(f"ok: `{shown}` -> exit 1, {p.stderr.strip().splitlines()[-1]}")


def check_mcp(scull: str, canary: Path, home: Path, env: dict[str, str]) -> None:
    config_dir = home / "scull-home" / "config"
    config_dir.mkdir(parents=True, exist_ok=True)
    root = str(canary / "tree").replace("\\", "\\\\")
    (config_dir / "config.toml").write_text(
        f'[scan]\nroots = ["{root}"]\n\n[mcp]\nenabled = true\n'
    )
    msgs = [
        {
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "release-gate", "version": "0"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}},
    ]
    proc = subprocess.Popen(
        [scull, "mcp", "serve", "--allow-destructive"], cwd=canary, env=env,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    assert proc.stdin and proc.stdout
    tools = None
    try:
        for m in msgs:
            proc.stdin.write(json.dumps(m) + "\n")
            proc.stdin.flush()
            if "id" not in m:
                continue
            while True:
                line = proc.stdout.readline()
                if not line:
                    fail(f"mcp server closed stdout: {proc.stderr.read() if proc.stderr else ''}")
                v = json.loads(line)
                if v.get("id") == m["id"]:
                    break
            if m["id"] == 2:
                tools = v["result"]["tools"]
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
            fail("mcp server did not exit after stdin closed")
    if not tools:
        fail("mcp server listed no tools")
    for t in tools:
        ann = t.get("annotations") or {}
        if ann.get("readOnlyHint") is not True or ann.get("destructiveHint") is not False:
            fail(f"MCP tool {t['name']!r} is not annotated read-only/non-destructive: {ann}")
    names = ", ".join(t["name"] for t in tools)
    print(f"ok: `scull mcp serve --allow-destructive` lists {len(tools)} read-only tools: {names}")


def check_binary(scull: str) -> None:
    scull = str(Path(scull).resolve())
    with tempfile.TemporaryDirectory(prefix="scull-release-gate-") as tmp:
        base = Path(tmp)
        home = base / "home"
        canary = base / "canary"
        home.mkdir()
        canary.mkdir()
        make_canary(canary)
        before = snapshot(canary)
        env = env_for(home)
        version = subprocess.run(
            [scull, "--version"], env=env, capture_output=True, text=True, check=True
        ).stdout.strip()
        print(f"checking {version} ({scull})")
        check_destructive(scull, canary, env)
        check_mcp(scull, canary, home, env)
        after = snapshot(canary)
        if after != before:
            changed = sorted(set(before.items()) ^ set(after.items()))
            fail(f"canary tree changed: {changed}")
        print(f"ok: canary tree unchanged ({len(before)} entries)")
    print("inspect-only gate passed")


def main(argv: list[str]) -> None:
    if len(argv) >= 2 and argv[1] == "deps":
        check_deps()
    elif len(argv) == 3 and argv[1] == "binary":
        check_binary(argv[2])
    else:
        print(__doc__, file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main(sys.argv)
