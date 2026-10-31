# scull — agent instructions

Start with `HANDOVER.md` and follow it. In short:

- Requirements: `docs/PRD.md` · Decisions: `docs/DECISIONS.md` · Safety: `docs/THREAT_MODEL.md`, `docs/FIXTURES.md` · Design: `design/html/index.html`, `design/tokens.json`.
- First session only: run the setup in `HANDOVER.md` §4 (bootstrap commit, then `python3 scripts/github/bootstrap_issues.py`).
- GitHub issues are the source of truth. Work one issue per branch/worktree; close only with an **Evidence** comment (HANDOVER §6).
- Use parallel subagents, one per lane, per HANDOVER §7.
- Never delete real user files in development or tests; deletion lives only in `scull-cleaner`; OS APIs only in `scull-platform`; no AGPL code.
