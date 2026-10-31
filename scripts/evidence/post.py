#!/usr/bin/env python3
"""Post an `## Evidence` comment on an issue (HANDOVER.md §6) and add `evidence-posted`.

    python3 scripts/evidence/post.py <issue> --sha <commit-sha> [--ci <run-url>] [--pr <n>] \
        [--note "text"] [--criteria "criterion 1" --criteria "criterion 2"]

Embeds every PNG/GIF under docs/evidence/<issue>/ (pushed at <commit-sha>) and links the
.txt captures next to them. Run after the evidence commit is pushed.
"""
import argparse, glob, json, os, subprocess, sys, tempfile

ROOT = subprocess.run(["git", "rev-parse", "--show-toplevel"], text=True, capture_output=True).stdout.strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("issue", type=int)
    ap.add_argument("--sha", required=True)
    ap.add_argument("--ci", action="append", default=[])
    ap.add_argument("--pr", type=int)
    ap.add_argument("--note", action="append", default=[])
    ap.add_argument("--criteria", action="append", default=[])
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    repo = json.loads(subprocess.run(["gh", "repo", "view", "--json", "nameWithOwner"], text=True,
                                     capture_output=True, cwd=ROOT).stdout)["nameWithOwner"]
    d = os.path.join(ROOT, "docs", "evidence", str(a.issue))
    imgs = sorted(glob.glob(os.path.join(d, "*.png")) + glob.glob(os.path.join(d, "*.gif")))
    if not imgs:
        sys.exit(f"no images in docs/evidence/{a.issue}/")
    lines = ["## Evidence", ""]
    if a.pr:
        lines.append(f"PR: #{a.pr} · commit `{a.sha[:7]}`")
    for c in a.ci:
        lines.append(f"CI (macOS · Linux · Windows): {c}")
    lines.append("")
    if a.criteria:
        lines.append("**Acceptance criteria**")
        lines += [f"- [x] {c}" for c in a.criteria]
        lines.append("")
    for n in a.note:
        lines += [n, ""]
    for p in imgs:
        rel = os.path.relpath(p, ROOT).replace(os.sep, "/")
        name = os.path.splitext(os.path.basename(p))[0]
        lines.append(f"**{name}**")
        lines.append(f"![{name}](https://github.com/{repo}/blob/{a.sha}/{rel}?raw=true)")
        txt = os.path.splitext(p)[0] + ".txt"
        if os.path.exists(txt):
            lines.append(f"<sub>[text capture](https://github.com/{repo}/blob/{a.sha}/{os.path.relpath(txt, ROOT).replace(os.sep, '/')})</sub>")
        lines.append("")
    body = "\n".join(lines)
    if a.dry_run:
        print(body)
        return
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
        f.write(body)
    url = subprocess.run(["gh", "issue", "comment", str(a.issue), "--body-file", f.name], text=True,
                         capture_output=True, cwd=ROOT).stdout.strip()
    os.unlink(f.name)
    subprocess.run(["gh", "issue", "edit", str(a.issue), "--add-label", "evidence-posted"], cwd=ROOT,
                   capture_output=True)
    print(url)


if __name__ == "__main__":
    main()
