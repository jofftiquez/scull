#!/usr/bin/env python3
"""MVP release gates (PRD §21.3, docs/FIXTURES.md): what replaces the inspect-only alpha gate
(packaging/check_inspect_only.py) once the Executor ships. Stdlib only.

``deps``
    ``cargo tree -p scull-cli`` (normal + build edges, every target) has no Tauri, wry, tao or
    webview crates: the CLI and MCP install without the desktop (PLT-05). Same check as the
    alpha gate.

``fixtures [--allow-skips] [--report FILE]``
    The fixture gate, run on each of macOS, Linux and Windows: "every MVP-gated fixture passes
    on macOS, Linux and Windows, with zero invariant violations". The fixture registry
    (``crates/scull-fixtures/src/registry.rs``, exported by its ``registry_json`` example)
    decides what runs. It **fails** when an MVP fixture is still ``Tracked`` or ``Deferred``,
    when an MVP fixture for this OS has no test here (``missing_os``), or when any registered
    test of an MVP fixture for this OS fails, skips itself (``SKIPPED <test>``), or does not
    run at all. Every fixture test checks the universal invariants itself
    (``check_invariants(...).assert_ok()``), so "passed" includes "zero invariant violations".
    Nightly-mode MVP fixtures run with ``--include-ignored``, one test at a time, with
    ``SCULL_FIXTURES_REQUIRE_PRIVILEGED=1`` (a privileged fixture that cannot get its privileges
    fails instead of skipping). Manual-checklist fixtures are listed: they need the signed
    manual checklist (FIXTURES.md §2.2), which a script cannot provide.

``binary <path-to-scull>``
    The built binary's destructive commands are present and gated exactly as designed, through
    the real binary in throwaway temp trees (``SCULL_HOME``/``HOME`` redirected, the Executor
    sandboxed with ``SCULL_TEST_SANDBOX`` and ``SCULL_QUARANTINE_BASE``, a canary file beside
    every tree): no execution without ``--confirm`` or a terminal; tampered and foreign plans
    refused; plans single-use; elevated runs refused without ``--allow-root``; not-undoable
    purges need ``--allow-permanent`` (or the typed count on a terminal); destructive MCP mode
    needs both ``[mcp] destructive = true`` and ``--allow-destructive`` and is never offered
    when elevated; a confirmed plan changes exactly its items and never the canary.

Usage::

    python3 packaging/check_mvp_gates.py deps
    python3 packaging/check_mvp_gates.py fixtures --report mvp-fixtures-linux.json
    python3 packaging/check_mvp_gates.py binary target/release/scull
"""

from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# The alpha gate's dependency check is reused unchanged.
_spec = importlib.util.spec_from_file_location("inspect_only", Path(__file__).with_name("check_inspect_only.py"))
inspect_only = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(inspect_only)

# Exit codes (CLI-05).
EXIT_OK, EXIT_GENERAL, EXIT_USAGE, EXIT_PARTIAL, EXIT_PLAN_REJECTED, EXIT_SAFETY = 0, 1, 2, 3, 4, 5
EXIT_CANCELLED = 130

FAILURES: list[str] = []


def fail(msg: str) -> None:
    FAILURES.append(msg)
    print(f"FAIL: {msg}", file=sys.stderr)


def ok(msg: str) -> None:
    print(f"ok: {msg}")


# --------------------------------------------------------------------------------------------
# fixtures
# --------------------------------------------------------------------------------------------


def this_os() -> str:
    s = platform.system()
    return {"Darwin": "M", "Linux": "L", "Windows": "W"}.get(s, "?")


def load_registry() -> list[dict]:
    out = subprocess.run(
        ["cargo", "run", "-q", "--locked", "-p", "scull-fixtures", "--example", "registry_json"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)["fixtures"]


def test_target(file: str) -> tuple[str, list[str]]:
    """(package, cargo test target args) for a registered test file."""
    parts = file.split("/")
    if len(parts) < 4 or parts[0] != "crates":
        raise ValueError(f"unexpected test location {file}")
    package = parts[1]
    if parts[2] == "src":
        return package, ["--lib"]
    if parts[2] == "tests":
        rel = "/".join(parts[2:])
        manifest = tomllib.loads((ROOT / "crates" / package / "Cargo.toml").read_text())
        for t in manifest.get("test", []):
            if t.get("path") == rel:
                return package, ["--test", t["name"]]
        if len(parts) == 4:
            return package, ["--test", parts[3].removesuffix(".rs")]
        if parts[-1] == "main.rs":
            return package, ["--test", parts[3]]
    raise ValueError(f"unexpected test location {file}")


TEST_LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b", re.M)
SKIP_LINE = re.compile(r"^SKIPPED (\S+?):?(?:\s|$)", re.M)


def run_group(package: str, target: list[str], fns: list[str], nightly: bool) -> tuple[dict[str, str], str]:
    """Run `fns` of one test target; returns ({fn: ok|FAILED|ignored|skipped|not run}, output)."""
    # scull-cli's tests of full-only commands are compiled out without `full` (#142), which
    # would make them pass vacuously.
    features = ["--features", "full"] if package == "scull-cli" else []
    cmd = ["cargo", "test", "--locked", "-p", package, *features, *target, "--"]
    if nightly:
        cmd += ["--include-ignored", "--test-threads=1"]
    cmd += fns
    env = dict(os.environ)
    if nightly:
        env.setdefault("SCULL_FIXTURES_REQUIRE_PRIVILEGED", "1")
        if platform.system() == "Darwin":
            # FX-EXEC-16 reads the fixture as `nobody`: macOS's per-user $TMPDIR (0700) can't
            # be traversed by another account; /tmp can (as in nightly-fixtures.yml).
            env["TMPDIR"] = "/tmp"
    p = subprocess.run(cmd, cwd=ROOT, env=env, capture_output=True, text=True)
    out = p.stdout + p.stderr
    return parse_results(out, fns, p.returncode), out


def parse_results(out: str, fns: list[str], returncode: int) -> dict[str, str]:
    """Each requested test's outcome in libtest-style output: ok, FAILED, ignored, skipped (it
    printed `SKIPPED <name>: <reason>`, the fixtures' convention, and passed) or "n/a" (not in
    the output at all: compiled out on this OS)."""
    seen: dict[str, str] = {}
    for name, status in TEST_LINE.findall(out):
        seen[name] = "FAILED" if seen.get(name) == "FAILED" else status
    result = {}
    for fn in fns:
        hits = [s for n, s in seen.items() if n == fn or n.endswith("::" + fn)]
        if not hits:
            # Not in the output at all: the test is compiled out on this OS (`#[cfg(windows)]`
            # and the like). registry.rs's own tests prove every registered function exists, so
            # this is "not for this OS"; the fixture still needs at least one test that ran.
            result[fn] = "n/a"
        elif "FAILED" in hits:
            result[fn] = "FAILED"
        elif all(h == "ignored" for h in hits):
            result[fn] = "ignored"
        else:
            result[fn] = "ok"
    for line in out.splitlines():
        m = SKIP_LINE.match(line)
        if not m:
            continue
        for fn in fns:
            if m.group(1) == fn or m.group(1).endswith("::" + fn):
                result[fn] = "skipped"
    if returncode != 0 and all(v in ("ok", "n/a") for v in result.values()):
        result = {fn: "FAILED" for fn in fns}
    return result


def check_fixtures(allow_skips: bool, report: Path | None) -> None:
    os_letter = this_os()
    # FX-OUT/FX-DATA and the CLI tests drive the real `scull` binary (target/debug/scull);
    # `cargo test -p scull-fixtures` alone does not build it.
    # The released command set (#142: the slim build is the source default).
    subprocess.run(["cargo", "build", "--locked", "-p", "scull-cli", "--features", "full"], cwd=ROOT, check=True)
    registry = load_registry()
    mvp = [f for f in registry if f["gate"] == "MVP"]
    print(f"MVP fixture gate on {platform.system()} ({os_letter}): {len(mvp)} MVP fixtures registered")
    rows = []
    groups: dict[tuple[str, tuple[str, ...], bool], set[str]] = {}
    plan: list[tuple[dict, list[tuple[tuple[str, tuple[str, ...], bool], str]]]] = []
    for f in mvp:
        if os_letter not in f["os"]:
            rows.append((f["id"], "n/a on this OS", ""))
            continue
        if f["status"] in ("tracked", "deferred"):
            why = f"issue #{f['issue']}: {f['reason']}" if f["status"] == "tracked" else "not started"
            fail(f"{f['id']} is an MVP fixture still {f['status'].upper()} ({why})")
            rows.append((f["id"], f["status"].upper(), why))
            continue
        if f["status"] == "manual_checklist":
            rows.append((f["id"], "manual", "needs the signed manual checklist (FIXTURES.md §2.2)"))
            continue
        if os_letter in f["missing_os"]:
            fail(f"{f['id']}: no registered test covers this OS ({os_letter})")
            rows.append((f["id"], "MISSING", "no test for this OS"))
            continue
        refs = []
        for t in f["tests"]:
            package, target = test_target(t["file"])
            key = (package, tuple(target), f["mode"] == "Nightly")
            groups.setdefault(key, set()).add(t["test_fn"])
            refs.append((key, t["test_fn"]))
        if not refs:
            fail(f"{f['id']}: implemented but lists no test")
        plan.append((f, refs))

    results: dict[tuple[tuple[str, tuple[str, ...], bool], str], str] = {}
    for (package, target, nightly), fns in sorted(groups.items()):
        label = f"{package} {' '.join(target)}{' (nightly)' if nightly else ''}"
        started = time.monotonic()
        res, out = run_group(package, list(target), sorted(fns), nightly)
        bad = {k: v for k, v in res.items() if v not in ("ok", "n/a")}
        na = sum(1 for v in res.values() if v == "n/a")
        passed = sum(1 for v in res.values() if v == "ok")
        print(f"  ran {label}: {len(fns)} test(s), {passed} passed"
              f"{f', {na} not built on this OS' if na else ''} ({time.monotonic() - started:.0f}s)")
        if bad:
            tail = "\n".join(out.splitlines()[-25:])
            print(f"    problems: {bad}\n{tail}", file=sys.stderr)
        for fn, status in res.items():
            results[((package, target, nightly), fn)] = status

    for f, refs in plan:
        statuses = [results[r] for r in refs]
        problems = [f"{fn}: {s}" for (_, fn), s in zip(refs, statuses) if s not in ("ok", "n/a")]
        if allow_skips:
            problems = [p for p in problems if not p.endswith(": skipped")]
        if not problems and "ok" not in statuses:
            problems = [f"no registered test ran on this OS ({', '.join(sorted(set(statuses)))})"]
        if problems:
            fail(f"{f['id']}: " + "; ".join(problems))
            rows.append((f["id"], "FAILED", "; ".join(problems)))
        else:
            rows.append((f["id"], "passed", f"{len(refs)} test(s)"))

    passed = sum(1 for r in rows if r[1] == "passed")
    blocked = [r for r in rows if r[1] not in ("passed", "n/a on this OS", "manual")]
    print(f"\n{passed} MVP fixtures passed on this OS; {len(blocked)} block the release; "
          f"{sum(1 for r in rows if r[1] == 'manual')} need the manual checklist")
    if report:
        report.write_text(json.dumps(
            {"os": os_letter, "fixtures": [{"id": i, "result": r, "detail": d} for i, r, d in rows]},
            indent=2) + "\n")


# --------------------------------------------------------------------------------------------
# binary
# --------------------------------------------------------------------------------------------

OLD = time.time() - 30 * 24 * 3600


def write(p: Path, data: str | bytes) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(data, bytes):
        p.write_bytes(data)
    else:
        p.write_text(data)


def cargo_project(root: Path, name: str, target_bytes: int) -> None:
    write(root / name / "Cargo.toml", f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n')
    write(root / name / "Cargo.lock", "version = 4\n")
    write(root / name / "src" / "main.rs", "fn main() {}\n")
    t = root / name / "target"
    write(t / "CACHEDIR.TAG", "Signature: 8a477f597d28d172789f06886806bc55\n")
    write(t / ".rustc_info.json", '{"rustc_fingerprint":0,"outputs":{}}')
    write(t / "debug" / ".fingerprint" / f"{name}-0123456789abcdef" / f"bin-{name}", "0123456789abcdef")
    write(t / "debug" / name, b"\0" * target_bytes)


class Tree:
    """A throwaway tree: `root` (scanned), a canary beside it, scull's home, the sandbox."""

    def __init__(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="scull-mvp-gate-"))
        self.root = self.tmp / "root"
        cargo_project(self.root, "app", 64 * 1024)
        write(self.root / "web" / "package.json", '{"name":"web","version":"1.0.0"}\n')
        write(self.root / "web" / "package-lock.json", '{"lockfileVersion":3,"packages":{}}\n')
        write(self.root / "web" / "node_modules" / "left-pad" / "package.json", '{"name":"left-pad"}\n')
        write(self.root / "web" / "node_modules" / "left-pad" / "index.js", "module.exports=1\n")
        cargo_project(self.root, "vault", 4096)
        write(self.root / "vault" / ".scull-protect", "")
        write(self.root / "notes" / "todo.txt", "keep me\n")
        for dirpath, dirnames, filenames in os.walk(self.root):
            for n in dirnames + filenames:
                os.utime(Path(dirpath, n), (OLD, OLD), follow_symlinks=False)
        self.canary = self.tmp / "CANARY-DO-NOT-DELETE"
        write(self.canary / "keep.txt", "scull must never touch this\n")
        (self.tmp / "home").mkdir()
        self.canary_before = inspect_only.snapshot(self.canary)

    def env(self, **extra: str) -> dict[str, str]:
        env = inspect_only.env_for(self.tmp / "home")
        for k in list(env):
            if k.startswith("SCULL_") and k != "SCULL_HOME":
                env.pop(k)
        env["SCULL_TEST_SANDBOX"] = str(self.tmp)
        env["SCULL_QUARANTINE_BASE"] = str(self.tmp / "quarantine")
        env.update(extra)
        return env

    def snapshot(self) -> dict[str, str]:
        return inspect_only.snapshot(self.root)

    def canary_intact(self, what: str) -> None:
        if inspect_only.snapshot(self.canary) != self.canary_before:
            fail(f"{what}: the canary beside the tree changed")

    def cleanup(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)


def run(scull: str, tree: Tree, args: list[str], env: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [scull, *args], cwd=tree.tmp, env=env or tree.env(), stdin=subprocess.DEVNULL,
        capture_output=True, text=True, timeout=180,
    )


def expect(p: subprocess.CompletedProcess, code: int, what: str, needle: str | None = None) -> bool:
    if p.returncode != code or (needle and needle not in p.stderr + p.stdout):
        tail = (p.stderr.strip() or p.stdout.strip())[-400:]
        fail(f"{what}: expected exit {code}{f' mentioning {needle!r}' if needle else ''}, got {p.returncode}: {tail!r}")
        return False
    return True


def plan_items(p: subprocess.CompletedProcess, what: str) -> list | None:
    """The plan items of a `clean --json` document, or None (and a failure) if it isn't one."""
    try:
        return json.loads(p.stdout)["data"]["plan"]["items"]
    except (ValueError, KeyError, TypeError):
        fail(f"{what}: not a scull/clean-plan document: {p.stdout[:200]!r}")
        return None


def scan_args(tree: Tree) -> list[str]:
    return ["--root", str(tree.root), "--allow-network-roots", "--no-progress"]


def make_plan(scull: str, tree: Tree, rels: list[str], out: str = "plan.json") -> dict | None:
    args = ["clean", *scan_args(tree), "--json", "--output", out]
    for r in rels:
        args += ["--item", str(tree.root / r)]
    p = run(scull, tree, args)
    if not expect(p, EXIT_OK, f"plan {rels}"):
        return None
    items = plan_items(p, f"plan {rels}")
    if items is None:
        return None
    if len(items) != len(rels):
        fail(f"plan {rels}: {len(items)} item(s) planned")
        return None
    if any(i.get("tier") == "S3" for i in items):
        fail(f"plan {rels}: contains an S3 item (I-1)")
    return {"items": items}


def check_clean_gating(scull: str) -> None:
    tree = Tree()
    try:
        before = tree.snapshot()
        # 1. Without a terminal, `scull clean` only plans and reviews (CLI-03).
        p = run(scull, tree, ["clean", *scan_args(tree)])
        if expect(p, EXIT_OK, "scull clean without a terminal") and tree.snapshot() == before:
            ok("`scull clean` without a terminal only reviews; nothing changed")
        else:
            fail("`scull clean` without a terminal changed the tree")
        # 2. A protected project is never planned, not even by explicit --item (I-1, SAFE-03).
        p = run(scull, tree, ["clean", *scan_args(tree), "--json", "--item", str(tree.root / "vault" / "target")])
        planned: list = []
        if p.returncode == EXIT_OK:
            items = plan_items(p, "plan of a protected item")
            planned = ["unreadable plan"] if items is None else items
        if planned:
            fail("an item inside a .scull-protect project was planned")
        elif tree.snapshot() != before:
            fail("asking for a protected item changed the tree")
        else:
            ok(f"an item inside a .scull-protect project is never planned (exit {p.returncode})")
        # 3. Plan, then review it without --confirm: nothing runs.
        if make_plan(scull, tree, ["app/target", "web/node_modules"]) is None:
            return
        p = run(scull, tree, ["clean", "--plan", "plan.json", "--allow-network-roots"])
        if expect(p, EXIT_OK, "review of a plan without --confirm") and tree.snapshot() == before:
            ok("reviewing a plan without --confirm changes nothing")
        # 4. Option rules: --allow-permanent needs --confirm; --confirm needs a plan to confirm.
        for args in (["clean", "--plan", "plan.json", "--allow-permanent"], ["clean", "--confirm"]):
            p = run(scull, tree, args)
            if expect(p, EXIT_USAGE, f"`scull {' '.join(args)}` (usage error)"):
                ok(f"`scull {' '.join(args)}` -> exit 2")
        # 5. Elevated runs are refused without --allow-root (SAFE-14, FX-EXEC-12).
        p = run(scull, tree, ["clean", "--plan", "plan.json", "--confirm", "--allow-network-roots"],
                env=tree.env(SCULL_TEST_ASSUME_ELEVATED="1"))
        if expect(p, EXIT_SAFETY, "elevated `clean --plan --confirm`", "--allow-root") and tree.snapshot() == before:
            ok("elevated `clean --plan --confirm` -> exit 5 (--allow-root required); nothing changed")
        # 6. A tampered plan is refused (SAFE-11, FX-EXEC-07).
        plan_text = (tree.tmp / "plan.json").read_text()
        tampered = plan_text.replace(str(tree.root / "app" / "target"), str(tree.root / "notes"), 1)
        write(tree.tmp / "tampered.json", tampered)
        p = run(scull, tree, ["clean", "--plan", "tampered.json", "--confirm", "--allow-network-roots"])
        if expect(p, EXIT_PLAN_REJECTED, "tampered plan") and tree.snapshot() == before:
            ok("a tampered plan -> exit 4; nothing changed")
        # 7. A plan made on another machine (another scull home) is refused (FX-EXEC-09).
        other = tree.env(SCULL_HOME=str(tree.tmp / "home" / "other-scull-home"))
        p = run(scull, tree, ["clean", "--plan", "plan.json", "--confirm", "--allow-network-roots"], env=other)
        if expect(p, EXIT_PLAN_REJECTED, "foreign plan") and tree.snapshot() == before:
            ok("a plan from another machine -> exit 4; nothing changed")
        # 8. --confirm executes exactly the plan's items; the canary and everything else stay.
        p = run(scull, tree, ["clean", "--plan", "plan.json", "--confirm", "--allow-network-roots", "--json"])
        if expect(p, EXIT_OK, "confirmed plan"):
            after = tree.snapshot()
            removed = sorted(k for k in before if k not in after)
            outside = [k for k in removed if not (k.startswith("app/target") or k.startswith("web/node_modules"))]
            added = [k for k in after if k not in before]
            changed = [k for k in before if k in after and before[k] != after[k] and not k.endswith("/")]
            if outside or added or changed:
                fail(f"confirmed plan changed more than its items: removed {outside}, added {added}, changed {changed}")
            elif not removed:
                fail("confirmed plan removed nothing")
            else:
                ok(f"`clean --plan --confirm` removed only the 2 planned items ({len(removed)} entries); "
                   "sources, the protected project and notes intact")
        tree.canary_intact("confirmed plan")
        # 9. Plans are single-use (SAFE-11).
        p = run(scull, tree, ["clean", "--plan", "plan.json", "--confirm", "--allow-network-roots"])
        if expect(p, EXIT_PLAN_REJECTED, "replayed plan"):
            ok("replaying an executed plan -> exit 4")
        # 10. Not-undoable quarantine purge needs --allow-permanent (or the typed count) and is
        #     refused elevated (SAFE-07).
        held = [k for k in inspect_only.snapshot(tree.tmp / "quarantine")] if (tree.tmp / "quarantine").exists() else []
        purge = ["clean", "--purge-quarantine", "--include-unexpired"]
        p = run(scull, tree, purge)
        expect(p, EXIT_OK, "purge preview")
        p = run(scull, tree, [*purge, "--confirm"])
        if expect(p, EXIT_SAFETY, "purge without --allow-permanent", "--allow-permanent"):
            ok("`clean --purge-quarantine --confirm` without --allow-permanent -> exit 5")
        p = run(scull, tree, [*purge, "--confirm", "--allow-permanent"], env=tree.env(SCULL_TEST_ASSUME_ELEVATED="1"))
        if expect(p, EXIT_SAFETY, "elevated purge"):
            ok("elevated `clean --purge-quarantine --confirm --allow-permanent` -> exit 5")
        if held and (tree.tmp / "quarantine").exists() and not inspect_only.snapshot(tree.tmp / "quarantine"):
            fail("a refused purge emptied the quarantine")
        if shutil.which("script") and platform.system() == "Linux":
            check_typed_count(scull, tree, purge)
        tree.canary_intact("purge refusals")
    finally:
        tree.cleanup()


def check_typed_count(scull: str, tree: Tree, purge: list[str]) -> None:
    """On a terminal (script(1)), a not-undoable purge needs its count typed: a wrong count
    cancels (exit 130) and purges nothing."""
    q = tree.tmp / "quarantine"
    before = inspect_only.snapshot(q) if q.exists() else {}
    line = " ".join("'" + a.replace("'", "'\\''") + "'" for a in [scull, *purge])
    p = subprocess.run(["script", "-qec", line, "/dev/null"], cwd=tree.tmp, env=tree.env(),
                       input="y\n999\n", capture_output=True, text=True, timeout=180)
    after = inspect_only.snapshot(q) if q.exists() else {}
    if p.returncode == EXIT_CANCELLED and before == after and "to confirm" in p.stdout:
        ok("on a terminal, a wrong typed count cancels the purge (exit 130); nothing purged")
    else:
        fail(f"typed-count purge on a terminal: exit {p.returncode}, quarantine changed: {before != after}: {p.stdout[-300:]!r}")


def check_other_commands(scull: str) -> None:
    tree = Tree()
    try:
        before = tree.snapshot()
        cases = [
            (["data", "purge"], EXIT_USAGE, "`scull data purge` without a terminal"),
            (["approve", "plan_000000000000000000000000"], EXIT_SAFETY, "`scull approve` without a terminal"),
            (["restore", "op_000000000000000000000000"], EXIT_GENERAL, "`scull restore` of an unknown operation"),
            (["clean", "caches"], EXIT_OK, "`scull clean caches` (preview)"),
        ]
        for args, code, what in cases:
            p = run(scull, tree, args)
            good = p.returncode == code if code is not None else p.returncode != EXIT_OK
            if not good:
                fail(f"{what}: unexpected exit {p.returncode}: {(p.stderr or p.stdout).strip()[-300:]!r}")
            elif tree.snapshot() != before:
                fail(f"{what}: changed the tree")
            else:
                ok(f"{what} -> exit {p.returncode}; nothing changed")
        tree.canary_intact("other commands")
    finally:
        tree.cleanup()


def mcp_tools(scull: str, tree: Tree, args: list[str], env: dict[str, str]) -> tuple[list[dict], str]:
    """Start `scull mcp serve`, list its tools, stop it."""
    proc = subprocess.Popen([scull, "mcp", "serve", *args], cwd=tree.tmp, env=env, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    def send(msg: dict) -> None:
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()
    def recv(want_id: int) -> dict:
        while True:
            line = proc.stdout.readline()
            if not line:
                raise RuntimeError("mcp server closed stdout")
            msg = json.loads(line)
            if msg.get("id") == want_id:
                return msg
    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "gate", "version": "1"}}})
        recv(1)
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
        tools = recv(2)["result"]["tools"]
    finally:
        proc.stdin.close()
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        err = proc.stderr.read()
    return tools, err


def check_mcp(scull: str) -> None:
    tree = Tree()
    try:
        cfg_dir = tree.tmp / "home" / "scull-home" / "config"
        def config(destructive: bool) -> None:
            write(cfg_dir / "config.toml",
                  f'[scan]\nroots = ["{tree.root.as_posix()}"]\n\n[mcp]\nenabled = true\ndestructive = {str(destructive).lower()}\n')
        plan_tools = {"prepare_cleanup", "get_cleanup_plan", "get_approval_status", "execute_cleanup", "get_cleanup_status"}
        cases = [
            (False, ["--allow-destructive"], {}, False, "flag without config"),
            (True, [], {}, False, "config without flag"),
            (True, ["--allow-destructive"], {"SCULL_TEST_ASSUME_ELEVATED": "1"}, False, "config + flag, elevated"),
            (True, ["--allow-destructive"], {}, True, "config + flag"),
        ]
        for destructive, flag, extra, offered, what in cases:
            config(destructive)
            tools, err = mcp_tools(scull, tree, flag, tree.env(**extra))
            names = {t["name"] for t in tools}
            if offered != plan_tools.issubset(names) or (not offered and names & plan_tools):
                fail(f"MCP {what}: plan tools offered={bool(names & plan_tools)}, expected {offered}")
                continue
            if offered:
                ex = next(t for t in tools if t["name"] == "execute_cleanup")
                if ex.get("annotations", {}).get("destructiveHint") is not True:
                    fail("MCP execute_cleanup is not annotated destructiveHint: true")
                    continue
            for t in tools:
                if t["name"] not in plan_tools and t.get("annotations", {}).get("destructiveHint") is not False:
                    fail(f"MCP tool {t['name']} is not annotated destructiveHint: false")
            ok(f"MCP {what}: plan tools {'offered (execute_cleanup destructive, approval required)' if offered else 'not offered'}")
        tree.canary_intact("mcp")
    finally:
        tree.cleanup()


def check_binary(scull: str) -> None:
    scull = str(Path(scull).resolve())
    v = subprocess.run([scull, "--version"], capture_output=True, text=True)
    print(f"checking {v.stdout.strip()} ({platform.system()}) ({scull})")
    check_clean_gating(scull)
    check_other_commands(scull)
    check_mcp(scull)


def main(argv: list[str]) -> int:
    if len(argv) >= 2 and argv[1] == "deps":
        inspect_only.check_deps()
        return 0
    if len(argv) >= 2 and argv[1] == "fixtures":
        allow = "--allow-skips" in argv
        report = None
        if "--report" in argv:
            report = Path(argv[argv.index("--report") + 1])
        check_fixtures(allow, report)
    elif len(argv) == 3 and argv[1] == "binary":
        check_binary(argv[2])
    else:
        print(__doc__.split("Usage::")[1], file=sys.stderr)
        return 2
    if FAILURES:
        print(f"\nMVP gate FAILED: {len(FAILURES)} problem(s)", file=sys.stderr)
        return 1
    print("\nMVP gate passed")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
