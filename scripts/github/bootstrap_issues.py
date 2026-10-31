#!/usr/bin/env python3
"""Create scull's labels, milestones, issues and roadmap tracker on GitHub.

Idempotent: safe to re-run. Uses the GitHub CLI (`gh`), which must be authenticated
(`gh auth status`). Run from the repository root AFTER the bootstrap commit
(docs/, design/, tasks/) has been pushed, because evidence images are linked by
commit SHA.

    python3 scripts/github/bootstrap_issues.py --dry-run   # show what would happen
    python3 scripts/github/bootstrap_issues.py             # do it

Writes tasks/issue-map.json (task key -> issue number).
"""
import argparse, json, os, re, subprocess, sys, tempfile, time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
TASKS = os.path.join(ROOT, "tasks")
TRACKER_TITLE = "Roadmap tracker (source of truth)"


def sh(args, check=True, capture=True):
    r = subprocess.run(args, cwd=ROOT, text=True, capture_output=capture)
    if check and r.returncode != 0:
        sys.exit(f"command failed: {' '.join(args)}\n{r.stderr}")
    return r.stdout.strip() if capture else ""


def gh_json(args):
    out = sh(["gh"] + args)
    return json.loads(out) if out else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--skip-evidence", action="store_true", help="create issues but don't close done ones")
    a = ap.parse_args()
    dry = a.dry_run

    issues = json.load(open(os.path.join(TASKS, "issues.json")))
    milestones = json.load(open(os.path.join(TASKS, "milestones.json")))
    labels = json.load(open(os.path.join(TASKS, "labels.json")))

    if dry:
        repo = "OWNER/REPO"
        sha = "<HEAD-SHA>"
    else:
        sh(["gh", "auth", "status"])
        repo = gh_json(["repo", "view", "--json", "nameWithOwner"])["nameWithOwner"]
        sha = sh(["git", "rev-parse", "HEAD"])
        # evidence images must exist in the pushed commit
        remote_has = subprocess.run(["git", "branch", "-r", "--contains", sha], cwd=ROOT, text=True, capture_output=True).stdout.strip()
        if not remote_has:
            sys.exit("HEAD is not pushed. Push the bootstrap commit first so evidence image links resolve.")
    print(f"repo: {repo}  sha: {sha}")

    # 1. labels
    for name, color in labels.items():
        print(f"label  {name}")
        if not dry:
            sh(["gh", "label", "create", name, "--color", color, "--force"])

    # 2. milestones
    existing_ms = {} if dry else {m["title"]: m["number"] for m in gh_json(["api", f"repos/{repo}/milestones?state=all&per_page=100"])}
    for m in milestones:
        if m["title"] in existing_ms:
            print(f"milestone exists  {m['title']}")
            continue
        print(f"milestone create  {m['title']}")
        if not dry:
            r = gh_json(["api", f"repos/{repo}/milestones", "-f", f"title={m['title']}", "-f", f"description={m['description']}"])
            existing_ms[m["title"]] = r["number"]

    # 3. issues (create missing; match by key marker in title)
    existing = {} if dry else {}
    if not dry:
        for it in gh_json(["issue", "list", "--state", "all", "--limit", "1000", "--json", "number,title"]):
            mk = re.match(r"\[([A-Z0-9-]+)\]", it["title"])
            if mk:
                existing[mk.group(1)] = it["number"]
    keymap = dict(existing)
    for t in issues:
        if t["key"] in keymap:
            print(f"issue exists  #{keymap[t['key']]} {t['title']}")
            continue
        print(f"issue create  {t['title']}")
        if dry:
            keymap[t["key"]] = f"#{t['key']}"
            continue
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
            f.write(t["body"])
            path = f.name
        args = ["gh", "issue", "create", "--title", t["title"], "--body-file", path, "--milestone", t["milestone"]]
        for l in t["labels"]:
            args += ["--label", l]
        url = sh(args)
        keymap[t["key"]] = int(url.rstrip("/").split("/")[-1])
        os.unlink(path)
        time.sleep(0.4)  # stay friendly with secondary rate limits

    # 4. second pass: resolve dependency placeholders to issue numbers
    for t in issues:
        if not t["deps"] or dry:
            continue
        num = keymap[t["key"]]
        cur = gh_json(["issue", "view", str(num), "--json", "body"])["body"]
        if "{{dep:" not in cur:
            continue
        new = re.sub(r"\{\{dep:([A-Z0-9-]+)\}\}", lambda m: f"#{keymap[m.group(1)]} ({m.group(1)})", cur)
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
            f.write(new)
            path = f.name
        sh(["gh", "issue", "edit", str(num), "--body-file", path])
        os.unlink(path)
        print(f"deps linked  #{num}")

    # 5. evidence for already-done issues, then close
    if not a.skip_evidence:
        for t in [x for x in issues if x["done"]]:
            num = keymap[t["key"]]
            imgs = "\n\n".join(f"![{os.path.basename(p)}](https://github.com/{repo}/blob/{sha}/{p}?raw=true)" for p in t["images"])
            files = "\n".join(f"- [`{p}`](https://github.com/{repo}/blob/{sha}/{p})" for p in t["images"])
            comment = f"## Evidence\n\nCompleted during the planning session (Oct 31, 2026) and committed in {sha[:7]}.\n\n{imgs}\n\n**Files**\n{files}\n"
            print(f"evidence + close  #{num} {t['key']}")
            if dry:
                continue
            state = gh_json(["issue", "view", str(num), "--json", "state"])["state"]
            if state == "CLOSED":
                continue
            with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
                f.write(comment)
                path = f.name
            sh(["gh", "issue", "comment", str(num), "--body-file", path])
            os.unlink(path)
            sh(["gh", "issue", "edit", str(num), "--add-label", "evidence-posted"])
            sh(["gh", "issue", "close", str(num), "--reason", "completed"])
            time.sleep(0.4)

    # 6. roadmap tracker issue with task lists per milestone
    lines = [
        "This issue is the **single checklist** for scull. Each line is a GitHub issue; it ticks itself when the issue closes.",
        "Rules: an issue closes only with an **Evidence** comment (see HANDOVER.md). Keys like `P1-CORE-02` are stable IDs.",
        "",
    ]
    for m in milestones:
        lines.append(f"### {m['title']}")
        lines.append(f"_{m['description']}_")
        for t in [x for x in issues if x["milestone"] == m["title"]]:
            ref = keymap[t["key"]]
            lines.append(f"- [{'x' if t['done'] else ' '}] {ref if isinstance(ref, str) else '#' + str(ref)}")
        lines.append("")
    tracker = "\n".join(lines)
    if dry:
        print(f"tracker  {TRACKER_TITLE} ({len(issues)} entries)")
    else:
        found = [i for i in gh_json(["issue", "list", "--state", "all", "--search", f'"{TRACKER_TITLE}" in:title', "--json", "number,title"]) if i["title"] == TRACKER_TITLE]
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
            f.write(tracker)
            path = f.name
        if found:
            sh(["gh", "issue", "edit", str(found[0]["number"]), "--body-file", path])
            num = found[0]["number"]
        else:
            num = int(sh(["gh", "issue", "create", "--title", TRACKER_TITLE, "--body-file", path, "--label", "type:chore"]).rstrip("/").split("/")[-1])
        os.unlink(path)
        subprocess.run(["gh", "issue", "pin", str(num)], cwd=ROOT, capture_output=True)
        print(f"tracker  #{num} (pinned)")

    if not dry:
        json.dump(keymap, open(os.path.join(TASKS, "issue-map.json"), "w"), indent=2)
        print("wrote tasks/issue-map.json")
    open_count = sum(1 for t in issues if not t["done"])
    print(f"\nsummary: {len(milestones)} milestones, {len(issues)} issues ({len(issues) - open_count} closed with evidence, {open_count} open)")


if __name__ == "__main__":
    main()
