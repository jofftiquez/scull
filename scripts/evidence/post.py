#!/usr/bin/env python3
"""Post an `## Evidence` comment on an issue and add `evidence-posted`.

    python3 scripts/evidence/post.py <issue> [--dir .evidence/<issue>] [--ci <run-url>] \
        [--pr <n>] [--sha <commit>] [--note "text"] [--criteria "criterion"]...

Screenshots are **attached** to the comment with `gh issue comment --attach` (GitHub-hosted
user-attachments). They are never committed to the repository. Images are read from
`.evidence/<issue>/` (gitignored; `term.sh` and `web.sh` write there). Text captures (`*.txt`)
next to the images are inlined in collapsible blocks.
"""
import argparse, glob, os, subprocess, sys, tempfile

ROOT = subprocess.run(["git", "rev-parse", "--show-toplevel"], text=True, capture_output=True).stdout.strip()
MAX_TXT = 6000


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("issue", type=int)
    ap.add_argument("--dir", help="folder with *.png/*.gif (default .evidence/<issue>)")
    ap.add_argument("--sha", help="commit the evidence refers to (optional)")
    ap.add_argument("--ci", action="append", default=[])
    ap.add_argument("--pr", type=int)
    ap.add_argument("--note", action="append", default=[])
    ap.add_argument("--criteria", action="append", default=[])
    ap.add_argument("--dry-run", action="store_true")
    a = ap.parse_args()
    d = os.path.abspath(a.dir or os.path.join(ROOT, ".evidence", str(a.issue)))
    imgs = sorted(glob.glob(os.path.join(d, "*.png")) + glob.glob(os.path.join(d, "*.gif")))
    if not imgs:
        sys.exit(f"no images in {d}")
    if len(imgs) > 50:
        sys.exit("gh allows at most 50 attachments per comment")
    lines = ["## Evidence", ""]
    meta = []
    if a.pr:
        meta.append(f"PR: #{a.pr}")
    if a.sha:
        meta.append(f"commit `{a.sha[:7]}`")
    if meta:
        lines.append(" · ".join(meta))
    for c in a.ci:
        lines.append(f"CI (macOS · Linux · Windows): {c}")
    lines.append("")
    if a.criteria:
        lines.append("**Acceptance criteria**")
        lines += [f"- [x] {c}" for c in a.criteria]
        lines.append("")
    for n in a.note:
        lines += [n, ""]
    attach = []
    for p in imgs:
        name = os.path.splitext(os.path.basename(p))[0]
        base = os.path.basename(p)
        lines.append(f"**{name}**")
        lines.append(f"![{name}](./{base})")
        txt = os.path.splitext(p)[0] + ".txt"
        if os.path.exists(txt):
            body = open(txt, encoding="utf-8", errors="replace").read()
            if len(body) > MAX_TXT:
                body = body[:MAX_TXT] + "\n… (truncated)"
            lines += ["<details><summary>text capture</summary>", "", "```text", body.rstrip("\n").replace("```", "ˋˋˋ"), "```", "</details>"]
        lines.append("")
        attach += ["--attach", f"./{base}#{name}"]
    body = "\n".join(lines)
    if a.dry_run:
        print(body)
        return
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
        f.write(body)
    repo = subprocess.run(["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"],
                          text=True, capture_output=True, cwd=ROOT).stdout.strip()
    # Run from the image folder so `./name.png` references are rewritten to the uploaded assets.
    r = subprocess.run(["gh", "issue", "comment", str(a.issue), "-R", repo, "--body-file", f.name, *attach],
                       text=True, capture_output=True, cwd=d)
    os.unlink(f.name)
    if r.returncode != 0:
        sys.exit(r.stderr)
    subprocess.run(["gh", "issue", "edit", str(a.issue), "--add-label", "evidence-posted"], cwd=ROOT,
                   capture_output=True)
    print(r.stdout.strip())


if __name__ == "__main__":
    main()
