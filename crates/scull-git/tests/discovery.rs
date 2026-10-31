//! Worktree discovery against fixtures built with the real `git` binary (GIT-01).
//!
//! Every test cross-checks the library's answer with `git worktree list --porcelain` for the
//! same repository. Skips (passes) when `git` is not installed.

mod common;

use common::{Sandbox, canon, parse_porcelain, snapshot};
use scull_git::{
    DiscoverError, LockState, PruneReason, RepoWorktrees, WorktreeHead, WorktreeKind, discover,
    list_worktrees,
};
use std::fs;
use std::path::Path;

/// Asserts that `found` agrees with `git worktree list --porcelain` run in `repo`.
fn assert_matches_git(sb: &Sandbox, repo: &Path, found: &RepoWorktrees) {
    let porcelain = sb.git(repo, &["worktree", "list", "--porcelain"]);
    let expected = parse_porcelain(&porcelain);
    assert_eq!(
        found.worktrees.len(),
        expected.len(),
        "worktree count differs\nours: {found:#?}\ngit:\n{porcelain}"
    );
    for (ours, git) in found.worktrees.iter().zip(&expected) {
        let ctx = format!("ours: {ours:#?}\ngit: {git:#?}");
        assert_eq!(canon(&ours.path.to_path_buf()), canon(&git.path), "{ctx}");
        assert_eq!(ours.kind == WorktreeKind::Bare, git.bare, "{ctx}");
        match &ours.head {
            WorktreeHead::Branch { name } if !git.bare => {
                assert_eq!(git.branch.as_deref(), Some(name.as_str()), "{ctx}");
            }
            WorktreeHead::Detached { oid } => {
                assert!(git.detached, "{ctx}");
                assert_eq!(git.head.as_deref(), Some(oid.as_str()), "{ctx}");
            }
            WorktreeHead::Branch { .. } => {}
            WorktreeHead::Invalid { detail } => panic!("unexpected invalid HEAD: {detail}"),
        }
        match &ours.lock {
            LockState::Unlocked => assert_eq!(git.locked, None, "{ctx}"),
            LockState::Locked { reason } => assert_eq!(
                git.locked.as_deref(),
                Some(reason.as_deref().unwrap_or("")),
                "{ctx}"
            ),
        }
        assert_eq!(ours.prunable.is_some(), git.prunable, "{ctx}");
    }
}

#[test]
fn basic_discovery_from_any_path_matches_git() {
    let Some(sb) = Sandbox::new_with_git() else {
        return;
    };
    let repo = sb.init_repo("repo");
    let root = sb.path();
    sb.git(
        &repo,
        &["worktree", "add", "-q", "-b", "agent-1", "../wt-branch"],
    );
    sb.git(
        &repo,
        &["worktree", "add", "-q", "--detach", "../wt-detached"],
    );
    fs::create_dir_all(repo.join("src/deep")).unwrap();
    fs::create_dir_all(root.join("wt-branch/nested")).unwrap();
    fs::write(root.join("wt-branch/nested/file.txt"), "x").unwrap();

    let from_main = discover(&repo).unwrap();
    assert_eq!(
        canon(&from_main.common_dir.to_path_buf()),
        repo.join(".git")
    );
    assert!(!from_main.bare);
    assert!(from_main.problems.is_empty(), "{:#?}", from_main.problems);
    assert_eq!(from_main.worktrees.len(), 3);

    let main = &from_main.worktrees[0];
    assert_eq!(main.kind, WorktreeKind::Main);
    assert_eq!(canon(&main.path.to_path_buf()), repo);
    assert_eq!(
        main.head,
        WorktreeHead::Branch {
            name: "refs/heads/main".into()
        }
    );

    let branch = &from_main.worktrees[1];
    assert_eq!(branch.kind, WorktreeKind::Linked);
    assert_eq!(branch.name.as_deref(), Some("wt-branch"));
    assert_eq!(canon(&branch.path.to_path_buf()), root.join("wt-branch"));
    assert_eq!(
        canon(&branch.git_dir.to_path_buf()),
        repo.join(".git/worktrees/wt-branch")
    );
    assert_eq!(
        branch.head,
        WorktreeHead::Branch {
            name: "refs/heads/agent-1".into()
        }
    );
    assert_eq!(branch.lock, LockState::Unlocked);
    assert_eq!(branch.prunable, None);

    let detached = &from_main.worktrees[2];
    let head_sha = sb.git(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(detached.head, WorktreeHead::Detached { oid: head_sha });

    assert_matches_git(&sb, &repo, &from_main);

    // Any path inside the repository or one of its worktrees finds the same repository.
    for start in [
        repo.join("src/deep"),
        repo.join(".git"),
        repo.join(".git/refs"),
        repo.join(".git/worktrees/wt-branch"),
        root.join("wt-branch"),
        root.join("wt-branch/nested"),
        root.join("wt-branch/nested/file.txt"),
        root.join("wt-detached"),
    ] {
        let found = discover(&start).unwrap_or_else(|e| panic!("{}: {e}", start.display()));
        assert_eq!(found, from_main, "starting from {}", start.display());
    }

    assert_eq!(list_worktrees(&repo.join(".git")).unwrap(), from_main);
}

#[test]
fn fx_git_09_locked_worktree() {
    let Some(sb) = Sandbox::new_with_git() else {
        return;
    };
    let repo = sb.init_repo("repo");
    sb.git(&repo, &["worktree", "add", "-q", "-b", "a", "../wt-reason"]);
    sb.git(
        &repo,
        &["worktree", "add", "-q", "-b", "b", "../wt-noreason"],
    );
    sb.git(
        &repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "agent session 42 is running",
            "../wt-reason",
        ],
    );
    sb.git(&repo, &["worktree", "lock", "../wt-noreason"]);

    let found = discover(&repo).unwrap();
    let by_name = |n: &str| {
        found
            .worktrees
            .iter()
            .find(|w| w.name.as_deref() == Some(n))
            .unwrap_or_else(|| panic!("{n} not found"))
    };
    assert_eq!(
        by_name("wt-reason").lock,
        LockState::Locked {
            reason: Some("agent session 42 is running".into())
        }
    );
    assert_eq!(
        by_name("wt-noreason").lock,
        LockState::Locked { reason: None }
    );
    assert!(by_name("wt-reason").lock.is_locked());
    assert_matches_git(&sb, &repo, &found);
}

#[test]
fn fx_git_10_prunable_registration() {
    let Some(sb) = Sandbox::new_with_git() else {
        return;
    };
    let repo = sb.init_repo("repo");
    let root = sb.path();
    sb.git(
        &repo,
        &["worktree", "add", "-q", "-b", "gone", "../wt-gone"],
    );
    sb.git(
        &repo,
        &["worktree", "add", "-q", "-b", "kept", "../wt-locked-gone"],
    );
    sb.git(
        &repo,
        &["worktree", "lock", "--reason", "usb", "../wt-locked-gone"],
    );
    sb.git(
        &repo,
        &["worktree", "add", "-q", "-b", "alive", "../wt-alive"],
    );
    // Delete the folders (inside the sandbox) without telling Git.
    fs::remove_dir_all(root.join("wt-gone")).unwrap();
    fs::remove_dir_all(root.join("wt-locked-gone")).unwrap();

    let admin_before = snapshot(&repo.join(".git/worktrees"));
    let found = discover(&repo).unwrap();
    let by_name = |n: &str| {
        found
            .worktrees
            .iter()
            .find(|w| w.name.as_deref() == Some(n))
            .unwrap_or_else(|| panic!("{n} not found"))
    };
    assert_eq!(
        by_name("wt-gone").prunable,
        Some(PruneReason::WorktreeMissing)
    );
    assert_eq!(
        canon(&by_name("wt-gone").path.to_path_buf()),
        root.join("wt-gone")
    );
    // Locked registrations are never prunable, even when the folder is gone.
    assert_eq!(by_name("wt-locked-gone").prunable, None);
    assert!(by_name("wt-locked-gone").lock.is_locked());
    assert_eq!(by_name("wt-alive").prunable, None);
    assert!(found.problems.is_empty(), "{:#?}", found.problems);

    assert_matches_git(&sb, &repo, &found);
    // Discovery only reads: the stale registration is still there.
    assert_eq!(snapshot(&repo.join(".git/worktrees")), admin_before);
}

#[test]
fn fx_git_16_bare_repo_worktree_outside_scan_root() {
    let Some(sb) = Sandbox::new_with_git() else {
        return;
    };
    let base = sb.path();
    let src = sb.init_repo("src");
    let outside = base.join("outside");
    let scan_root = base.join("root");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&scan_root).unwrap();
    sb.git(
        &outside,
        &["clone", "-q", "--bare", &common::slashed(&src), "repo.git"],
    );
    let bare = outside.join("repo.git");
    sb.git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "inside",
            &common::slashed(&scan_root.join("wt-in")),
        ],
    );
    sb.git(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "elsewhere",
            &common::slashed(&outside.join("wt-out")),
        ],
    );
    fs::write(scan_root.join("wt-in/notes.txt"), "work").unwrap();

    // The worktree inside the scan root has a `.git` file pointing outside it.
    let dotgit = fs::read_to_string(scan_root.join("wt-in/.git")).unwrap();
    assert!(dotgit.starts_with("gitdir:"), "{dotgit}");

    let outside_before = snapshot(&outside);
    let scan_root_before = snapshot(&scan_root);

    let found = discover(&scan_root.join("wt-in/notes.txt")).unwrap();

    assert!(found.bare);
    assert_eq!(canon(&found.common_dir.to_path_buf()), bare);
    assert!(found.problems.is_empty(), "{:#?}", found.problems);
    assert_eq!(found.worktrees.len(), 3);
    assert_eq!(found.worktrees[0].kind, WorktreeKind::Bare);
    assert_eq!(canon(&found.worktrees[0].path.to_path_buf()), bare);
    assert!(!found.worktrees[0].is_within(&scan_root));

    let wt_in = found
        .worktrees
        .iter()
        .find(|w| w.name.as_deref() == Some("wt-in"))
        .unwrap();
    assert_eq!(wt_in.kind, WorktreeKind::Linked);
    assert_eq!(canon(&wt_in.path.to_path_buf()), scan_root.join("wt-in"));
    assert_eq!(
        wt_in.head,
        WorktreeHead::Branch {
            name: "refs/heads/inside".into()
        }
    );
    assert!(wt_in.is_within(&scan_root));

    let wt_out = found
        .worktrees
        .iter()
        .find(|w| w.name.as_deref() == Some("wt-out"))
        .unwrap();
    assert!(!wt_out.is_within(&scan_root));

    assert_matches_git(&sb, &bare, &found);

    // Nothing outside (or inside) the scan root was touched by discovery.
    assert_eq!(snapshot(&outside), outside_before);
    assert_eq!(snapshot(&scan_root), scan_root_before);

    // The same answer from inside the bare repository.
    assert_eq!(discover(&bare.join("refs")).unwrap(), found);
}

#[test]
fn json_output_round_trips() {
    let Some(sb) = Sandbox::new_with_git() else {
        return;
    };
    let repo = sb.init_repo("repo");
    sb.git(&repo, &["worktree", "add", "-q", "-b", "a", "../wt"]);
    sb.git(&repo, &["worktree", "lock", "--reason", "why", "../wt"]);
    let found = discover(&repo).unwrap();
    let json = serde_json::to_string(&found).unwrap();
    assert!(json.contains(r#""state":"locked""#), "{json}");
    assert!(json.contains(r#""reason":"why""#), "{json}");
    let back: RepoWorktrees = serde_json::from_str(&json).unwrap();
    assert_eq!(back, found);
}

#[test]
fn outside_any_repository() {
    let sb = Sandbox::new();
    let dir = sb.path().join("plain");
    fs::create_dir_all(&dir).unwrap();
    match discover(&dir) {
        Err(DiscoverError::NotARepository { .. }) => {}
        // Only acceptable if the temp dir itself lives inside some repository.
        Ok(found) => assert!(!canon(&found.common_dir.to_path_buf()).starts_with(sb.path())),
        Err(e) => panic!("unexpected error: {e}"),
    }
    assert!(matches!(
        discover(&sb.path().join("does-not-exist")),
        Err(DiscoverError::NotFound { .. })
    ));
}
