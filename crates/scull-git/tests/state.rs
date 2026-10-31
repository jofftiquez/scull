//! Worktree state analysis (GIT-02) against real repositories built with `scull-fixtures`.
//!
//! Each test is named after the FX-GIT fixture it covers and ends by running the safety
//! invariants: the analysis is read-only, so nothing in the fixture root may change (no fetch,
//! no index refresh, no lock files). Skips (passes) when `git` is not installed.

mod fx;

use fx::{World, paths, slashed};
use scull_fixtures::InvariantInputs;
use scull_git::{
    AnalyzeOptions, HeadState, LockState, MergeStatus, Observed, Reachability, WorktreeKind,
};
use std::process::{Command, Stdio};

#[test]
fn fx_git_01_merged_and_pushed_worktree_is_clean_and_proven_merged() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "feature.txt", "f\n", "feature work");
    // Merge into main (fast-forward) and push it.
    w.git(&["merge", "--quiet", "--ff-only", "feature"]);
    w.git(&["push", "--quiet", "origin", "main"]);
    w.checkpoint();

    let st = w.analyze("wt");
    assert_eq!(st.kind, WorktreeKind::Linked);
    assert!(!st.missing);
    assert!(paths(&st.tracked_changes).is_empty());
    assert!(paths(&st.untracked).is_empty());
    assert!(paths(&st.ignored).is_empty());
    assert_eq!(st.submodules_dirty(), Some(false));
    assert!(st.unpushed.value().expect("known").is_empty());
    assert_eq!(
        st.merge,
        MergeStatus::AncestorOfDefault {
            default_ref: "refs/remotes/origin/main".into()
        }
    );
    match st.head.value().expect("head") {
        HeadState::Branch { name, oid } => {
            assert_eq!(name, "refs/heads/feature");
            assert!(oid.is_some());
        }
        h => panic!("unexpected head {h:?}"),
    }
    // Activity was sampled through scull-platform.
    let act = st.activity.value().expect("activity");
    assert!(act.newest_unix.is_some() && act.entries_sampled > 1);
    // Parent repository is reported.
    assert!(st.repo.common_dir.as_str().ends_with(".git"));
    assert!(st.repo.main_worktree.is_some());
    // Serializable.
    let json = serde_json::to_value(&st).expect("serialize");
    assert_eq!(json["merge"]["method"], "ancestor_of_default");
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_02_modified_tracked_file_is_reported() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/README.md", "changed\n").expect("write");
    w.checkpoint();

    let st = w.analyze("wt");
    assert_eq!(paths(&st.tracked_changes), vec!["README.md"]);
    assert!(paths(&st.untracked).is_empty());
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_03_untracked_file_is_reported() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/notes.txt", "mine\n").expect("write");
    w.fx.file("wt/drafts/a.md", "a\n").expect("write");
    w.checkpoint();

    let st = w.analyze("wt");
    assert!(paths(&st.tracked_changes).is_empty());
    let mut u = paths(&st.untracked);
    u.sort();
    assert_eq!(u, vec!["drafts/", "notes.txt"]);
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_04_ignored_files_are_listed_by_name() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.fx.file("repo/.gitignore", ".env\n*.sqlite\ntarget/\n")
        .expect("write");
    w.repo.commit_all("ignore").expect("commit");
    w.git(&["push", "--quiet", "origin", "main"]);
    w.add_worktree("wt", Some("feature"), "main");
    w.fx.file("wt/.env", "SECRET=1\n").expect("write");
    w.fx.file("wt/dev.sqlite", "db").expect("write");
    w.fx.file("wt/target/debug/app", "bin").expect("write");
    w.checkpoint();

    let st = w.analyze("wt");
    let mut ig = paths(&st.ignored);
    ig.sort();
    assert_eq!(ig, vec![".env", "dev.sqlite", "target/"]);
    assert!(paths(&st.untracked).is_empty());
    assert!(paths(&st.tracked_changes).is_empty());
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_05_unpushed_commits_are_counted() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    let c1 = w.commit_in("wt", "a.txt", "a\n", "one");
    let c2 = w.commit_in("wt", "b.txt", "b\n", "two");
    w.checkpoint();

    let st = w.analyze("wt");
    let un = st.unpushed.value().expect("known");
    assert_eq!(un.count, 2);
    assert_eq!(un.oids, vec![c2, c1]);
    assert!(!un.truncated);
    assert!(
        matches!(&st.merge, MergeStatus::NotMerged { checked } if checked.contains(&"refs/remotes/origin/main".to_string())),
        "{:?}",
        st.merge
    );
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_06_squash_merged_branch_is_patch_id_only() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "feature.txt", "feature\n", "feature work");
    // Upstream moves on, then the feature lands as a squash commit (different id, same patch).
    w.fx.file("repo/other.txt", "other\n").expect("write");
    w.repo.commit_all("other work on main").expect("commit");
    w.git(&["merge", "--quiet", "--squash", "feature"]);
    w.git(&["commit", "--quiet", "-m", "Squashed feature (#1)"]);
    w.git(&["push", "--quiet", "origin", "main"]);
    w.checkpoint();

    let st = w.analyze("wt");
    assert_eq!(st.unpushed.value().expect("known").count, 1);
    assert_eq!(
        st.merge,
        MergeStatus::PatchIdOnly {
            default_ref: "refs/remotes/origin/main".into(),
            commits: 1
        }
    );
    assert!(!st.merge.is_proven());

    // With the heuristic off, the same branch is simply not merged.
    let off = w.analyze_with(
        "wt",
        &AnalyzeOptions {
            patch_id: false,
            ..AnalyzeOptions::default()
        },
    );
    assert!(matches!(off.merge, MergeStatus::NotMerged { .. }));
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_07_detached_head_reachable_from_no_ref() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", None, "main");
    let orphan = w.commit_in("wt", "x.txt", "x\n", "detached work");
    w.checkpoint();

    let st = w.analyze("wt");
    assert_eq!(st.head.value(), Some(&HeadState::Detached { oid: orphan }));
    assert_eq!(
        st.head_reachability.value(),
        Some(&Reachability::Unreachable)
    );
    assert_eq!(st.upstream.value(), Some(&None));
    assert_eq!(st.unpushed.value().expect("known").count, 1);
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_08_detached_head_reachable_from_pushed_ref() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", None, "origin/main");
    w.checkpoint();

    let st = w.analyze("wt");
    match st.head_reachability.value().expect("known") {
        Reachability::ReachableFrom { refs, .. } => {
            assert!(
                refs.contains(&"refs/remotes/origin/main".to_string()),
                "{refs:?}"
            );
        }
        r => panic!("unexpected {r:?}"),
    }
    assert!(st.unpushed.value().expect("known").is_empty());
    assert!(st.merge.is_proven(), "{:?}", st.merge);
    assert!(paths(&st.tracked_changes).is_empty());
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_09_lock_and_reason_are_reported() {
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

    let st = w.analyze("wt");
    assert_eq!(
        st.lock,
        LockState::Locked {
            reason: Some("agent run 42 in progress".into())
        }
    );
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_10_missing_folder_is_reported_as_missing_and_unknown() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    let dir = w.add_worktree("wt", Some("feature"), "main");
    std::fs::remove_dir_all(&dir).expect("remove worktree folder inside the fixture");
    w.checkpoint();

    let st = w.analyze("wt");
    assert!(st.missing);
    assert!(st.tracked_changes.is_unknown());
    assert!(st.unpushed.is_unknown());
    assert!(matches!(st.merge, MergeStatus::Unknown { .. }));
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_11_no_upstream_and_local_only_commits() {
    let Some(mut w) = World::local() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "local only");
    w.checkpoint();

    let st = w.analyze("wt");
    assert_eq!(st.upstream.value(), Some(&None));
    // No remote at all: every commit is unpushed (the initial one and the feature commit).
    assert_eq!(st.unpushed.value().expect("known").count, 2);
    assert_eq!(
        st.merge,
        MergeStatus::NotMerged {
            checked: vec!["refs/heads/main".into()]
        }
    );
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_12_stale_remote_refs_are_used_as_is_and_nothing_is_fetched() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.add_worktree("wt", Some("feature"), "main");
    w.commit_in("wt", "a.txt", "a\n", "pushed later");
    w.git_in("wt", &["push", "--quiet", "-u", "origin", "feature"]);
    // The remote has the commit, but our remote-tracking ref is stale (as if the push happened
    // from another clone and we never fetched).
    let base = w.git(&["rev-parse", "main"]);
    w.git(&["update-ref", "refs/remotes/origin/feature", &base]);
    w.checkpoint();

    let st = w.analyze("wt");
    let up = st
        .upstream
        .value()
        .expect("known")
        .clone()
        .expect("upstream");
    assert_eq!(up.name, "refs/remotes/origin/feature");
    assert!(up.exists);
    assert_eq!(up.ahead, Some(1));
    assert_eq!(st.unpushed.value().expect("known").count, 1);
    assert!(!st.merge.is_proven());
    // I-3: nothing in the root changed: no FETCH_HEAD, no updated refs, remote untouched.
    assert!(!w.fx.path("repo/.git/FETCH_HEAD").expect("path").exists());
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_13_dirty_submodule_is_reported() {
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
    // Local change inside the submodule's work tree only.
    w.fx.file("wt/sub/lib.txt", "changed in submodule\n")
        .expect("write");
    w.checkpoint();

    let st = w.analyze("wt");
    let subs = st.submodules.value().expect("known");
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].path, "sub");
    assert!(subs[0].initialized);
    assert_eq!(subs[0].dirty, Observed::known(true));
    assert_eq!(st.submodules_dirty(), Some(true));
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_14_analysis_works_while_a_process_has_its_cwd_in_the_worktree() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    let dir = w.add_worktree("wt", Some("feature"), "main");
    w.checkpoint();

    // A long-lived child whose working directory is the worktree (waits on stdin).
    let mut child = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");
    let st = w.analyze("wt");
    drop(child.stdin.take());
    let _ = child.wait();

    // State analysis itself reports the facts; "in use" is an input to the classifier
    // (process-cwd detection is P1-SAFE-02).
    assert!(paths(&st.tracked_changes).is_empty());
    assert!(st.unpushed.value().is_some());
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn fx_git_16_bare_repository_with_worktrees() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    w.git(&["clone", "--quiet", "--bare", ".", "../bare.git"]);
    w.git_in(
        "bare.git",
        &[
            "worktree", "add", "--quiet", "-b", "feature", "../bwt", "main",
        ],
    );
    w.checkpoint();

    let repo = scull_git::discover(&w.fx.path("bare.git").expect("path")).expect("discover");
    let states = scull_git::analyze_repo(
        &w.runner,
        &repo,
        |_| Observed::unknown("not sampled"),
        &AnalyzeOptions::default(),
    );
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].kind, WorktreeKind::Bare);
    assert!(states[0].repo.bare);
    assert!(
        states[0].tracked_changes.is_unknown(),
        "bare: no working tree"
    );
    assert_eq!(states[1].kind, WorktreeKind::Linked);
    assert!(paths(&states[1].tracked_changes).is_empty());
    assert!(states[1].activity.is_unknown());
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn activity_sample_is_bounded_and_says_so() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    let dir = w.add_worktree("wt", Some("feature"), "main");
    w.checkpoint();
    let full = scull_git::sample_activity(&scull_platform::Native, &dir, 10_000);
    let full = full.value().expect("known");
    assert!(full.complete);
    let cut = scull_git::sample_activity(&scull_platform::Native, &dir, 2);
    let cut = cut.value().expect("known");
    assert!(!cut.complete);
    assert_eq!(cut.entries_sampled, 2);
    let missing = scull_git::sample_activity(&scull_platform::Native, &dir.join("nope"), 10);
    assert!(missing.is_unknown());
    w.assert_invariants(&InvariantInputs::new());
}

#[test]
fn worktree_state_has_a_json_schema() {
    let schema = schemars::schema_for!(scull_git::WorktreeState);
    let json = serde_json::to_string(&schema).expect("schema");
    for field in ["tracked_changes", "head_reachability", "merge", "activity"] {
        assert!(json.contains(field), "{field} missing from schema");
    }
}

#[test]
fn runner_errors_make_facts_unknown_never_clean() {
    let Some(mut w) = World::with_remote() else {
        return;
    };
    let dir = w.add_worktree("wt", Some("feature"), "main");
    let repo = scull_git::discover(w.repo.path()).expect("discover");
    // Break the worktree after discovery: its `.git` link disappears.
    std::fs::remove_file(dir.join(".git")).expect("remove .git file inside the fixture");
    w.checkpoint();

    let wt = repo
        .worktrees
        .iter()
        .find(|x| x.kind == WorktreeKind::Linked)
        .expect("linked");
    let st = scull_git::analyze_worktree(
        &w.runner,
        &repo,
        wt,
        Observed::unknown("not sampled"),
        &AnalyzeOptions::default(),
    );
    assert!(st.tracked_changes.is_unknown(), "{:?}", st.tracked_changes);
    assert!(st.untracked.is_unknown());
    assert!(st.ignored.is_unknown());
    assert!(st.submodules.is_unknown());
    assert_eq!(st.submodules_dirty(), None);
    w.assert_invariants(&InvariantInputs::new());
}
