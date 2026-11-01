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

## Contributing a detector rule

Detector rules are data-only TOML files (`rules/`, format in `rules/README.md`). Start with
[docs/RULE_AUTHORING.md](docs/RULE_AUTHORING.md):

1. Open a **New detector rule** issue (template) with the tool, the folder, what proves it
   (the project file beside it, what the tool writes inside), how it is regenerated, and
   where people keep their own files near it.
2. Write the rule as a community rule first, with a cases file beside it, and make
   `scull rules validate FILE` and `scull rules test FILE` pass. The loader refuses community
   rules that could claim people's work (`user_work_folder`), match almost anything
   (`wildcard_name`) or apply outside a project (`no_project_marker`), and caps them at S1.
3. For the bundled set, add it to `rules/ecosystems/<ecosystem>.toml` (and
   `BUNDLED_RULE_FILES`) with fixture tests on temp trees: a positive case, a negative case,
   every exclusion, and the tier, with the canary and invariant checks (`docs/FIXTURES.md`).

**Review checklist** (reviewers check each; the pull request template repeats it):

- [ ] A marker beside or above the folder proves the project; the name alone never does.
- [ ] The folder never holds people's own work, settings or secrets, on any OS.
- [ ] The tier is the least that's true: S0 only when regeneration is local, quick and needs
      no network or credentials; S1 when it is long or needs the network; S2 when unsure.
- [ ] Lockfile-aware where the tool has one (`lockfiles.any`): no lockfile means at least S2.
- [ ] No command, script or hook anywhere; a cleanup command (bundled only) is chosen in Rust.
- [ ] Look-alike folders of other tools are excluded and tested (`exclude.markers`,
      `exclude.within`).
- [ ] Fixture tests (or, for a community rule, a cases file) cover positive, negative and
      every exclusion, and pass on macOS, Linux and Windows where the rule applies.

## Licensing

Contributions are dual licensed under MIT OR Apache-2.0, as described in `README.md`.
