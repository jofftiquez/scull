//! FX-GIT-15 and friends: repository config must never execute code during a scan (SAFE-15,
//! T-21), and only a trusted `git` binary is ever run (SAFE-12, T-22).
//!
//! Each test plants a malicious config vector whose command creates a sentinel file in the
//! sandbox, runs every [`GitRunner`] call a scan makes, and asserts the sentinel was never
//! created while the scan still produced correct results. On Unix a control run of plain
//! (unhardened) `git` then proves the fixture is live: the same vector *does* create the
//! sentinel. Skips (passes) when `git` is not installed.

mod common;

use common::{Sandbox, slashed, snapshot};
use scull_git::{
    GitError, GitRunner, IgnoredMode, ObjectId, RunnerOptions, Status, StatusKind, StatusOptions,
    UntrackedMode,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A sandbox, a repository with tracked, untracked and ignored files, and a runner.
struct Fixture {
    sb: Sandbox,
    repo: PathBuf,
    runner: GitRunner,
    sentinels: PathBuf,
}

impl Fixture {
    fn new() -> Option<Self> {
        let sb = Sandbox::new_with_git()?;
        let repo = sb.init_repo("repo");
        fs::write(repo.join("tracked.txt"), "aaaa\n").expect("write");
        fs::write(repo.join("other.txt"), "cccc\n").expect("write");
        fs::write(repo.join(".gitignore"), "*.log\n").expect("write");
        sb.git(&repo, &["add", "."]);
        sb.git(&repo, &["commit", "-q", "-m", "files"]);
        let sentinels = sb.path().join("sentinels");
        fs::create_dir(&sentinels).expect("mkdir");
        let runner = GitRunner::new().expect("trusted git");
        Some(Self {
            sb,
            repo,
            runner,
            sentinels,
        })
    }

    /// A shell command that creates sentinel `name`.
    fn touch(&self, name: &str) -> String {
        format!("touch '{}'", slashed(&self.sentinels.join(name)))
    }

    /// Sets repository config with plain `git config` (which never runs repository code).
    fn config(&self, key: &str, value: &str) {
        self.sb.git(&self.repo, &["config", key, value]);
    }

    /// Makes the work tree dirty: same-size edits (so Git must hash the content, which is what
    /// runs clean filters), plus an untracked and an ignored file.
    fn dirty(&self) {
        fs::write(self.repo.join("tracked.txt"), "bbbb\n").expect("write");
        fs::write(self.repo.join("other.txt"), "dddd\n").expect("write");
        fs::write(self.repo.join("untracked.txt"), "u\n").expect("write");
        fs::write(self.repo.join("debug.log"), "i\n").expect("write");
    }

    fn sentinels(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.sentinels)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn assert_no_sentinel(&self) {
        assert_eq!(
            self.sentinels(),
            Vec::<String>::new(),
            "repository code ran"
        );
    }

    /// Runs every call a scan makes against `repo` and checks the scan completed correctly.
    fn scan(&self) -> Status {
        let status = scan_repo(&self.runner, &self.repo);
        let paths = |kind| {
            status
                .entries
                .iter()
                .filter(|e| e.kind == kind)
                .map(|e| e.path.to_string_lossy())
                .collect::<Vec<_>>()
        };
        let changed = paths(StatusKind::Changed);
        assert!(changed.contains(&"tracked.txt".to_owned()), "{status:#?}");
        assert!(changed.contains(&"other.txt".to_owned()), "{status:#?}");
        assert!(paths(StatusKind::Untracked).contains(&"untracked.txt".to_owned()));
        assert!(paths(StatusKind::Ignored).contains(&"debug.log".to_owned()));
        status
    }

    /// Control run (Unix): plain `git <args>` executes the planted vector.
    fn assert_control_fires(&self, args: &[&str]) {
        if !cfg!(unix) {
            return;
        }
        let _ = self.sb.git_output(&self.repo, args);
        assert!(
            !self.sentinels().is_empty(),
            "control: plain `git {args:?}` should have run the planted command; the fixture \
             is not exercising anything"
        );
    }
}

/// Every runner call a scan makes; returns the default status.
fn scan_repo(runner: &GitRunner, repo: &Path) -> Status {
    let all = StatusOptions {
        untracked: UntrackedMode::All,
        ignored: IgnoredMode::Matching,
    };
    let _ = runner.status(repo, &all).expect("status (all)");
    let status = runner
        .status(repo, &StatusOptions::default())
        .expect("status");
    let head = runner
        .rev_parse(repo, "HEAD^{commit}")
        .expect("rev-parse")
        .expect("HEAD resolves");
    let refs = runner.for_each_ref(repo, &[]).expect("for-each-ref");
    assert!(refs.iter().any(|r| r.name == "refs/heads/main"));
    runner
        .refs_containing(repo, &head, &["refs/heads/"])
        .expect("refs --contains");
    let _ = runner.upstream(repo, "main").expect("upstream");
    assert!(runner.is_ancestor(repo, &head, &head).expect("is-ancestor"));
    assert_eq!(
        runner.merge_base(repo, &head, &head).expect("merge-base"),
        Some(head.clone())
    );
    runner
        .commits_not_on_remotes(repo, &head, Some(100))
        .expect("rev-list");
    status
}

fn make_executable(p: &Path) {
    if cfg!(unix) {
        let ok = Command::new("chmod")
            .arg("+x")
            .arg(p)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "chmod +x {}", p.display());
    }
}

/// FX-GIT-15 itself: `core.fsmonitor` set to a command that creates a sentinel.
#[test]
fn fx_git_15_fsmonitor_command_never_runs() {
    let Some(fx) = Fixture::new() else { return };
    fx.config("core.fsmonitor", &fx.touch("fsmonitor"));
    fx.dirty();
    fx.scan();
    fx.assert_no_sentinel();
    fx.assert_control_fires(&["status"]);
}

/// `core.fsmonitor` and a filter driver set through `include.path` and `includeIf` files.
#[test]
fn fx_git_15_included_config_never_runs() {
    let Some(fx) = Fixture::new() else { return };
    let inc = fx.sb.path().join("evil.inc");
    let inc_if = fx.sb.path().join("evil-if.inc");
    fs::write(
        &inc,
        format!(
            "[core]\n\tfsmonitor = \"{}\"\n[include]\n\tpath = {}\n",
            fx.touch("include"),
            slashed(&inc_if)
        ),
    )
    .expect("write");
    fs::write(
        &inc_if,
        format!(
            "[filter \"incdrv\"]\n\tclean = \"{}; cat\"\n\trequired = true\n",
            fx.touch("include-filter")
        ),
    )
    .expect("write");
    // `gitdir:<repo>/` matches `<repo>/.git` (a trailing slash means "and everything inside").
    let repo = slashed(&fx.repo);
    fx.config(&format!("includeIf.gitdir:{repo}/.path"), &slashed(&inc));
    fs::write(fx.repo.join(".gitattributes"), "other.txt filter=incdrv\n").expect("write");
    fx.dirty();
    fx.scan();
    fx.assert_no_sentinel();
    fx.assert_control_fires(&["status"]);
}

/// A malicious `core.hooksPath`, and hooks in `.git/hooks`: nothing a scan does may run one.
/// (`post-index-change` is what a plain `git status` runs when it rewrites the index; the
/// control uses `commit`, which runs hooks deterministically.)
#[test]
fn fx_git_15_hooks_never_run() {
    let Some(fx) = Fixture::new() else { return };
    let evil_hooks = fx.sb.path().join("evil-hooks");
    fs::create_dir(&evil_hooks).expect("mkdir");
    let local_hooks = fx.repo.join(".git").join("hooks");
    fs::create_dir_all(&local_hooks).expect("mkdir");
    for name in [
        "post-index-change",
        "reference-transaction",
        "post-checkout",
        "pre-auto-gc",
        "fsmonitor-watchman",
        "pre-commit",
        "post-commit",
    ] {
        for (dir, tag) in [(&evil_hooks, "hookspath"), (&local_hooks, "local")] {
            let hook = dir.join(name);
            let body = format!("#!/bin/sh\n{}\n", fx.touch(&format!("{tag}-{name}")));
            fs::write(&hook, body).expect("write hook");
            make_executable(&hook);
        }
    }
    fx.config("core.hooksPath", &slashed(&evil_hooks));
    fx.dirty();
    fx.scan();
    fx.assert_no_sentinel();
    fx.assert_control_fires(&["commit", "-q", "--allow-empty", "-m", "control"]);
}

/// Clean/smudge/process filter drivers named by `.gitattributes`: `git status` runs the clean
/// filter (or long-running process) on modified files unless every driver is neutralized.
#[test]
fn fx_git_15_filter_drivers_never_run() {
    let Some(fx) = Fixture::new() else { return };
    for (driver, tag) in [("evil", "filter"), ("we.ird drv", "filter-weird")] {
        let section = format!("filter.{driver}");
        fx.config(
            &format!("{section}.clean"),
            &format!("{}; cat", fx.touch(&format!("{tag}-clean"))),
        );
        fx.config(
            &format!("{section}.smudge"),
            &format!("{}; cat", fx.touch(&format!("{tag}-smudge"))),
        );
        fx.config(&format!("{section}.required"), "true");
    }
    fx.config("filter.proc.process", &fx.touch("filter-process"));
    fs::write(
        fx.repo.join(".gitattributes"),
        "tracked.txt filter=evil\nother.txt filter=proc\n",
    )
    .expect("write");
    // Attributes from info/attributes are honoured too.
    let info = fx.repo.join(".git").join("info");
    fs::create_dir_all(&info).expect("mkdir");
    fs::write(info.join("attributes"), "untracked.txt filter=evil\n").expect("write");
    fx.dirty();
    fx.scan();
    fx.assert_no_sentinel();
    fx.assert_control_fires(&["status"]);
}

/// `diff.external`, `diff.<driver>.textconv` and `diff.<driver>.command`.
#[test]
fn fx_git_15_diff_drivers_never_run() {
    let Some(fx) = Fixture::new() else { return };
    fx.config("diff.external", &fx.touch("diff-external"));
    fx.config("diff.evil.textconv", &fx.touch("diff-textconv"));
    fx.config("diff.evil.command", &fx.touch("diff-command"));
    fx.config("core.pager", &fx.touch("pager"));
    fs::write(fx.repo.join(".gitattributes"), "*.txt diff=evil\n").expect("write");
    fx.dirty();
    fx.scan();
    fx.assert_no_sentinel();
    fx.assert_control_fires(&["diff"]);
}

/// `core.sshCommand`, `remote.*.uploadpack` and a promisor remote (lazy fetch of a missing
/// object): no network transport may ever start.
#[test]
fn fx_git_15_ssh_command_and_lazy_fetch_never_run() {
    let Some(fx) = Fixture::new() else { return };
    fx.config("core.sshCommand", &fx.touch("ssh"));
    fx.config("core.gitProxy", &fx.touch("proxy"));
    fx.config("remote.origin.url", "ssh://example.invalid/repo.git");
    fx.config("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*");
    fx.config("remote.origin.uploadpack", &fx.touch("uploadpack"));
    fx.config("remote.origin.promisor", "true");
    fx.config("remote.origin.partialclonefilter", "blob:none");
    fx.config("core.repositoryformatversion", "1");
    fx.config("extensions.partialClone", "origin");
    fx.config("branch.main.remote", "origin");
    fx.config("branch.main.merge", "refs/heads/main");
    fx.dirty();
    fx.scan();

    // Upstream comes from config, without contacting the remote.
    assert_eq!(
        fx.runner.upstream(&fx.repo, "main").expect("upstream"),
        Some("refs/remotes/origin/main".to_owned())
    );
    // A missing object would trigger a lazy fetch from the promisor remote.
    let missing = ObjectId::parse(&"1".repeat(40)).expect("oid");
    let rev = format!("{missing}^{{commit}}");
    let _ = fx.runner.rev_parse(&fx.repo, &rev);
    let head = fx
        .runner
        .rev_parse(&fx.repo, "HEAD")
        .expect("rev-parse")
        .expect("HEAD");
    let _ = fx.runner.is_ancestor(&fx.repo, &missing, &head);
    let _ = fx.runner.merge_base(&fx.repo, &missing, &head);
    fx.assert_no_sentinel();
    fx.assert_control_fires(&["cat-file", "-p", missing.as_str()]);
}

/// A submodule with its own malicious config: the superproject scan never enters it, and
/// scanning the submodule itself neutralizes its config like any other repository.
#[test]
fn fx_git_15_submodule_config_never_runs() {
    let Some(fx) = Fixture::new() else { return };
    let src = fx.sb.init_repo("subsrc");
    fs::write(src.join("s.txt"), "ssss\n").expect("write");
    fx.sb.git(&src, &["add", "."]);
    fx.sb.git(&src, &["commit", "-q", "-m", "s"]);
    let src_url = slashed(&src);
    fx.sb.git(
        &fx.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &src_url,
            "sub",
        ],
    );
    fx.sb.git(&fx.repo, &["commit", "-q", "-m", "add sub"]);
    let sub = fx.repo.join("sub");
    fx.sb.git(
        &sub,
        &["config", "core.fsmonitor", &fx.touch("sub-fsmonitor")],
    );
    fx.sb.git(
        &sub,
        &[
            "config",
            "filter.subdrv.clean",
            &format!("{}; cat", fx.touch("sub-filter")),
        ],
    );
    fs::write(sub.join(".gitattributes"), "s.txt filter=subdrv\n").expect("write");
    fs::write(sub.join("s.txt"), "tttt\n").expect("write");
    fs::write(sub.join("new.txt"), "n\n").expect("write");
    fx.dirty();

    fx.scan();
    let sub_status = fx
        .runner
        .status(&sub, &StatusOptions::default())
        .expect("submodule status");
    let changed: Vec<String> = sub_status
        .tracked_changes()
        .map(|e| e.path.to_string_lossy())
        .collect();
    assert!(changed.contains(&"s.txt".to_owned()), "{sub_status:#?}");
    assert!(
        sub_status
            .untracked()
            .any(|e| e.path.to_string_lossy() == "new.txt")
    );
    fx.assert_no_sentinel();
    fx.assert_control_fires(&["status"]);
}

/// A scan doesn't write to the repository: not the index, not config, not refs.
#[test]
fn fx_git_15_scan_leaves_repository_unchanged() {
    let Some(fx) = Fixture::new() else { return };
    fx.config("core.fsmonitor", &fx.touch("fsmonitor"));
    fx.config("core.untrackedCache", "true");
    fx.config("core.splitIndex", "true");
    fx.dirty();
    let before = snapshot(&fx.repo);
    fx.scan();
    assert!(
        before == snapshot(&fx.repo),
        "the scan modified the repository"
    );
    fx.assert_no_sentinel();
}

/// Fake `git` binaries planted in the repository, the current directory, `node_modules/.bin`
/// and as relative `PATH` entries are never resolved or run; with only those on `PATH`, no
/// binary is found at all.
#[test]
fn fx_git_15_untrusted_git_binaries_are_never_used() {
    let Some(fx) = Fixture::new() else { return };
    let real_dir = fx
        .runner
        .resolved_binary()
        .parent()
        .expect("parent")
        .to_path_buf();

    let exe = format!("git{}", std::env::consts::EXE_SUFFIX);
    let plant = |dir: &Path| {
        fs::create_dir_all(dir).expect("mkdir");
        let body = format!("#!/bin/sh\n{}\n", fx.touch("fake-git"));
        for name in [exe.as_str(), "git", "git.cmd", "git.bat"] {
            let p = dir.join(name);
            fs::write(&p, &body).expect("write fake git");
            make_executable(&p);
        }
    };
    let cwd = fx.sb.path().join("cwd");
    let nm = fx.sb.path().join("proj").join("node_modules").join(".bin");
    plant(&fx.repo);
    plant(&cwd);
    plant(&nm);
    plant(&cwd.join("bin"));

    let untrusted = [
        fx.repo.clone(),
        cwd.clone(),
        nm.clone(),
        PathBuf::from("bin"),
        PathBuf::from("."),
        PathBuf::new(),
    ];
    let mut entries = untrusted.to_vec();
    entries.push(real_dir.clone());
    let opts = |entries: &[PathBuf]| RunnerOptions {
        path_env: Some(std::env::join_paths(entries).expect("join PATH")),
        cwd: Some(cwd.clone()),
        excluded_roots: vec![fx.repo.clone()],
        ..RunnerOptions::default()
    };

    let runner = GitRunner::with_options(opts(&entries)).expect("trusted git");
    assert_eq!(
        runner.resolved_binary(),
        fx.runner.resolved_binary(),
        "a planted git was resolved"
    );
    assert!(runner.resolved_binary().is_absolute());
    fx.dirty();
    scan_repo(&runner, &fx.repo);
    fx.assert_no_sentinel();

    // Only untrusted locations on PATH: refuse rather than fall back.
    let err = GitRunner::with_options(opts(&untrusted)).expect_err("must not resolve");
    assert_eq!(err, GitError::NotFound);
    fx.assert_no_sentinel();
}
