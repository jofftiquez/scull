#!/usr/bin/env python3
"""Platform-boundary check (P1-CI-03, PLT-11).

Fails if OS-specific APIs are used outside `crates/scull-platform/`:
  * Rust source: `std::os::unix`, `std::os::windows`, `std::os::macos`/`linux`/`darwin`,
    `libc::`, `windows_sys`, `windows::` (the `windows` crate), `nix::`, `rustix::`,
    Apple framework crates (`core_foundation`, `objc`, `objc2`, `cocoa`, `security_framework`,
    `io_kit`, `fsevent`), `extern "C"` / `extern "system"` blocks.
  * Cargo manifests: `[dependencies]`/`[target.*.dependencies]` on those crates.

Also fails (#91) if `Platform::entry_metadata` is called outside `crates/scull-scanner/` (and
`crates/scull-platform/`): on macOS it may return what the directory listing saw, a snapshot
that can describe an entry already removed or replaced. It feeds the scan only; revalidation,
the executor, quarantine/restore and purges must use fresh no-follow checks
(`Platform::metadata`, handle-based checks), never listing-derived metadata.

Exempt:
  * `crates/scull-platform/` (the one place OS code lives);
  * `crates/scull-fixtures/` (test-only fixture builders create OS-specific trees: symlinks,
    junctions; never linked into a shipped binary; see docs/FIXTURES.md §2);
  * test code: files under a `tests/` or `benches/` directory, and the item (module, fn, use)
    directly following a `#[cfg(test)]` attribute (found by brace counting);
  * `[dev-dependencies]` sections.

Usage: python3 scripts/ci/platform_boundary.py [--root DIR]
Exit 0 = clean, 1 = violations (printed as file:line: message).
"""
import argparse, os, re, sys

API = re.compile(
    r"\bstd::os::(unix|windows|macos|linux|darwin|wasi)\b"
    r"|\blibc::|\bwindows_sys\b|\bwindows::|\bnix::|\brustix::"
    r"|\b(core_foundation|core_foundation_sys|objc2?|cocoa|security_framework|io_kit_sys|fsevent(_sys)?)::"
    r"|\bextern\s+\"(C|system)\"\s*\{"
)
OS_CRATES = {
    "libc", "windows-sys", "windows", "nix", "rustix", "core-foundation", "core-foundation-sys",
    "objc", "objc2", "cocoa", "security-framework", "io-kit-sys", "fsevent", "fsevent-sys",
    "winapi", "ntapi",
}
EXEMPT_DIRS = ("crates/scull-platform/", "crates/scull-fixtures/")
# Listing-derived metadata (`entry_metadata`) is for the scan only (#91).
ENTRY_MD = re.compile(r"(\.|::)\s*entry_metadata\s*\(")
SCAN_ONLY_ALLOWED = ("crates/scull-platform/", "crates/scull-scanner/")
SKIP_DIRS = {"target", "node_modules", ".git", "docs", "design"}


def rel(root, p):
    return os.path.relpath(p, root).replace(os.sep, "/")


def is_test_path(r):
    parts = r.split("/")
    return "tests" in parts[:-1] or "benches" in parts[:-1]


def strip_comment(line):
    i = line.find("//")
    return line if i < 0 else line[:i]


def check_rust(root, path, out):
    r = rel(root, path)
    check_api = not r.startswith(EXEMPT_DIRS)
    check_entry_md = not r.startswith(SCAN_ONLY_ALLOWED)
    if is_test_path(r) or not (check_api or check_entry_md):
        return
    # Skip exactly the item that follows a `#[cfg(test)]` attribute (a test module, fn or use),
    # by counting braces until it closes (or until `;` for brace-less items).
    pending = False   # saw #[cfg(test)], waiting for the item to start
    depth = 0         # brace depth inside a skipped test item (0 = not skipping)
    with open(path, encoding="utf-8", errors="replace") as f:
        for n, line in enumerate(f, 1):
            code = strip_comment(line)
            s = code.strip()
            if depth == 0 and not pending and s.startswith("#[cfg(test)]"):
                pending = True
                s = s[len("#[cfg(test)]"):].strip()
                code = s
                if not s:
                    continue
            if pending:
                if s.startswith("#[") or not s:
                    continue  # further attributes / blank lines before the item
                opens, closes = code.count("{"), code.count("}")
                if opens == 0:
                    if s.endswith(";"):
                        pending = False  # e.g. `use std::os::unix::...;`
                    continue
                depth = opens - closes
                pending = False
                continue
            if depth > 0:
                depth += code.count("{") - code.count("}")
                continue
            m = API.search(code) if check_api else None
            if m:
                out.append(f"{r}:{n}: OS-specific API `{m.group(0).strip()}` outside scull-platform (PLT-11)")
            if check_entry_md and ENTRY_MD.search(code):
                out.append(
                    f"{r}:{n}: `entry_metadata` outside scull-scanner: it may be a listing snapshot; "
                    "use a fresh no-follow check (`metadata`) here (#91)"
                )


def check_manifest(root, path, out):
    r = rel(root, path)
    if r.startswith(EXEMPT_DIRS):
        return
    section = ""
    with open(path, encoding="utf-8") as f:
        for n, line in enumerate(f, 1):
            s = line.strip()
            if s.startswith("["):
                section = s.strip("[]").strip()
                continue
            in_deps = section.endswith("dependencies") and "dev-dependencies" not in section
            if not in_deps or not s or s.startswith("#"):
                continue
            name = re.split(r"[\s.=]", s, 1)[0].strip('"')
            pkg = re.search(r'package\s*=\s*"([^"]+)"', s)
            for cand in {name, pkg.group(1) if pkg else name}:
                if cand in OS_CRATES and not (r == "Cargo.toml" and section == "workspace.dependencies"):
                    out.append(f"{r}:{n}: dependency on OS crate `{cand}` outside scull-platform (PLT-11)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..")))
    a = ap.parse_args()
    root = os.path.abspath(a.root)
    out, scanned = [], 0
    for dp, dns, fns in os.walk(root):
        dns[:] = [d for d in dns if d not in SKIP_DIRS and not d.startswith(".")]
        for fn in fns:
            p = os.path.join(dp, fn)
            if fn.endswith(".rs"):
                scanned += 1
                check_rust(root, p, out)
            elif fn == "Cargo.toml":
                scanned += 1
                check_manifest(root, p, out)
    for v in out:
        print(v)
    print(f"platform-boundary: scanned {scanned} files, {len(out)} violation(s)")
    return 1 if out else 0


if __name__ == "__main__":
    sys.exit(main())
