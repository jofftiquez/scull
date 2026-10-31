//! FX-GIT end-to-end fixtures (docs/FIXTURES.md §3.2, P1-FX-03): each builds real
//! repositories and worktrees with `git` in a fixture, then runs the worktree pipeline
//! (`common::worktrees`): the real scanner finds the repositories under the scan root,
//! `scull-git` discovers and analyses each worktree with its hardened runner (never fetching),
//! `scull-safety` supplies the in-use signal, and the GIT-03 table classifies. Every fixture
//! runs twice (scan, revalidation) and checks I-1, I-2, I-3 and I-8.
//!
//! The per-layer versions (analysis, decision table, hardened runner) live in `scull-git`'s
//! tests; the registry (`scull_fixtures::registry`) lists both.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::worktrees::{GitWorld, WorktreePipeline, WtItem, check_wt, run_wt_checked, slashed};
use common::{InUseSource, Pipeline};
use scull_core::{Action, ReasonCode, Tier};
use scull_git::ArtifactCleanup;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::Duration;

fn wt_run(w: &GitWorld) -> common::worktrees::WtRun {
    run_wt_checked(&w.fx, &WorktreePipeline::new(w.fx.root()))
}

fn assert_reason(item: &WtItem, code: ReasonCode, tier: Tier) {
    assert!(
        item.has_reason(code, tier),
        "expected {code:?} at {tier}: {:#?}",
        item.decision
    );
}

/// The worktree is never in the plan.
fn assert_not_planned(run: &common::worktrees::WtRun, item: &WtItem) {
    assert!(
        !run.plan().iter().any(|c| c.id == item.candidate.id),
        "{} must not be planned",
        item.state.path
    );
}

// ---------------------------------------------------------------------------------------------

/// FX-GIT-01: clean, unlocked worktree; branch merged into default and pushed → whole
/// worktree S1 (never S0); branch preserved after removal.
#[test]
fn fx_git_01_clean_merged_pushed_worktree_is_s1_branch_kept() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "feature.txt", "f\n", "feature work");
    w.git(&["merge", "--quiet", "--ff-only", "feature"]);
    w.git(&["push", "--quiet", "origin", "main"]);
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S1, "{:#?}", wt.decision);
    assert_reason(wt, ReasonCode::WorktreeClean, Tier::S1);
    assert_eq!(wt.candidate.action, Action::GitWorktreeRemove);
    assert!(wt.candidate.sizes.logical > 0, "sized by the scan");
    let regen = wt.candidate.regeneration.instructions.as_deref().unwrap();
    assert!(regen.contains("git worktree add") && regen.contains("feature"));
    // The main worktree and the bare remote are never removable.
    for other in run
        .items
        .iter()
        .filter(|i| i.candidate.id != wt.candidate.id)
    {
        assert_eq!(other.candidate.tier, Tier::S3, "{}", other.state.path);
    }
    assert_eq!(run.plan().len(), 1);
    assert!(!w.git(&["rev-parse", "--verify", "feature"]).is_empty());
}

/// FX-GIT-02: modified tracked file → S3; artifact-only cleanup offered.
#[test]
fn fx_git_02_modified_tracked_file_is_s3_artifacts_only() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/README.md", "changed\n").unwrap();
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::WorktreeDirty, Tier::S3);
    assert!(
        wt.decision
            .reasons
            .iter()
            .any(|r| r.message.contains("README.md"))
    );
    assert_eq!(wt.decision.artifact_cleanup, ArtifactCleanup::Allowed);
    assert_not_planned(&run, wt);
}

/// FX-GIT-03: untracked, non-ignored file → S3.
#[test]
fn fx_git_03_untracked_file_is_s3() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/notes.txt", "mine\n").unwrap();
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::WorktreeDirty, Tier::S3);
    assert!(
        wt.decision
            .reasons
            .iter()
            .any(|r| r.message.contains("notes.txt"))
    );
    assert_not_planned(&run, wt);
}

/// FX-GIT-04: ignored `.env` and `dev.sqlite` not matched by any artifact rule → ≥ S2; both
/// listed by name. The ignored `target/` is a recognised artifact and doesn't count.
#[test]
fn fx_git_04_unique_ignored_files_are_s2_and_listed_by_name() {
    let mut w = GitWorld::with_remote();
    w.fx.file("repo/.gitignore", ".env\n*.sqlite\ntarget/\n")
        .unwrap()
        .cargo_project("repo", "app")
        .unwrap();
    w.repo.commit_all("rust project").unwrap();
    w.git(&["push", "--quiet", "origin", "main"]);
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/.env", "SECRET=1\n")
        .unwrap()
        .file("wt/dev.sqlite", "db")
        .unwrap()
        .cargo_target("wt", "app", 512)
        .unwrap();
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S2, "{:#?}", wt.decision);
    assert_reason(wt, ReasonCode::WorktreeUniqueIgnored, Tier::S2);
    let mut unique = wt.decision.unique_ignored.clone();
    unique.sort();
    assert_eq!(unique, [".env", "dev.sqlite"]);
    assert_eq!(wt.decision.artifacts.len(), 1);
    assert_eq!(wt.decision.artifacts[0].rule, "rust.target");
    // S2: restorable quarantine only.
    assert_eq!(wt.candidate.action, Action::Quarantine);
}

/// FX-GIT-05: commits not pushed to any remote → S3.
#[test]
fn fx_git_05_unpushed_commits_are_s3() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "one");
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::UnpushedCommits, Tier::S3);
    assert_not_planned(&run, wt);
}

/// FX-GIT-06: branch squash-merged upstream (patch-equivalent), no forge lookup → S2, "merge
/// unproven".
#[test]
fn fx_git_06_squash_merged_branch_is_s2_merge_unproven() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "feature.txt", "feature\n", "feature work");
    w.fx.file("repo/other.txt", "other\n").unwrap();
    w.repo.commit_all("other work on main").unwrap();
    w.git(&["merge", "--quiet", "--squash", "feature"]);
    w.git(&["commit", "--quiet", "-m", "Squashed feature (#1)"]);
    w.git(&["push", "--quiet", "origin", "main"]);
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S2, "{:#?}", wt.decision);
    assert_reason(wt, ReasonCode::MergeUnproven, Tier::S2);
    assert!(
        wt.decision
            .reasons
            .iter()
            .any(|r| r.message.contains("merge unproven"))
    );
    assert_eq!(wt.candidate.action, Action::Quarantine);
}

/// FX-GIT-07: detached HEAD at a commit reachable from no ref → S3.
#[test]
fn fx_git_07_orphan_detached_head_is_s3() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", None, "main");
    w.commit_in("wt", "x.txt", "x\n", "detached work");
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::OrphanHead, Tier::S3);
    assert_not_planned(&run, wt);
}

/// FX-GIT-08: detached HEAD at a commit reachable from a pushed ref, clean → S1.
#[test]
fn fx_git_08_detached_head_on_pushed_commit_is_s1() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", None, "origin/main");
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S1, "{:#?}", wt.decision);
    assert_reason(wt, ReasonCode::WorktreeClean, Tier::S1);
    assert_eq!(wt.candidate.action, Action::GitWorktreeRemove);
}

/// FX-GIT-09: locked worktree → S3; lock reason shown.
#[test]
fn fx_git_09_locked_worktree_is_s3_with_reason() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", Some("feature"), "main");
    w.git(&[
        "worktree",
        "lock",
        "--reason",
        "agent run 42 in progress",
        "../wt",
    ]);
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::WorktreeLocked, Tier::S3);
    assert!(
        wt.decision
            .reasons
            .iter()
            .any(|r| r.message.contains("agent run 42 in progress"))
    );
    assert_not_planned(&run, wt);
}

/// FX-GIT-10: registered worktree whose folder was deleted → `git worktree prune` candidate,
/// S0 (metadata only).
#[test]
fn fx_git_10_registration_without_folder_is_s0_prune() {
    let mut w = GitWorld::with_remote();
    let dir = w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "unpushed");
    std::fs::remove_dir_all(&dir).expect("remove the worktree folder inside the fixture");
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let stale: Vec<&WtItem> = run.items.iter().filter(|i| i.state.missing).collect();
    assert_eq!(stale.len(), 1, "found through the repository's metadata");
    let wt = stale[0];
    assert_eq!(wt.candidate.tier, Tier::S0, "{:#?}", wt.decision);
    assert_reason(wt, ReasonCode::StaleWorktreeRegistration, Tier::S0);
    assert_eq!(wt.candidate.action, Action::GitWorktreePrune);
    assert_eq!(wt.candidate.path, wt.state.git_dir, "metadata only");
    assert_eq!(wt.candidate.sizes.logical, 0);
    assert!(run.plan().iter().any(|c| c.id == wt.candidate.id));
}

/// FX-GIT-11: no upstream configured; commits exist only locally → S3.
#[test]
fn fx_git_11_no_upstream_local_only_commits_is_s3() {
    let mut w = GitWorld::local();
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "local only");
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::UnpushedCommits, Tier::S3);
    assert_not_planned(&run, wt);
}

/// FX-GIT-12: branch pushed recently, but the local remote-tracking refs are stale →
/// classified protectively (S3/S2), never lower; no fetch performed.
#[test]
fn fx_git_12_stale_remote_refs_are_protective_without_fetch() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "pushed later");
    w.git_in("wt", &["push", "--quiet", "-u", "origin", "feature"]);
    let base = w.git(&["rev-parse", "main"]);
    w.git(&["update-ref", "refs/remotes/origin/feature", &base]);
    w.fx.checkpoint().unwrap();

    // I-3 (inside `run_wt_checked`) proves nothing was fetched: refs and FETCH_HEAD unchanged.
    let run = wt_run(&w);
    let wt = run.at("wt");
    assert!(wt.candidate.tier >= Tier::S2);
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::UnpushedCommits, Tier::S3);
    assert!(!w.fx.root().join("repo/.git/FETCH_HEAD").exists());
}

/// FX-GIT-13: submodule inside a worktree with local changes → S3.
#[test]
fn fx_git_13_dirty_submodule_is_s3() {
    let mut w = GitWorld::with_remote();
    let lib = w.fx.git_repo("sublib").unwrap();
    w.fx.file("sublib/lib.txt", "lib\n").unwrap();
    lib.commit_all("lib").unwrap();
    w.add_worktree("wt", Some("feature"), "main");
    let url = slashed(lib.path());
    w.git_in(
        "wt",
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
            &url,
            "sub",
        ],
    );
    w.git_in("wt", &["commit", "--quiet", "-m", "add submodule"]);
    w.git_in("wt", &["push", "--quiet", "origin", "feature"]);
    w.fx.file("wt/sub/lib.txt", "changed in submodule\n")
        .unwrap();
    w.fx.checkpoint().unwrap();

    let run = wt_run(&w);
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::WorktreeDirty, Tier::S3);
    assert!(
        wt.decision
            .reasons
            .iter()
            .any(|r| r.message.contains("submodule sub"))
    );
    assert_not_planned(&run, wt);
}

/// Kills and reaps a child on drop, so a failing assertion never leaves one behind.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// FX-GIT-14: a process has its working directory inside the worktree → S3 for this run,
/// "in use", through the real OS probe (`NativeActivityProbe`).
#[test]
fn fx_git_14_process_cwd_in_worktree_is_s3_in_use() {
    let w = GitWorld::with_remote();
    let dir = w.add_worktree("wt", None, "origin/main");
    let mut fx = w.fx;
    fx.checkpoint().unwrap();

    // A real process (a `git` waiting on stdin) whose working directory is the worktree.
    let child = ChildGuard(
        Command::new("git")
            .args(["cat-file", "--batch"])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child"),
    );
    let pipeline = WorktreePipeline::new(fx.root()).in_use(InUseSource::Native);
    let mut scan = pipeline.run();
    for _ in 0..50 {
        if scan.at("wt").in_use == scull_git::Tri::Yes {
            break;
        }
        std::thread::sleep(Duration::from_millis(100)); // let the child settle into its cwd
        scan = pipeline.run();
    }
    let revalidated = pipeline.run();
    let wt = scan.at("wt");
    assert_eq!(wt.in_use, scull_git::Tri::Yes, "pid {}", child.0.id());
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::InUse, Tier::S3);
    assert_eq!(
        wt.decision.artifact_cleanup,
        ArtifactCleanup::BlockedForThisRun
    );
    assert_eq!(wt.candidate.flags.active, Some(true));
    assert!(!scan.plan().iter().any(|c| c.id == wt.candidate.id));
    check_wt(&fx, &scan, &revalidated, &[]);
    drop(child);

    // "For this run": once the process is gone, the same clean worktree is S1 again.
    let after = WorktreePipeline::new(fx.root()).run();
    assert_eq!(after.at("wt").candidate.tier, Tier::S1);
}

/// FX-GIT-15: malicious repository: `core.fsmonitor` (and a hook) set to commands that
/// create a sentinel → the sentinel is never created; the scan completes. On Unix a control
/// run of plain `git` afterwards proves the planted command is live.
#[test]
fn fx_git_15_malicious_fsmonitor_never_runs_and_scan_completes() {
    let mut w = GitWorld::with_remote();
    w.add_worktree("wt", Some("feature"), "main");
    let sentinel = w.fx.base().join("fsmonitor-ran");
    let hook_sentinel = w.fx.base().join("hook-ran");
    let touch = |p: &std::path::Path| format!("touch '{}'", slashed(p));
    w.git(&["config", "core.fsmonitor", &touch(&sentinel)]);
    w.fx.file(
        "repo/.git/hooks/post-index-change",
        format!("#!/bin/sh\n{}\n", touch(&hook_sentinel)),
    )
    .unwrap();
    if cfg!(unix) {
        let hook = w.fx.root().join("repo/.git/hooks/post-index-change");
        assert!(
            Command::new("chmod")
                .arg("+x")
                .arg(&hook)
                .status()
                .unwrap()
                .success()
        );
    }
    // Make Git want to refresh the index: a same-size edit and an untracked file.
    w.fx.file("wt/README.md", "HELLO\n").unwrap();
    w.fx.file("wt/new.txt", "n\n").unwrap();
    w.fx.checkpoint().unwrap();

    // Both pipelines (artifacts and worktrees) over the whole root.
    let artifacts = Pipeline::new(w.fx.root()).run();
    let run = wt_run(&w);
    let _ = artifacts;
    assert!(!sentinel.exists(), "core.fsmonitor ran");
    assert!(!hook_sentinel.exists(), "a hook ran");
    // The scan completed with correct results despite the vectors.
    let wt = run.at("wt");
    assert_eq!(wt.candidate.tier, Tier::S3);
    assert_reason(wt, ReasonCode::WorktreeDirty, Tier::S3);
    assert!(
        wt.decision
            .reasons
            .iter()
            .any(|r| r.message.contains("README.md"))
    );

    if cfg!(unix) {
        // Control: plain git (isolated from the user's config, without the builder's
        // `core.fsmonitor=false`) runs the planted fsmonitor command.
        let home = w.fx.base().join("control-home");
        std::fs::create_dir_all(&home).unwrap();
        let out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(w.fx.root().join("repo"))
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            sentinel.exists(),
            "control: plain `git status` should run core.fsmonitor; the fixture is not live"
        );
    }
}

/// FX-GIT-16: bare repository with worktrees; one worktree's `.git` file points outside the
/// scan root → found via Git metadata; reported correctly; nothing outside the root touched.
#[test]
fn fx_git_16_bare_repo_worktree_pointing_outside_scan_root() {
    let w = GitWorld::with_remote();
    w.git(&["clone", "--quiet", "--bare", ".", "../bare.git"]);
    let mut fx = w.fx;
    fx.dir("scan").unwrap();
    common::worktrees::run(
        fx.git_command(
            "bare.git",
            [
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feature",
                "../scan/bwt",
                "main",
            ],
        )
        .unwrap(),
    );
    fx.checkpoint().unwrap();

    // Scan only `scan/`: the bare repository is outside it.
    let scan_root = fx.root().join("scan");
    let pipeline = WorktreePipeline::new(&scan_root);
    let scan = pipeline.run();
    let revalidated = pipeline.run();
    check_wt(&fx, &scan, &revalidated, &[]);
    assert_eq!(scan.repos.len(), 1, "found via the worktree's .git file");
    assert!(scan.repos[0].bare);
    let wt = scan.at("bwt");
    assert_eq!(wt.candidate.tier, Tier::S1, "{:#?}", wt.decision);
    assert_eq!(wt.candidate.action, Action::GitWorktreeRemove);
    // The bare repository itself (outside the root) is reported, and never removable.
    let bare: Vec<&WtItem> = scan
        .items
        .iter()
        .filter(|i| i.candidate.id != wt.candidate.id)
        .collect();
    assert_eq!(bare.len(), 1);
    assert_eq!(bare[0].candidate.tier, Tier::S3);
    assert_reason(bare[0], ReasonCode::ProtectedRoot, Tier::S3);
    assert_eq!(scan.plan().len(), 1);
}

/// FX-GIT-17: 60 agent-style worktrees in mixed states → each classified per its own state;
/// totals correct; run time within budget.
///
/// Budget: no PRD figure exists for worktree analysis yet (PRD §21.2 covers scans), so this
/// uses 2 s per worktree on a CI runner (120 s for 60, scan + discovery + analysis +
/// classification), and prints the measured time.
#[test]
fn fx_git_17_sixty_agent_worktrees_in_mixed_states() {
    const N: usize = 60;
    const BUDGET: Duration = Duration::from_secs(120);
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum State {
        Clean,
        Dirty,
        Untracked,
        Unpushed,
        Locked,
        DetachedPushed,
        Stale,
        UniqueIgnored,
    }
    const STATES: [State; 8] = [
        State::Clean,
        State::Dirty,
        State::Untracked,
        State::Unpushed,
        State::Locked,
        State::DetachedPushed,
        State::Stale,
        State::UniqueIgnored,
    ];
    let expected_tier = |s: State| match s {
        State::Clean | State::DetachedPushed => Tier::S1,
        State::Stale => Tier::S0,
        State::UniqueIgnored => Tier::S2,
        _ => Tier::S3,
    };

    let mut w = GitWorld::with_remote();
    w.fx.file("repo/.gitignore", ".env\n").unwrap();
    w.repo.commit_all("ignore .env").unwrap();
    w.git(&["push", "--quiet", "origin", "main"]);
    w.fx.dir("agents").unwrap();
    let mut want: BTreeMap<String, State> = BTreeMap::new();
    for i in 0..N {
        let s = STATES[i % STATES.len()];
        let name = format!("agents/task-{i:02}");
        let branch = format!("agent/task-{i:02}");
        let target = format!("../{name}");
        match s {
            State::DetachedPushed => {
                w.git(&[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    &target,
                    "origin/main",
                ]);
            }
            _ => {
                w.git(&["worktree", "add", "--quiet", "-b", &branch, &target, "main"]);
            }
        }
        match s {
            State::Clean | State::DetachedPushed => {}
            State::Dirty => {
                w.fx.file(format!("{name}/README.md"), format!("task {i}\n"))
                    .unwrap();
            }
            State::Untracked => {
                w.fx.file(format!("{name}/scratch.txt"), "notes\n").unwrap();
            }
            State::Unpushed => {
                w.commit_in(&name, "work.txt", &format!("{i}\n"), "agent work");
            }
            State::Locked => {
                w.git(&["worktree", "lock", "--reason", "agent running", &target]);
            }
            State::Stale => {
                std::fs::remove_dir_all(w.fx.root().join(&name)).unwrap();
            }
            State::UniqueIgnored => {
                w.fx.file(format!("{name}/.env"), "TOKEN=x\n").unwrap();
            }
        }
        want.insert(format!("task-{i:02}"), s);
    }
    w.fx.checkpoint().unwrap();

    let pipeline = WorktreePipeline::new(w.fx.root());
    let scan = pipeline.run();
    let revalidated = pipeline.run();
    check_wt(&w.fx, &scan, &revalidated, &[]);
    eprintln!(
        "FX-GIT-17: {} worktrees classified in {:.2?} (budget {:?})",
        scan.items.len(),
        scan.elapsed,
        BUDGET
    );
    assert!(scan.elapsed < BUDGET, "took {:?}", scan.elapsed);

    // Each classified per its own state.
    let mut got_tiers: BTreeMap<Tier, usize> = BTreeMap::new();
    let mut seen = 0;
    for item in &scan.items {
        let name = item
            .state
            .path
            .to_path_buf()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(&s) = want.get(&name) else {
            continue; // the main worktree and the bare remote
        };
        seen += 1;
        assert_eq!(
            item.candidate.tier,
            expected_tier(s),
            "{name} ({s:?}): {:#?}",
            item.decision
        );
        assert_eq!(item.state.missing, s == State::Stale, "{name}");
        *got_tiers.entry(item.candidate.tier).or_default() += 1;
    }
    assert_eq!(seen, N, "every agent worktree found exactly once");

    // Totals: per-tier counts match the states built, and the plan is exactly the non-S3 ones.
    let mut want_tiers: BTreeMap<Tier, usize> = BTreeMap::new();
    for s in want.values() {
        *want_tiers.entry(expected_tier(*s)).or_default() += 1;
    }
    assert_eq!(got_tiers, want_tiers);
    let planned = scan.plan().len();
    assert_eq!(planned, N - want_tiers.get(&Tier::S3).copied().unwrap_or(0));
    // Every present worktree is sized by the scan; stale registrations hold no bytes.
    for item in &scan.items {
        if want.contains_key(
            &item
                .state
                .path
                .to_path_buf()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ) {
            if item.state.missing {
                assert_eq!(item.candidate.sizes.logical, 0);
            } else {
                assert!(item.candidate.sizes.logical > 0, "{}", item.state.path);
            }
        }
    }
}
