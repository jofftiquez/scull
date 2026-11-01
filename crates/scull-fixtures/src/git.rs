//! Git repositories built with the real `git` binary in an isolated environment.
//!
//! Isolation: every `GIT_*` variable inherited from the parent is removed, then
//! `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=<git-home>/.gitconfig`, `HOME`/`XDG_CONFIG_HOME`
//! pointing inside the fixture's temporary directory, `GIT_TERMINAL_PROMPT=0`, and fixed author
//! and committer identity and dates (so commit IDs are deterministic). Every invocation also gets
//! `-c init.defaultBranch=main -c user.name=… -c user.email=… -c commit.gpgsign=false
//! -c tag.gpgsign=false -c core.autocrlf=false -c core.fsmonitor=false -c gc.auto=0
//! -c maintenance.auto=false`, so neither the user's
//! configuration nor a fixture's (possibly malicious, FX-GIT-15) repo config can make the
//! *builder* run hooks or helpers.

use crate::error::{FixtureError, Result};
use crate::fixture::Fixture;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Author/committer name used for fixture commits.
pub const GIT_USER_NAME: &str = "scull fixtures";
/// Author/committer email used for fixture commits.
pub const GIT_USER_EMAIL: &str = "fixtures@scull.invalid";
/// Default author/committer date for fixture commits (override per command via
/// [`GitRepo::command`] and `GIT_AUTHOR_DATE` / `GIT_COMMITTER_DATE`).
pub const GIT_DATE: &str = "2026-01-01T00:00:00+0000";

const CONFIG: &[&str] = &[
    "init.defaultBranch=main",
    "commit.gpgsign=false",
    "tag.gpgsign=false",
    "core.autocrlf=false",
    "core.fsmonitor=false",
    // No automatic gc/maintenance: newer Git runs `maintenance run --auto --detach` after a
    // commit, and the detached process writes `.git/objects/maintenance.lock` (and may repack)
    // after the fixture's I-3 baseline, failing it at random.
    "gc.auto=0",
    "maintenance.auto=false",
];

/// A Git repository inside a fixture.
#[derive(Debug, Clone)]
pub struct GitRepo {
    dir: PathBuf,
    home: PathBuf,
}

impl GitRepo {
    /// The repository's working directory (absolute).
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// An isolated `git` command (see module docs) with `args`, run in the repository.
    pub fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        isolated_git(&self.home, &self.dir, args)
    }

    /// Run `git <args>` in the repository and return trimmed standard output.
    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run(self.command(args))
    }

    /// `git add -A && git commit -m <message>`; returns the new commit ID.
    pub fn commit_all(&self, message: &str) -> Result<String> {
        self.run(["add", "-A"])?;
        self.run(["commit", "--quiet", "--allow-empty", "-m", message])?;
        self.run(["rev-parse", "HEAD"])
    }
}

impl Fixture {
    /// Create directory `rel` (relative to the root) and `git init` it on branch `main`.
    pub fn git_repo(&mut self, rel: impl AsRef<Path>) -> Result<GitRepo> {
        let rel = rel.as_ref();
        self.dir(rel)?;
        let repo = GitRepo {
            dir: self.path(rel)?,
            home: self.git_home()?,
        };
        repo.run(["init", "--quiet"])?;
        Ok(repo)
    }

    /// An isolated `git` command run in `rel` (relative to the root), for repositories that
    /// were not created by [`Fixture::git_repo`] (e.g. a clone or worktree target).
    pub fn git_command<I, S>(&self, rel: impl AsRef<Path>, args: I) -> Result<Command>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(isolated_git(&self.git_home()?, &self.path(rel)?, args))
    }
}

fn isolated_git<I, S>(home: &Path, cwd: &Path, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut c = Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().to_ascii_uppercase().starts_with("GIT_") {
            c.env_remove(k);
        }
    }
    c.current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", GIT_USER_NAME)
        .env("GIT_AUTHOR_EMAIL", GIT_USER_EMAIL)
        .env("GIT_COMMITTER_NAME", GIT_USER_NAME)
        .env("GIT_COMMITTER_EMAIL", GIT_USER_EMAIL)
        .env("GIT_AUTHOR_DATE", GIT_DATE)
        .env("GIT_COMMITTER_DATE", GIT_DATE)
        .env("LC_ALL", "C");
    c.arg("-c").arg(format!("user.name={GIT_USER_NAME}"));
    c.arg("-c").arg(format!("user.email={GIT_USER_EMAIL}"));
    for kv in CONFIG {
        c.arg("-c").arg(kv);
    }
    c.args(args);
    c
}

fn run(mut c: Command) -> Result<String> {
    let command = format!("{c:?}");
    let out = c.output().map_err(|e| FixtureError::Tool {
        command: command.clone(),
        status: format!("spawn failed: {e}"),
        stderr: String::new(),
    })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(FixtureError::Tool {
            command,
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        })
    }
}
