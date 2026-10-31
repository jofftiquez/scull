# CI

Workflow: `.github/workflows/ci.yml`.

| Job | Runs on | What |
|---|---|---|
| `rustfmt` | Linux | `cargo fmt --all --check` |
| `test (<os>)` | macOS, Linux, Windows on PRs; Linux on pushes to `main` | `cargo clippy --workspace --all-targets -D warnings`, `cargo test --workspace`, `cargo doc -D warnings` (Linux) |
| `CI ok` | Linux | Passes only if every job above passed. **This is the single required status check.** |

- Docs/design/task-only changes skip CI (`paths-ignore`).
- Superseded runs on the same branch are cancelled.
- Commits that only add evidence images use `[skip ci]` in the commit message.
- Windows runners enable `core.longpaths` and disable `autocrlf` so fixtures see real bytes.

## Branch protection (maintainer)

Require the `CI ok` check and one review on `main`, squash merges, auto-delete branches
(HANDOVER.md §4.4). Branch protection on a **private** repository needs GitHub Pro/Team;
on a public repository it is free.
