# Contributing to scull

Thanks for helping. scull deletes files, so safety comes before everything else.

## Ground rules

1. **Safety before reclamation.** Any change that could make scull delete something it
   shouldn't needs a fixture first (`docs/FIXTURES.md`). When unsure, classify more protectively.
2. **Never delete real user files while developing.** Destructive tests run only inside generated
   fixture trees with canary directories. No test touches `$HOME` outside a temp dir.
3. **One engine.** Deletion lives only in `scull-cleaner`'s Executor. The desktop app and MCP
   server never mutate the filesystem directly.
4. **OS APIs only in `scull-platform`.** macOS, Linux and Windows are equal.
5. **No AGPL/GPL or other copyleft code**, in source or dependencies.

## Workflow

- Every change is tied to a GitHub issue. Branch name: `issue-<number>-<slug>`.
- PR title `[KEY] summary`; body starts with `Closes #<n>`.
- Before pushing: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- CI must be green on macOS, Linux and Windows.
- Issues close only with an `## Evidence` comment. Screenshots are attached to the
  comment with `gh issue comment --attach` (`scripts/evidence/post.py`); never commit them.

## Licensing

Contributions are dual licensed under MIT OR Apache-2.0, as described in `README.md`.
