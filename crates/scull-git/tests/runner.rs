//! What [`GitRunner`] reports (the inputs to state analysis, GIT-02), against repositories built
//! with the real `git`. Skips (passes) when `git` is not installed.

mod common;

use common::{Sandbox, slashed};
use scull_git::{
    GitError, GitPath, GitRunner, IgnoredMode, ObjectId, StatusKind, StatusOptions, UntrackedMode,
};
use std::fs;

fn setup() -> Option<(Sandbox, GitRunner)> {
    let sb = Sandbox::new_with_git()?;
    Some((sb, GitRunner::new().expect("trusted git")))
}

fn oid(sb: &Sandbox, repo: &std::path::Path, rev: &str) -> ObjectId {
    ObjectId::parse(&sb.git(repo, &["rev-parse", rev])).expect("oid")
}

#[test]
fn resolved_binary_and_version_are_reported() {
    let Some((_sb, runner)) = setup() else { return };
    assert!(runner.resolved_binary().is_absolute());
    assert!(runner.resolved_binary().is_file());
    assert!(runner.version() >= scull_git::MIN_GIT_VERSION);
}

#[test]
fn status_reports_tracked_untracked_and_ignored() {
    let Some((sb, runner)) = setup() else { return };
    let repo = sb.init_repo("repo");
    fs::create_dir_all(repo.join("dir")).expect("mkdir");
    for (p, c) in [
        ("modified.txt", "1\n"),
        ("deleted.txt", "2\n"),
        ("staged.txt", "3\n"),
        ("dir/with space.txt", "4\n"),
        (".gitignore", "*.log\nnode_modules/\n.env\n"),
    ] {
        fs::write(repo.join(p), c).expect("write");
    }
    sb.git(&repo, &["add", "."]);
    sb.git(&repo, &["commit", "-q", "-m", "files"]);

    fs::write(repo.join("modified.txt"), "changed\n").expect("write");
    fs::remove_file(repo.join("deleted.txt")).expect("rm");
    fs::write(repo.join("staged.txt"), "staged\n").expect("write");
    sb.git(&repo, &["add", "staged.txt"]);
    fs::write(repo.join("dir/with space.txt"), "x\n").expect("write");
    fs::write(repo.join("new.txt"), "n\n").expect("write");
    fs::create_dir_all(repo.join("newdir")).expect("mkdir");
    fs::write(repo.join("newdir/a.txt"), "a\n").expect("write");
    fs::write(repo.join("newdir/b.txt"), "b\n").expect("write");
    fs::write(repo.join("café.txt"), "c\n").expect("write");
    fs::write(repo.join(".env"), "SECRET=1\n").expect("write");
    fs::write(repo.join("debug.log"), "l\n").expect("write");
    fs::create_dir_all(repo.join("node_modules/pkg")).expect("mkdir");
    fs::write(repo.join("node_modules/pkg/index.js"), "x\n").expect("write");

    let s = runner
        .status(&repo, &StatusOptions::default())
        .expect("status");
    let find = |p: &str| {
        s.entries
            .iter()
            .find(|e| e.path == GitPath::from(p))
            .unwrap_or_else(|| panic!("{p} missing from {s:#?}"))
    };
    let m = find("modified.txt");
    assert_eq!(
        (m.kind, m.index, m.worktree),
        (StatusKind::Changed, '.', 'M')
    );
    assert_eq!(find("deleted.txt").worktree, 'D');
    assert_eq!(find("staged.txt").index, 'M');
    assert_eq!(find("dir/with space.txt").worktree, 'M');
    assert_eq!(find("new.txt").kind, StatusKind::Untracked);
    assert_eq!(find("newdir/").kind, StatusKind::Untracked);
    assert_eq!(find("café.txt").kind, StatusKind::Untracked);
    assert_eq!(find(".env").kind, StatusKind::Ignored);
    assert_eq!(find("debug.log").kind, StatusKind::Ignored);
    assert_eq!(find("node_modules/").kind, StatusKind::Ignored);
    assert_eq!(s.tracked_changes().count(), 4);
    assert!(!s.is_clean());
    assert_eq!(s.branch.head.as_deref(), Some("main"));
    assert_eq!(s.branch.oid, Some(oid(&sb, &repo, "HEAD")));
    assert_eq!(s.branch.upstream, None);
    assert_eq!(
        find("dir/with space.txt").path.to_path(&repo),
        Some(repo.join("dir").join("with space.txt"))
    );

    let all = runner
        .status(
            &repo,
            &StatusOptions {
                untracked: UntrackedMode::All,
                ignored: IgnoredMode::No,
            },
        )
        .expect("status");
    let untracked: Vec<String> = all.untracked().map(|e| e.path.to_string_lossy()).collect();
    assert!(
        untracked.contains(&"newdir/a.txt".to_owned()),
        "{untracked:?}"
    );
    assert_eq!(all.ignored().count(), 0);

    let none = runner
        .status(
            &repo,
            &StatusOptions {
                untracked: UntrackedMode::No,
                ignored: IgnoredMode::No,
            },
        )
        .expect("status");
    assert_eq!(none.untracked().count(), 0);
    assert_eq!(none.tracked_changes().count(), 4);
}

#[test]
fn clean_repo_and_ignored_only_are_clean() {
    let Some((sb, runner)) = setup() else { return };
    let repo = sb.init_repo("repo");
    fs::write(repo.join(".gitignore"), ".env\n").expect("write");
    sb.git(&repo, &["add", "."]);
    sb.git(&repo, &["commit", "-q", "-m", "ignore"]);
    let s = runner
        .status(&repo, &StatusOptions::default())
        .expect("status");
    assert!(s.is_clean(), "{s:#?}");
    fs::write(repo.join(".env"), "x\n").expect("write");
    let s = runner
        .status(&repo, &StatusOptions::default())
        .expect("status");
    assert!(s.is_clean());
    assert_eq!(s.ignored().count(), 1);
}

#[test]
fn unborn_and_detached_heads() {
    let Some((sb, runner)) = setup() else { return };
    let root = sb.path();
    sb.git(&root, &["init", "-q", "unborn"]);
    let unborn = root.join("unborn");
    let s = runner
        .status(&unborn, &StatusOptions::default())
        .expect("status");
    assert_eq!(s.branch.oid, None);
    assert_eq!(s.branch.head.as_deref(), Some("main"));
    assert_eq!(runner.rev_parse(&unborn, "HEAD").expect("rev-parse"), None);

    let repo = sb.init_repo("repo");
    sb.git(&repo, &["checkout", "-q", "--detach"]);
    let s = runner
        .status(&repo, &StatusOptions::default())
        .expect("status");
    assert_eq!(s.branch.head, None);
    assert_eq!(s.branch.oid, Some(oid(&sb, &repo, "HEAD")));
}

#[test]
fn upstream_unpushed_commits_and_merge_status() {
    let Some((sb, runner)) = setup() else { return };
    let origin = sb.init_repo("origin");
    let root = sb.path();
    sb.git(&root, &["clone", "-q", &slashed(&origin), "clone"]);
    let clone = root.join("clone");
    let pushed = oid(&sb, &clone, "HEAD");

    assert_eq!(
        runner.upstream(&clone, "main").expect("upstream"),
        Some("refs/remotes/origin/main".to_owned())
    );
    assert_eq!(
        runner
            .upstream(&clone, "refs/heads/main")
            .expect("upstream"),
        Some("refs/remotes/origin/main".to_owned())
    );
    assert_eq!(runner.upstream(&clone, "nope").expect("upstream"), None);
    assert!(
        runner
            .commits_not_on_remotes(&clone, &pushed, None)
            .expect("rev-list")
            .is_empty()
    );

    // Two local commits on main, and a feature branch with no upstream.
    sb.git(&clone, &["commit", "-q", "--allow-empty", "-m", "local 1"]);
    sb.git(&clone, &["commit", "-q", "--allow-empty", "-m", "local 2"]);
    let tip = oid(&sb, &clone, "HEAD");
    sb.git(&clone, &["branch", "feature"]);
    let unpushed = runner
        .commits_not_on_remotes(&clone, &tip, None)
        .expect("rev-list");
    assert_eq!(unpushed.len(), 2);
    assert_eq!(unpushed[0], tip);
    assert_eq!(
        runner
            .commits_not_on_remotes(&clone, &tip, Some(1))
            .expect("rev-list")
            .len(),
        1
    );
    assert_eq!(runner.upstream(&clone, "feature").expect("upstream"), None);

    let s = runner
        .status(&clone, &StatusOptions::default())
        .expect("status");
    assert_eq!(s.branch.upstream.as_deref(), Some("origin/main"));
    assert_eq!((s.branch.ahead, s.branch.behind), (Some(2), Some(0)));

    assert!(runner.is_ancestor(&clone, &pushed, &tip).expect("ancestor"));
    assert!(!runner.is_ancestor(&clone, &tip, &pushed).expect("ancestor"));
    assert_eq!(
        runner
            .merge_base(&clone, &pushed, &tip)
            .expect("merge-base"),
        Some(pushed.clone())
    );

    let containing = runner
        .refs_containing(&clone, &pushed, &["refs/remotes/"])
        .expect("contains");
    assert!(
        containing
            .iter()
            .any(|r| r.name == "refs/remotes/origin/main")
    );
    assert!(
        runner
            .refs_containing(&clone, &tip, &["refs/remotes/"])
            .expect("contains")
            .is_empty()
    );
    let heads: Vec<String> = runner
        .refs_containing(&clone, &tip, &["refs/heads/"])
        .expect("contains")
        .into_iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(heads, ["refs/heads/feature", "refs/heads/main"]);

    let all = runner.for_each_ref(&clone, &[]).expect("refs");
    let origin_head = all
        .iter()
        .find(|r| r.name == "refs/remotes/origin/HEAD")
        .expect("origin/HEAD");
    assert_eq!(
        origin_head.symref.as_deref(),
        Some("refs/remotes/origin/main")
    );
    assert!(origin_head.is_remote_tracking());
    let main = all
        .iter()
        .find(|r| r.branch_name() == Some("main"))
        .expect("main");
    assert_eq!(main.oid, tip);
    assert_eq!(main.object_type, "commit");

    // Unrelated history has no merge base.
    sb.git(&clone, &["checkout", "-q", "--orphan", "orphan"]);
    sb.git(&clone, &["commit", "-q", "--allow-empty", "-m", "orphan"]);
    let orphan = oid(&sb, &clone, "HEAD");
    assert_eq!(
        runner
            .merge_base(&clone, &orphan, &tip)
            .expect("merge-base"),
        None
    );
    assert!(!runner.is_ancestor(&clone, &orphan, &tip).expect("ancestor"));
}

#[test]
fn linked_worktrees_and_bare_repos() {
    let Some((sb, runner)) = setup() else { return };
    let repo = sb.init_repo("repo");
    let wt = sb.path().join("wt");
    sb.git(
        &repo,
        &["worktree", "add", "-q", "-b", "wt-branch", &slashed(&wt)],
    );
    fs::write(wt.join("x.txt"), "x\n").expect("write");
    let s = runner
        .status(&wt, &StatusOptions::default())
        .expect("status");
    assert_eq!(s.branch.head.as_deref(), Some("wt-branch"));
    assert_eq!(s.untracked().count(), 1);
    // The main worktree is unaffected.
    let s = runner
        .status(&repo, &StatusOptions::default())
        .expect("status");
    assert!(s.is_clean());

    let root = sb.path();
    sb.git(
        &root,
        &["clone", "-q", "--bare", &slashed(&repo), "bare.git"],
    );
    let bare = root.join("bare.git");
    assert_eq!(
        runner.rev_parse(&bare, "HEAD").expect("rev-parse"),
        Some(oid(&sb, &repo, "HEAD"))
    );
    assert!(matches!(
        runner.status(&bare, &StatusOptions::default()),
        Err(GitError::NotARepository(_))
    ));
}

#[test]
fn bad_arguments_and_non_repositories_are_errors() {
    let Some((sb, runner)) = setup() else { return };
    let repo = sb.init_repo("repo");
    fs::create_dir_all(repo.join("sub")).expect("mkdir");
    let plain = sb.path().join("plain");
    fs::create_dir(&plain).expect("mkdir");

    assert!(matches!(
        runner.rev_parse(&repo, "--output=x"),
        Err(GitError::InvalidArgument(_))
    ));
    assert!(matches!(
        runner.for_each_ref(&repo, &["--contains=HEAD"]),
        Err(GitError::InvalidArgument(_))
    ));
    assert!(matches!(
        runner.for_each_ref(&repo, &["heads/"]),
        Err(GitError::InvalidArgument(_))
    ));
    assert_eq!(runner.rev_parse(&repo, "no-such-ref").expect("ok"), None);
    assert!(matches!(
        runner.status(&sb.path().join("missing"), &StatusOptions::default()),
        Err(GitError::NotADirectory(_))
    ));
    let e = runner
        .status(&plain, &StatusOptions::default())
        .expect_err("not a repo");
    assert!(matches!(e, GitError::NotARepository(_)), "{e:?}");
    // Discovery doesn't climb out of the folder given: a subfolder of a work tree is refused.
    let e = runner
        .status(&repo.join("sub"), &StatusOptions::default())
        .expect_err("subfolder");
    assert!(matches!(e, GitError::NotARepository(_)), "{e:?}");
}

#[test]
fn core_worktree_cannot_redirect_status() {
    let Some((sb, runner)) = setup() else { return };
    let repo = sb.init_repo("repo");
    let elsewhere = sb.path().join("elsewhere");
    fs::create_dir(&elsewhere).expect("mkdir");
    fs::write(elsewhere.join("victim.txt"), "v\n").expect("write");
    fs::write(repo.join("mine.txt"), "m\n").expect("write");
    sb.git(&repo, &["config", "core.worktree", &slashed(&elsewhere)]);
    let s = runner
        .status(&repo, &StatusOptions::default())
        .expect("status");
    let untracked: Vec<String> = s.untracked().map(|e| e.path.to_string_lossy()).collect();
    assert_eq!(untracked, ["mine.txt"]);
}

/// A config `include.path` pointing at a FIFO makes `git` block forever; the call must time
/// out instead of hanging the scan.
#[test]
fn hung_git_calls_time_out() {
    if !cfg!(unix) {
        return;
    }
    let Some(sb) = Sandbox::new_with_git() else {
        return;
    };
    let repo = sb.init_repo("repo");
    let fifo = sb.path().join("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("skipping: mkfifo unavailable");
        return;
    }
    sb.git(&repo, &["config", "include.path", &slashed(&fifo)]);
    let runner = GitRunner::with_options(scull_git::RunnerOptions {
        timeout: std::time::Duration::from_secs(2),
        ..Default::default()
    })
    .expect("trusted git");
    let started = std::time::Instant::now();
    let e = runner
        .status(&repo, &StatusOptions::default())
        .expect_err("must time out");
    assert!(matches!(e, GitError::Timeout { .. }), "{e:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}
