//! The GIT-03 decision table against real repositories (FX-GIT-01…14, -16), built with
//! `scull-fixtures`: discover → analyse (GIT-02) → classify (GIT-03) → candidate.
//!
//! Every test ends with the safety invariants: I-1 over the plan a planner would build from
//! the candidates (S3 never enters it), I-2 canary untouched, I-3 nothing in the root changed
//! (classification is read-only and never fetches). Skips (passes) when `git` is not installed.
//!
//! In-use: process-cwd / open-file detection is P1-SAFE-02 (#49), which isn't available yet, so
//! the in-use signal is passed in. Clean fixtures pass `Tri::No` (as a detector that found
//! nothing would); FX-GIT-14 starts a real process with its working directory in the worktree
//! and passes `Tri::Yes`.

mod fx;

use fx::{DetectorRules, World, slashed};
use scull_core::{Action, Candidate, FileIdentity, ReasonCode, Sizes, Tier};
use scull_fixtures::InvariantInputs;
use scull_git::{
    ArtifactCleanup, ArtifactMatcher, NoArtifactRules, Tri, WorktreeDecision, WorktreeState,
    classify_worktree, worktree_candidate,
};
use scull_platform::Platform;
use std::process::{Command, Stdio};

/// Classifies `state` and builds its candidate (identity read through scull-platform).
fn decide(
    state: &WorktreeState,
    in_use: Tri,
    rules: &dyn ArtifactMatcher,
) -> (WorktreeDecision, Candidate) {
    let dec = classify_worktree(state, in_use, rules);
    let at = if state.missing {
        state.git_dir.to_path_buf()
    } else {
        state.path.to_path_buf()
    };
    let identity = scull_platform::Native
        .metadata(&at)
        .ok()
        .and_then(|m| m.identity)
        .unwrap_or(FileIdentity::new(0, 0));
    let c = worktree_candidate(state, &dec, in_use, identity, Sizes::default());
    assert!(c.is_consistent(), "{c:#?}");
    assert_eq!(c.tier, dec.tier);
    (dec, c)
}

/// Asserts a reason with `code` fired at exactly `tier`.
fn assert_reason(dec: &WorktreeDecision, code: ReasonCode, tier: Tier) {
    assert!(
        dec.reasons.iter().any(|r| r.code == code && r.tier == tier),
        "expected {code:?} at {tier}: {dec:#?}"
    );
}

/// Runs the invariants, feeding I-1 the plan a planner would build (no S3 items).
fn finish(w: &World, candidates: &[&Candidate]) {
    let plan: Vec<&Candidate> = candidates
        .iter()
        .copied()
        .filter(|c| c.tier.is_actionable())
        .collect();
    w.assert_invariants(&InvariantInputs::new().plan(plan));
}

#[test]
fn fx_git_01_clean_merged_pushed_worktree_is_s1_never_s0() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "feature.txt", "f\n", "feature work");
    w.git(&["merge", "--quiet", "--ff-only", "feature"]);
    w.git(&["push", "--quiet", "origin", "main"]);
    w.checkpoint();

    let st = w.analyze("wt");
    let (dec, c) = decide(&st, Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S1);
    assert_reason(&dec, ReasonCode::WorktreeClean, Tier::S1);
    assert_eq!(dec.reasons.len(), 1);
    assert_eq!(c.action, Action::GitWorktreeRemove);
    assert_eq!(dec.artifact_cleanup, ArtifactCleanup::Allowed);
    // The branch is preserved: the only action is `git worktree remove`, and the candidate
    // tells how to recreate the worktree from that branch.
    let regen = c
        .regeneration
        .instructions
        .as_deref()
        .expect("instructions");
    assert!(
        regen.contains("git worktree add") && regen.contains("feature"),
        "{regen}"
    );
    assert!(!w.git(&["rev-parse", "--verify", "feature"]).is_empty());
    finish(&w, &[&c]);
}

#[test]
fn fx_git_02_modified_tracked_file_is_s3_with_artifact_cleanup_allowed() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/README.md", "changed\n").expect("write");
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::WorktreeDirty, Tier::S3);
    assert!(dec.reasons.iter().any(|r| r.message.contains("README.md")));
    assert_eq!(c.action, Action::None);
    assert_eq!(dec.artifact_cleanup, ArtifactCleanup::Allowed);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_03_untracked_file_is_s3() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/notes.txt", "mine\n").expect("write");
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::WorktreeDirty, Tier::S3);
    assert!(dec.reasons.iter().any(|r| r.message.contains("notes.txt")));
    finish(&w, &[&c]);
}

#[test]
fn fx_git_04_unique_ignored_files_are_s2_and_listed_by_name() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    // A Rust project whose `target/` is a recognised artifact; `.env` and `dev.sqlite` are not.
    w.fx.file("repo/.gitignore", ".env\n*.sqlite\ntarget/\n")
        .expect("write");
    w.fx.file(
        "repo/Cargo.toml",
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .expect("write");
    w.fx.file("repo/Cargo.lock", "version = 4\n")
        .expect("write");
    w.repo.commit_all("rust project").expect("commit");
    w.git(&["push", "--quiet", "origin", "main"]);
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/.env", "SECRET=1\n").expect("write");
    w.fx.file("wt/dev.sqlite", "db").expect("write");
    w.fx.file("wt/target/debug/app", "bin").expect("write");
    // Cargo tags every target dir it creates; the `rust.target` rule requires the tag.
    w.fx.file("wt/target/CACHEDIR.TAG", scull_fixtures::CACHEDIR_TAG)
        .expect("write");
    w.checkpoint();

    let st = w.analyze("wt");
    let (dec, c) = decide(&st, Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S2);
    assert_reason(&dec, ReasonCode::WorktreeUniqueIgnored, Tier::S2);
    let mut unique = dec.unique_ignored.clone();
    unique.sort();
    assert_eq!(unique, vec![".env", "dev.sqlite"]);
    let msg = &dec
        .reasons
        .iter()
        .find(|r| r.code == ReasonCode::WorktreeUniqueIgnored)
        .expect("reason")
        .message;
    assert!(msg.contains(".env") && msg.contains("dev.sqlite"), "{msg}");
    assert_eq!(dec.artifacts.len(), 1);
    assert_eq!(dec.artifacts[0].path, "target/");
    assert_eq!(dec.artifacts[0].rule, "rust.target");
    // S2: quarantine only (restorable), never an irreversible `git worktree remove`.
    assert_eq!(c.action, Action::Quarantine);

    // Without artifact rules, `target/` is unique too: more protective, never less.
    let (strict, _) = decide(&st, Tri::No, &NoArtifactRules);
    assert_eq!(strict.tier, Tier::S2);
    assert_eq!(strict.unique_ignored.len(), 3);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_05_unpushed_commits_are_s3() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "one");
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::UnpushedCommits, Tier::S3);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_06_squash_merge_without_forge_is_s2_merge_unproven() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "feature.txt", "feature\n", "feature work");
    w.fx.file("repo/other.txt", "other\n").expect("write");
    w.repo.commit_all("other work on main").expect("commit");
    w.git(&["merge", "--quiet", "--squash", "feature"]);
    w.git(&["commit", "--quiet", "-m", "Squashed feature (#1)"]);
    w.git(&["push", "--quiet", "origin", "main"]);
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S2);
    assert_reason(&dec, ReasonCode::MergeUnproven, Tier::S2);
    assert!(
        dec.reasons
            .iter()
            .any(|r| r.message.contains("merge unproven")),
        "{dec:#?}"
    );
    // S2: quarantine only (restorable), never an irreversible `git worktree remove`.
    assert_eq!(c.action, Action::Quarantine);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_07_orphan_detached_head_is_s3() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", None, "main");
    w.commit_in("wt", "x.txt", "x\n", "detached work");
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::OrphanHead, Tier::S3);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_08_detached_head_on_pushed_commit_clean_is_s1() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", None, "origin/main");
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S1);
    assert_reason(&dec, ReasonCode::WorktreeClean, Tier::S1);
    assert_eq!(c.action, Action::GitWorktreeRemove);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_09_locked_worktree_is_s3_with_reason_shown() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.git(&[
        "worktree",
        "lock",
        "--reason",
        "agent run 42 in progress",
        "../wt",
    ]);
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::WorktreeLocked, Tier::S3);
    assert!(
        dec.reasons
            .iter()
            .any(|r| r.message.contains("agent run 42 in progress"))
    );
    assert_eq!(dec.artifact_cleanup, ArtifactCleanup::Allowed);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_10_stale_registration_is_s0_prune_metadata_only() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    let dir = w.add_worktree("wt", Some("feature"), "main");
    // Unpushed work on the branch must not matter: prune only drops metadata, the branch stays.
    w.commit_in("wt", "a.txt", "a\n", "unpushed");
    std::fs::remove_dir_all(&dir).expect("remove worktree folder inside the fixture");
    w.checkpoint();

    let st = w.analyze("wt");
    let (dec, c) = decide(&st, Tri::Unknown, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S0);
    assert_eq!(dec.reasons.len(), 1);
    assert_reason(&dec, ReasonCode::StaleWorktreeRegistration, Tier::S0);
    assert_eq!(c.action, Action::GitWorktreePrune);
    assert_eq!(dec.artifact_cleanup, ArtifactCleanup::NotApplicable);
    // The candidate points at the admin dir, which still exists.
    assert_eq!(c.path, st.git_dir);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_11_no_upstream_local_only_commits_is_s3() {
    let Some(mut w) = World::local() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "local only");
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::UnpushedCommits, Tier::S3);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_12_stale_remote_refs_classify_protectively_without_fetch() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "pushed later");
    w.git_in("wt", &["push", "--quiet", "-u", "origin", "feature"]);
    let base = w.git(&["rev-parse", "main"]);
    w.git(&["update-ref", "refs/remotes/origin/feature", &base]);
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert!(dec.tier >= Tier::S2, "never lower than S2: {dec:#?}");
    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::UnpushedCommits, Tier::S3);
    assert!(!w.fx.path("repo/.git/FETCH_HEAD").expect("path").exists());
    finish(&w, &[&c]);
}

#[test]
fn fx_git_13_dirty_submodule_is_s3() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    let lib = w.fx.git_repo("sublib").expect("repo");
    w.fx.file("sublib/lib.txt", "lib\n").expect("write");
    lib.commit_all("lib").expect("commit");
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
        .expect("write");
    w.checkpoint();

    let (dec, c) = decide(&w.analyze("wt"), Tri::No, &DetectorRules::bundled());
    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::WorktreeDirty, Tier::S3);
    assert!(
        dec.reasons
            .iter()
            .any(|r| r.message.contains("submodule sub"))
    );
    finish(&w, &[&c]);
}

#[test]
fn fx_git_14_process_cwd_in_worktree_is_s3_in_use_for_this_run() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    let dir = w.add_worktree("wt", None, "origin/main");
    w.checkpoint();

    // A real process with its working directory inside the worktree.
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");
    let st = w.analyze("wt");
    // Process-cwd detection is P1-SAFE-02 (#49, not available yet): the signal it would
    // report is passed in.
    let (dec, c) = decide(&st, Tri::Yes, &DetectorRules::bundled());
    drop(child.stdin.take());
    let _ = child.wait();

    assert_eq!(dec.tier, Tier::S3);
    assert_reason(&dec, ReasonCode::InUse, Tier::S3);
    assert_eq!(dec.artifact_cleanup, ArtifactCleanup::BlockedForThisRun);
    assert_eq!(c.flags.active, Some(true));
    assert_eq!(c.action, Action::None);

    // The same clean worktree, not in use: S1; in-use unknown: S2 (disclosed).
    let (idle, _) = decide(&st, Tri::No, &DetectorRules::bundled());
    assert_eq!(idle.tier, Tier::S1);
    let (unknown, _) = decide(&st, Tri::Unknown, &DetectorRules::bundled());
    assert_eq!(unknown.tier, Tier::S2);
    assert_reason(&unknown, ReasonCode::SignalUnavailable, Tier::S2);
    finish(&w, &[&c]);
}

#[test]
fn fx_git_16_bare_repo_worktree_outside_scan_root() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    // The bare repository lives outside the scan root; its worktree inside it, so the
    // worktree's `.git` file points outside the scan root.
    w.git(&["clone", "--quiet", "--bare", ".", "../bare.git"]);
    w.fx.dir("scan").expect("mkdir");
    w.git_in(
        "bare.git",
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "feature",
            "../scan/bwt",
            "main",
        ],
    );
    w.checkpoint();

    let scan_root = w.fx.path("scan").expect("path");
    let repo = scull_git::discover(&scan_root.join("bwt")).expect("discover via .git file");
    assert!(repo.bare);
    let states = scull_git::analyze_repo(
        &w.runner,
        &repo,
        |wt| scull_git::sample_activity(&scull_platform::Native, &wt.path.to_path_buf(), 1000),
        &scull_git::AnalyzeOptions::default(),
    );
    let mut candidates = Vec::new();
    for (wt, st) in repo.worktrees.iter().zip(&states) {
        let (dec, c) = decide(st, Tri::No, &DetectorRules::bundled());
        if wt.is_within(&scan_root) {
            // The linked worktree: clean, `feature` == `main` (merged): S1.
            assert_eq!(dec.tier, Tier::S1, "{dec:#?}");
            assert_eq!(c.action, Action::GitWorktreeRemove);
        } else {
            // The bare repository itself (outside the root): never removable.
            assert_eq!(dec.tier, Tier::S3);
            assert_reason(&dec, ReasonCode::ProtectedRoot, Tier::S3);
        }
        candidates.push(c);
    }
    assert_eq!(candidates.len(), 2);
    // Nothing outside (or inside) the root was touched: I-2 canary, I-3 root unchanged.
    let refs: Vec<&Candidate> = candidates.iter().collect();
    finish(&w, &refs);
}
