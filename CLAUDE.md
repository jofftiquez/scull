# scull — agent instructions

- If `CLAUDE.local.md` exists, read it first: it points to the maintainers' private planning docs
  (requirements, decision log, threat model, design screenshots), which are kept outside git.
- Public references: `docs/ARCHITECTURE.md`, `docs/FIXTURES.md`, `docs/CI.md`, `design/html/index.html`,
  `design/tokens.json`, `CONTRIBUTING.md`, `SECURITY.md`.
- GitHub issues are the source of truth. Work one issue per branch/worktree; close an issue only with an
  `## Evidence` comment (screenshots attached with `gh issue comment --attach`, never committed).
- Never delete real user files in development or tests; destructive tests run only in fixture temp trees
  with canaries. Deletion lives only in `scull-cleaner`; OS APIs only in `scull-platform`; no AGPL or
  other copyleft code.
- Never commit private planning docs, screenshots or `local/` to this repository.
