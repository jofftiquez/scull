#!/usr/bin/env python3
"""Release gates for the inspect-only alpha (P1-REL-01, PRD §22 Phase 1).

Two checks, both stdlib-only so they run unchanged on Linux, macOS and Windows:

``deps``
    ``cargo tree -p scull-cli`` (normal + build edges, every target) contains no Tauri, wry,
    tao or webview crates (building the CLI must not pull in the desktop).

``binary <path-to-scull>``
    The built ``scull`` binary has no destructive path wired in:

    * every command that would delete, move, restore or purge (``clean`` in all its forms,
      ``clean caches --confirm --wipe``, ``restore``, ``approve``, ``data purge``,
      ``insights purge``, ...) exits 1 with "is not implemented yet";
    * ``scull agent install <client>`` without ``--yes`` (and without a terminal to confirm
      on) refuses and leaves the home folder unchanged; ``--dry-run`` only shows the diff;
    * ``scull schedule enable`` likewise refuses without ``--yes`` and ``--dry-run`` only shows
      the unit files and scheduler commands; ``disable``/``status`` with nothing registered
      change nothing (no real schedule is ever registered by the gate);
    * ``scull worktrees register`` writes nothing outside ``SCULL_HOME``;
    * ``scull mcp serve --allow-destructive`` lists only tools annotated
      ``destructiveHint: false``, all ``readOnlyHint: true`` except the metadata-only
      ``register_worktree``, which, called on a real linked worktree, writes only under
      ``SCULL_HOME`` and leaves the worktree unchanged;
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
]

# Commands that edit an AI client's config only with explicit consent (P4-AI-01): `--yes`, or a
# "y" typed at an interactive prompt. Run as the gate runs them (stdin is not a terminal, no
# `--yes`), each must exit with the code given and leave the home folder byte-for-byte
# unchanged. `--dry-run` only shows the diff. With no scull entry, uninstall has nothing to do.
CONSENT_GATED = [
    (["agent", "install", "claude-code"], 2),
    (["agent", "install", "codex"], 2),
    (["agent", "install", "claude-code", "--dry-run"], 0),
    (["agent", "uninstall", "claude-code"], 0),
    # P4-PRO-03: registering the periodic `scull check` needs `--yes` or a typed "y"; the gate
    # never passes `--yes`, so no real launchd/systemd/Task Scheduler job is ever created.
    # With nothing registered, disable has nothing to do and status runs no scheduler tool.
    (["schedule", "enable"], 2),
    (["schedule", "enable", "--dry-run"], 0),
    (["schedule", "disable"], 0),
    (["schedule", "status"], 0),
    # P5-AI-01: `scull ask` is off by default; it refuses (exit 2) and sends nothing until the
    # user enables it, and `--show-request` only prints what would be sent.
    (["ask", "why is my disk full?"], 2),
    (["ask", "--show-request", "why is my disk full?"], 0),
]


# Commands that may record metadata in scull's own data folder (P4-AI-02) and nowhere else: the
# home folder outside ``SCULL_HOME`` and the canary tree must stay byte-for-byte unchanged
# (``tree`` is not a Git worktree, so this one is refused, but it still must not write).
SCULL_HOME_ONLY = [
    ["worktrees", "register", "tree", "--ttl", "1d"],
]

# MCP tools that record metadata in scull's data folder (PRD §13.2: ``register_worktree``
# records a TTL, never deletes). They may be annotated ``readOnlyHint: false`` but must be
# ``destructiveHint: false``; every other tool must be read-only and non-destructive.
METADATA_ONLY = {"register_worktree"}

SCULL_HOME_DIR = "scull-home"


def fail(msg: str) -> None:
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


def check_deps() -> None:
    cmd = [
        "cargo", "tree", "-p", "scull-cli", "--locked", "--features", "full",
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


def check_consent_gated(scull: str, canary: Path, home: Path, env: dict[str, str]) -> None:
    before = snapshot(home)
    for args, code in CONSENT_GATED:
        p = subprocess.run(
            [scull, *args], cwd=canary, env=env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=60,
        )
        shown = "scull " + " ".join(args)
        if p.returncode != code:
            fail(
                f"`{shown}` without consent must exit {code} "
                f"(got exit {p.returncode}; stderr: {p.stderr.strip()!r})"
            )
        after = snapshot(home)
        if after != before:
            changed = sorted(set(before.items()) ^ set(after.items()))
            fail(f"`{shown}` without consent changed the home folder: {changed}")
        print(f"ok: `{shown}` -> exit {code}, home folder unchanged")


def outside_scull_home(snap: dict[str, str]) -> dict[str, str]:
    """``snap`` (of the gate's home folder) without anything under ``SCULL_HOME``."""
    return {k: v for k, v in snap.items() if not k.startswith(SCULL_HOME_DIR + "/")}


def check_scull_home_only(scull: str, canary: Path, home: Path, env: dict[str, str]) -> None:
    for args in SCULL_HOME_ONLY:
        before_home = outside_scull_home(snapshot(home))
        before_canary = snapshot(canary)
        p = subprocess.run(
            [scull, *args], cwd=canary, env=env, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=60,
        )
        shown = "scull " + " ".join(args)
        if outside_scull_home(snapshot(home)) != before_home:
            fail(f"`{shown}` wrote to the home folder outside SCULL_HOME")
        if snapshot(canary) != before_canary:
            fail(f"`{shown}` changed the canary tree")
        print(f"ok: `{shown}` -> exit {p.returncode}, nothing written outside SCULL_HOME")


def check_tool_annotations(tools: list[dict]) -> None:
    """Every tool read-only and non-destructive, except METADATA_ONLY tools, which may write
    metadata (readOnlyHint false) but are never destructive."""
    for t in tools:
        ann = t.get("annotations") or {}
        if ann.get("destructiveHint") is not False:
            fail(f"MCP tool {t['name']!r} is not annotated non-destructive: {ann}")
        if t["name"] in METADATA_ONLY:
            continue
        if ann.get("readOnlyHint") is not True:
            fail(f"MCP tool {t['name']!r} is not annotated read-only: {ann}")


def make_worktree(base: Path, env: dict[str, str]) -> Path | None:
    """A Git repository with a linked worktree under ``base`` (None without git)."""
    repo = base / "repo"
    try:
        def git(*args: str, cwd: Path = repo) -> None:
            subprocess.run(["git", *args], cwd=cwd, env=env, check=True, capture_output=True,
                           timeout=60)
        repo.mkdir(parents=True)
        git("init", "-q", "-b", "main")
        (repo / "README.md").write_text("gate\n")
        git("add", "README.md")
        git("-c", "user.name=gate", "-c", "user.email=gate@example.invalid",
            "commit", "-q", "-m", "init")
        git("worktree", "add", "-q", "-b", "agent", str(base / "repo-wt"))
    except (OSError, subprocess.SubprocessError):
        return None
    return base / "repo-wt"


def check_mcp(scull: str, canary: Path, home: Path, env: dict[str, str]) -> None:
    config_dir = home / "scull-home" / "config"
    config_dir.mkdir(parents=True, exist_ok=True)
    root = str(canary / "tree").replace("\\", "\\\\")
    # A linked worktree for register_worktree, outside the canary and the home folder.
    wt_base = home.parent / "wt"
    worktree = make_worktree(wt_base, env)
    wt_root = str(wt_base).replace("\\", "\\\\")
    (config_dir / "config.toml").write_text(
        f'[scan]\nroots = ["{root}", "{wt_root}"]\n\n[mcp]\nenabled = true\n'
    )
    before_home = outside_scull_home(snapshot(home))
    before_wt = snapshot(wt_base) if worktree else {}
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
    if worktree:
        msgs.append({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "register_worktree",
                       "arguments": {"path": str(worktree), "ttl": "1d"}},
        })
    registered = None
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
            if m["id"] == 3:
                registered = v.get("result") or {}
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
            fail("mcp server did not exit after stdin closed")
    if not tools:
        fail("mcp server listed no tools")
    check_tool_annotations(tools)
    names = ", ".join(t["name"] for t in tools)
    print(f"ok: `scull mcp serve --allow-destructive` lists {len(tools)} non-destructive tools "
          f"(metadata-only: {', '.join(sorted(METADATA_ONLY))}): {names}")
    if worktree:
        if registered is None or registered.get("isError"):
            fail(f"register_worktree failed on a linked worktree: {registered}")
        if outside_scull_home(snapshot(home)) != before_home:
            fail("register_worktree wrote to the home folder outside SCULL_HOME")
        if snapshot(wt_base) != before_wt:
            fail("register_worktree changed the worktree or its repository")
        if not (home / SCULL_HOME_DIR / "data" / "worktree-ttl.jsonl").is_file():
            fail("register_worktree recorded nothing in SCULL_HOME")
        print("ok: register_worktree recorded a TTL in SCULL_HOME only; worktree unchanged")
    else:
        print("note: git unavailable; register_worktree's write check skipped")


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
        check_scull_home_only(scull, canary, home, env)
        check_consent_gated(scull, canary, home, env)
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
