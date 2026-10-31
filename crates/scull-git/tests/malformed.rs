//! Discovery against hand-crafted (malformed or malicious) Git metadata. No `git` binary needed.
//!
//! A crafted repository controls every byte of its metadata (THREAT_MODEL S2). Discovery must
//! never panic, never follow a pointer it can't validate, and never present an unrelated folder
//! as a worktree.

// Fixture-building helpers outside `#[test]` fns; `allow-unwrap-in-tests` doesn't reach them.
#![allow(clippy::unwrap_used)]

mod common;

use common::{Sandbox, canon, slashed, snapshot};
use scull_git::{DiscoverError, LockState, ProblemKind, WorktreeHead, WorktreeKind, discover};
use std::fs;
use std::path::{Path, PathBuf};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// Minimal non-bare repository at `dir` (`.git/{HEAD,config,objects,refs}`); returns `.git`.
fn fake_repo(dir: &Path) -> PathBuf {
    let git = dir.join(".git");
    fs::create_dir_all(git.join("objects")).unwrap();
    fs::create_dir_all(git.join("refs/heads")).unwrap();
    fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(git.join("config"), "[core]\n\tbare = false\n").unwrap();
    git
}

/// Registers a linked worktree by hand, the way `git worktree add` lays it out.
fn fake_linked(common: &Path, name: &str, wt: &Path) -> PathBuf {
    let admin = common.join("worktrees").join(name);
    fs::create_dir_all(&admin).unwrap();
    fs::write(admin.join("commondir"), "../..\n").unwrap();
    fs::write(admin.join("HEAD"), format!("{SHA}\n")).unwrap();
    fs::write(
        admin.join("gitdir"),
        format!("{}\n", slashed(&wt.join(".git"))),
    )
    .unwrap();
    fs::create_dir_all(wt).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}\n", slashed(&admin))).unwrap();
    admin
}

#[test]
fn hand_made_layout_with_forward_slashes_is_discovered() {
    let sb = Sandbox::new();
    let repo = sb.path().join("repo");
    let common = fake_repo(&repo);
    let wt = sb.path().join("wt");
    fake_linked(&common, "wt", &wt);

    let found = discover(&wt).unwrap();
    assert_eq!(canon(&found.common_dir.to_path_buf()), common);
    assert!(found.problems.is_empty(), "{:#?}", found.problems);
    assert_eq!(found.worktrees.len(), 2);
    assert_eq!(found.worktrees[1].kind, WorktreeKind::Linked);
    assert_eq!(canon(&found.worktrees[1].path.to_path_buf()), wt);
    assert_eq!(
        found.worktrees[1].head,
        WorktreeHead::Detached { oid: SHA.into() }
    );
}

#[test]
fn relative_paths_are_resolved() {
    // Git 2.48+ `worktree.useRelativePaths` writes relative links in both directions.
    let sb = Sandbox::new();
    let repo = sb.path().join("repo");
    let common = fake_repo(&repo);
    let wt = sb.path().join("wt");
    let admin = fake_linked(&common, "wt", &wt);
    fs::write(admin.join("gitdir"), "../../../../wt/.git\n").unwrap();
    fs::write(wt.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();

    let found = discover(&wt).unwrap();
    assert!(found.problems.is_empty(), "{:#?}", found.problems);
    assert_eq!(canon(&found.worktrees[1].path.to_path_buf()), wt);
}

#[test]
fn crafted_gitdir_naming_an_unrelated_repository_is_a_problem() {
    let sb = Sandbox::new();
    let repo = sb.path().join("repo");
    let common = fake_repo(&repo);
    // The victim is an independent project with its own `.git` folder.
    let victim = sb.path().join("victim");
    fake_repo(&victim);
    fs::write(victim.join("precious.txt"), "do not touch").unwrap();
    let admin = common.join("worktrees/evil");
    fs::create_dir_all(&admin).unwrap();
    fs::write(admin.join("commondir"), "../..").unwrap();
    fs::write(admin.join("HEAD"), "ref: refs/heads/main").unwrap();
    fs::write(admin.join("gitdir"), slashed(&victim.join(".git"))).unwrap();

    let before = snapshot(&victim);
    let found = discover(&repo).unwrap();
    assert_eq!(
        found.worktrees.len(),
        1,
        "victim must not be listed: {found:#?}"
    );
    assert_eq!(found.problems.len(), 1);
    let p = &found.problems[0];
    assert_eq!(p.name, "evil");
    assert_eq!(p.kind, ProblemKind::BackLinkMismatch);
    assert_eq!(
        canon(&p.claimed_path.as_ref().unwrap().to_path_buf()),
        victim
    );
    assert_eq!(snapshot(&victim), before);
}

#[test]
fn crafted_gitdir_naming_a_plain_folder_is_a_problem_not_prunable() {
    let sb = Sandbox::new();
    let repo = sb.path().join("repo");
    let common = fake_repo(&repo);
    let victim = sb.path().join("photos");
    fs::create_dir_all(&victim).unwrap();
    let admin = common.join("worktrees/evil");
    fs::create_dir_all(&admin).unwrap();
    fs::write(admin.join("commondir"), "../..").unwrap();
    fs::write(admin.join("HEAD"), "ref: refs/heads/main").unwrap();
    fs::write(admin.join("gitdir"), slashed(&victim.join(".git"))).unwrap();

    let found = discover(&repo).unwrap();
    assert_eq!(found.worktrees.len(), 1, "{found:#?}");
    assert_eq!(found.problems[0].kind, ProblemKind::BackLinkMismatch);
}

#[test]
fn worktree_linking_to_another_admin_dir_is_a_problem() {
    let sb = Sandbox::new();
    let repo = sb.path().join("repo");
    let common = fake_repo(&repo);
    let a = sb.path().join("a");
    let b = sb.path().join("b");
    fake_linked(&common, "a", &a);
    let admin_b = fake_linked(&common, "b", &b);
    // `a/.git` now claims to be `b`.
    fs::write(a.join(".git"), format!("gitdir: {}\n", slashed(&admin_b))).unwrap();

    let found = discover(&repo).unwrap();
    let names: Vec<_> = found
        .worktrees
        .iter()
        .filter_map(|w| w.name.clone())
        .collect();
    assert_eq!(names, vec!["b".to_owned()]);
    assert_eq!(found.problems.len(), 1);
    assert_eq!(found.problems[0].name, "a");
    assert_eq!(found.problems[0].kind, ProblemKind::BackLinkMismatch);
}

#[test]
fn commondir_pointing_elsewhere_is_a_problem() {
    let sb = Sandbox::new();
    let repo = sb.path().join("repo");
    let common = fake_repo(&repo);
    let other = fake_repo(&sb.path().join("other"));
    let wt = sb.path().join("wt");
    let admin = fake_linked(&common, "wt", &wt);
    fs::write(admin.join("commondir"), slashed(&other)).unwrap();

    let found = discover(&repo).unwrap();
    assert_eq!(found.worktrees.len(), 1);
    assert_eq!(found.problems[0].kind, ProblemKind::CommonDirMismatch);

    // Starting from the worktree, its admin dir no longer leads back to its container.
    assert!(matches!(
        discover(&wt),
        Err(DiscoverError::Malformed { .. })
    ));
}

#[test]
fn missing_empty_oversized_and_non_file_metadata_are_problems() {
    let sb = Sandbox::new();
    let repo = sb.path().join("repo");
    let common = fake_repo(&repo);

    let a = fake_linked(&common, "a-missing", &sb.path().join("a"));
    fs::remove_file(a.join("gitdir")).unwrap();
    let b = fake_linked(&common, "b-empty", &sb.path().join("b"));
    fs::write(b.join("gitdir"), "\n").unwrap();
    let c = fake_linked(&common, "c-huge", &sb.path().join("c"));
    fs::write(c.join("gitdir"), vec![b'x'; 2 * 1024 * 1024]).unwrap();
    let d = fake_linked(&common, "d-dir", &sb.path().join("d"));
    fs::remove_file(d.join("gitdir")).unwrap();
    fs::create_dir(d.join("gitdir")).unwrap();
    let e = fake_linked(&common, "e-nocommondir", &sb.path().join("e"));
    fs::remove_file(e.join("commondir")).unwrap();
    let f = fake_linked(&common, "f-notdotgit", &sb.path().join("f"));
    fs::write(f.join("gitdir"), slashed(&sb.path().join("f"))).unwrap();
    let g = fake_linked(&common, "g-binary", &sb.path().join("g"));
    fs::write(g.join("gitdir"), [0xff, 0xfe, 0x00]).unwrap();
    // A stray file in `worktrees/` is ignored, like Git does.
    fs::write(common.join("worktrees/README"), "hi").unwrap();

    let found = discover(&repo).unwrap();
    assert_eq!(found.worktrees.len(), 1, "{found:#?}");
    let kinds: Vec<_> = found
        .problems
        .iter()
        .map(|p| (p.name.as_str(), p.kind))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("a-missing", ProblemKind::MissingFile),
            ("b-empty", ProblemKind::Malformed),
            ("c-huge", ProblemKind::Malformed),
            ("d-dir", ProblemKind::Malformed),
            ("e-nocommondir", ProblemKind::MissingFile),
            ("f-notdotgit", ProblemKind::Malformed),
            ("g-binary", ProblemKind::Malformed),
        ]
    );
}

#[test]
fn garbage_head_and_lock_are_reported_protectively() {
    let sb = Sandbox::new();
    let repo = sb.path().join("repo");
    let common = fake_repo(&repo);
    let wt = sb.path().join("wt");
    let admin = fake_linked(&common, "wt", &wt);
    fs::write(admin.join("HEAD"), "not a head").unwrap();
    // A `locked` folder instead of a file still means locked (Git only checks existence).
    fs::create_dir(admin.join("locked")).unwrap();

    let found = discover(&wt).unwrap();
    let w = &found.worktrees[1];
    assert!(matches!(w.head, WorktreeHead::Invalid { .. }));
    assert_eq!(w.lock, LockState::Locked { reason: None });
}

#[test]
fn dot_git_file_pointing_at_arbitrary_folders_is_rejected() {
    let sb = Sandbox::new();
    let wt = sb.path().join("wt");
    fs::create_dir_all(&wt).unwrap();
    let elsewhere = sb.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();

    for content in [
        "nonsense".to_owned(),
        "gitdir:".to_owned(),
        format!("gitdir: {}", slashed(&elsewhere)),
        format!("gitdir: {}", slashed(&sb.path().join("missing"))),
    ] {
        fs::write(wt.join(".git"), &content).unwrap();
        let r = discover(&wt);
        assert!(
            matches!(r, Err(DiscoverError::Malformed { .. })),
            "content {content:?} gave {r:?}"
        );
    }

    // A fake admin dir that isn't inside a `worktrees/` folder of a Git dir.
    let fake_admin = elsewhere.join("admin");
    fs::create_dir_all(&fake_admin).unwrap();
    fs::write(fake_admin.join("commondir"), "..").unwrap();
    fs::write(fake_admin.join("gitdir"), slashed(&wt.join(".git"))).unwrap();
    fs::write(fake_admin.join("HEAD"), SHA).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}", slashed(&fake_admin))).unwrap();
    assert!(matches!(
        discover(&wt),
        Err(DiscoverError::Malformed { .. })
    ));

    // `.git` is a folder that isn't a Git dir.
    let other = sb.path().join("other");
    fs::create_dir_all(other.join(".git")).unwrap();
    assert!(matches!(
        discover(&other),
        Err(DiscoverError::Malformed { .. })
    ));
}

#[test]
fn separate_git_dir_layout_is_followed() {
    // `git init --separate-git-dir` and submodules: `.git` file points straight at a Git dir.
    let sb = Sandbox::new();
    let store = sb.path().join("store");
    let gitdir = fake_repo(&store);
    let wt = sb.path().join("work");
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".git"), format!("gitdir: {}\n", slashed(&gitdir))).unwrap();
    let found = discover(&wt).unwrap();
    assert_eq!(canon(&found.common_dir.to_path_buf()), gitdir);
}

#[test]
fn bare_detection_without_config() {
    let sb = Sandbox::new();
    let bare = sb.path().join("thing.git");
    fs::create_dir_all(bare.join("objects")).unwrap();
    fs::create_dir_all(bare.join("refs")).unwrap();
    fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let found = discover(&bare).unwrap();
    assert!(found.bare);
    assert_eq!(found.worktrees[0].kind, WorktreeKind::Bare);
    assert_eq!(canon(&found.worktrees[0].path.to_path_buf()), bare);
}
