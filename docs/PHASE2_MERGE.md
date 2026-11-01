# Merging `phase2` into `main`

Decision D-33 keeps every Phase 2 change (the Executor, quarantine, restore, journal, audit,
Trash, Quick Clean, `scull clean*`, `scull history/restore/data`, the MCP approval broker) on the
`phase2` branch until it is safe to ship. Merging `phase2` into `main` ships the Executor: from
then on `main` produces binaries that can delete. This is the checklist for that merge. Tick
every box, with a link to its evidence, in the merge PR's description. Only the maintainer
merges.

## 1. The branch is green on every OS

- [ ] **3-OS CI on the `phase2` head.** Pushes to `phase2` run macOS, Linux and Windows
      (pull requests run Linux only). Start one by hand if needed:
      `gh workflow run ci.yml --ref phase2`, then `gh run watch`.
- [ ] **#53 is closed** (SAFE-09 race, macOS paths, Windows FX-WIN-03 hang), via PR #54 and any
      follow-ups. Until #54 lands, `fx_out_01`'s SAFE-09 race is a known flake on `phase2`.
- [ ] **Nightly fixtures on `phase2`**, run by hand:
      `gh workflow run nightly-fixtures.yml --ref phase2` and
      `gh workflow run nightly-os-fixtures.yml --ref phase2`.
- [ ] **Registry up to date.** The Phase 2 fixtures marked "pending CI" on macOS and Windows
      (`PHASE2_PENDING`, `WINDOWS_PENDING`, `MACOS_PENDING` in
      `crates/scull-fixtures/src/registry.rs`) have run green on those OSs, and the markers
      are removed.
- [ ] **No `ci-pending` issues left from Phase 2:**
      `gh issue list -R jofftiquez/scull-archive --label ci-pending` lists none whose code is
      on `phase2`.

## 2. Independent review

- [ ] **#14, the independent safety review of the Executor** (PRD §21.3 MVP gate). Someone
      who did not write the Executor and rubric reviews them: the maintainer, or a reviewer
      the maintainer names. Every finding is fixed or explicitly accepted on the issue.
- [ ] The provisional overnight decisions are confirmed or changed by the maintainer: D-34
      (changed items: CLI/desktop skip, MCP voids), D-35 (MPL exceptions only in the desktop's
      deny.toml) and D-36 (orphaned caches deleted at S1; decision issue #30).

## 3. MVP release gates (#66)

`release-gates.yml` picks the gate from the tag: alpha tags run the inspect-only gate, every
other tag runs the MVP gates. On `phase2` a manual run with the default `auto` choice runs
the MVP gates.

- [ ] **Dry run:** Actions → Release gates → Run workflow, branch `phase2`, gate `mvp` (or
      `auto`). Every job is green:
  - `MVP gate (dependencies and release binary)`:
    - `check_mvp_gates.py deps`: no Tauri or webview crates in the CLI.
    - `check_mvp_gates.py binary target/release/scull`: the destructive commands are present
      and gated as designed.
  - `MVP fixture gate` on `ubuntu-latest`, `macos-latest` and `windows-latest`:
    - every MVP fixture for that OS passes with zero invariant violations;
    - none is still `Tracked` or `Deferred`;
    - each OS's report is kept as a workflow artifact.
- [ ] The **Tracked MVP fixtures are finished.** List what is still open with
      `python3 packaging/check_mvp_gates.py fixtures`; the gate names each one with its issue.
      When this checklist was written, 22 MVP fixtures were Tracked: FX-FS-01/03/05/07/08/12,
      FX-RACE-06/07, FX-EXEC-01..06/11/12/14/16, FX-PHY-06, FX-QC-05, FX-OUT-02/03. Several
      wait on Windows quarantine and restore, and FX-PHY-06 on the G3 corpus.
- [ ] The **manual checklist** (docs/FIXTURES.md §2.2) is run on a release build and signed for
      every MVP Manual fixture (today FX-MAC-01).
- [ ] **G3:** predicted vs. measured reclaim is within target on the reference machines.
- [ ] **0 open issues with severity "data loss" or "safety bypass"** (check `safety-critical`
      and `bug`).

## 4. Bring `phase2` up to date with `main`, then merge

1. Merge `main` into `phase2` in a PR (`sync-main-into-phase2-N`, as before), resolve the
   conflicts, and get CI green. Conflicts to expect:
   - `.github/workflows/release-gates.yml`: keep `phase2`'s gate selection (`select`,
     `inspect-only`, `mvp-binary`, `mvp-fixtures`) and every dry-run addition from `main`.
     #56's additions (packaging self-tests, SBOMs, global artifacts, signing readiness) are
     already on `phase2`, synced in #63.
   - `RELEASING.md`, `CHANGELOG.md`, `dist-workspace.toml`, `docs/CI.md`.
   - Regenerated files: re-run the generators rather than hand-merging. That means
     `dist generate` (release.yml), the schema tests with `SCULL_BLESS=1`, and the NDJSON line
     schemas.
2. Re-run the dry run from section 3 on the synced `phase2`.
3. Open the PR `phase2` → `main` and **merge with a merge commit** (not squash), so the Phase 2
   history and the per-issue commits stay traceable.
4. Right after the merge, on `main`:
   - [ ] CI green on all three OSs (`gh workflow run ci.yml --ref main`).
   - [ ] A Release gates dry run on `main` with gate `mvp` is green.
   - [ ] `RELEASING.md` and `CHANGELOG.md` no longer describe the build as inspect-only. The
         alpha section stays as history.
   - [ ] `packaging/check_inspect_only.py` stays for alpha tags only. Nothing tags an alpha
         from `main` after the merge, so it can be retired at the first MVP release.

## 5. Issues this closes or unblocks

| Issue | What happens at the merge |
|---|---|
| #66 MVP release gates | Closes when this PR's evidence is posted (the gates exist; passing them is section 3). |
| #53 phase2 3-OS CI | Must already be closed (section 1). |
| #14 independent Executor review | Must already be closed by the maintainer (section 2). |
| #30 orphaned-cache decision (D-36) | Must already be decided (section 2). |
| #15 MVP release | Unblocked: the first non-alpha tag after the merge runs the MVP gates and the signing pipeline. |
| `ci-pending` Phase 2 issues (scull-archive) | Close as their 3-OS runs go green (section 1). |
| Phase 4 MCP issues already on `phase2` (#32, #33, #34, #35, #65) | Ship with the merge. They close individually with their own evidence; the merge does not close them. |
