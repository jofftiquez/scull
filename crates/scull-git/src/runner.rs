//! Hardened execution of the system `git` binary (SAFE-12, SAFE-15, T-21, T-22).
//!
//! Repository config is **untrusted input**: a cloned repository can set `core.fsmonitor`,
//! hooks, filter drivers, `diff.external`, `core.sshCommand`, promisor remotes and more, each of
//! which makes a naive `git status` run an arbitrary command (FX-GIT-15). Every call made by
//! [`GitRunner`] is locked down as follows.
//!
//! **Binary.** Resolved once from trusted `PATH` entries only (see `resolve.rs`): never the
//! current directory, relative or empty entries, `node_modules`, or the scan roots. Spawned by
//! absolute path with [`std::process::Command`]: no shell.
//!
//! **Environment.** Cleared, then only: `PATH` (the trusted entries), `HOME`,
//! `XDG_CONFIG_HOME`, `TMPDIR`/`TEMP`/`TMP` (empty private temp dirs), `GIT_CONFIG_NOSYSTEM=1`,
//! `GIT_CONFIG_GLOBAL=<empty file>`, `GIT_ATTR_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`,
//! `GIT_OPTIONAL_LOCKS=0`, `GIT_NO_REPLACE_OBJECTS=1`, `GIT_NO_LAZY_FETCH=1`,
//! `GIT_PROTOCOL_FROM_USER=0`, `GIT_PAGER=cat`, `LC_ALL=C` (and `SystemRoot` on Windows).
//! System and global config, `GIT_DIR`-style overrides and `GIT_SSH*` never reach Git.
//!
//! **Working directory.** An empty private folder; the repository is passed with `-C`.
//! `GIT_CEILING_DIRECTORIES` stops discovery from climbing above the given folder.
//!
//! **Forced config** (`-c`, which takes precedence over every config file, including
//! `include`/`includeIf` files): `core.fsmonitor=false`, `core.hooksPath=<empty dir>`,
//! `core.untrackedCache=false`, `core.attributesFile=<empty file>`, `core.askPass=`,
//! `core.pager=cat`, `credential.helper=`, `diff.external=`, `protocol.allow=never` (no
//! transport of any kind, so no fetch, lazy fetch, `core.sshCommand` or remote helper),
//! `gc.auto=0`, `maintenance.auto=false`, `log.showSignature=false`,
//! `status.submoduleSummary=false`, `submodule.recurse=false`. Global options
//! `--no-pager --no-optional-locks --no-replace-objects` are always passed.
//!
//! **Filter drivers.** `git status` runs `filter.<driver>.clean` / `.process` on modified files
//! whose `.gitattributes` name a driver. There is no switch to disable filters, so before every
//! `status` the driver names are enumerated with `git config --list --name-only -z` (a read-only
//! command that resolves includes) and each is neutralized with
//! `-c filter.<driver>.clean= -c filter.<driver>.smudge= -c filter.<driver>.process=
//! -c filter.<driver>.required=false`. A driver name that can't be expressed safely as a `-c`
//! key is refused ([`GitError::UnsafeConfig`]) instead of risking execution. With the filter
//! disabled Git hashes the raw file, so a filtered file may be reported as modified when it
//! isn't (for example Git LFS files): the error is on the safe side.
//!
//! **Submodules.** `status` passes `--ignore-submodules=dirty`, so Git never spawns a second
//! Git inside a submodule (whose config was not inspected). A changed submodule commit is still
//! reported; analyse each submodule's work tree with its own `status` call.
//!
//! **Commands.** Only read-only plumbing is run: `status`, `rev-parse`, `for-each-ref`,
//! `merge-base`, `rev-list`, `cherry`, `ls-files`, `config --list`. None writes to the repository
//! (`GIT_OPTIONAL_LOCKS=0` stops `status` from refreshing the index), none touches the network.
//! Every call has a timeout and an output cap.
//!
//! **`safe.directory`.** Git's ownership check is left in force: a repository owned by another
//! user fails with [`GitError::DubiousOwnership`] (the repository can't opt itself in, since
//! `safe.directory` is only read from system/global config, which is disabled).

use crate::refs::{self, CherryCommit, ObjectId, REF_FORMAT, RefEntry};
use crate::resolve;
use crate::status::{self, GitPath, IgnoredMode, Status, StatusOptions, UntrackedMode};
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Oldest Git supported (porcelain v2 `--no-renames`, `--no-optional-locks`,
/// `for-each-ref --contains`).
pub const MIN_GIT_VERSION: GitVersion = GitVersion {
    major: 2,
    minor: 20,
    patch: 0,
};

/// Default per-call timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Default cap on a call's stdout.
pub const DEFAULT_MAX_OUTPUT: usize = 256 * 1024 * 1024;

const MAX_STDERR: usize = 64 * 1024;

/// A parsed `git --version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitVersion {
    /// Major version.
    pub major: u32,
    /// Minor version.
    pub minor: u32,
    /// Patch version.
    pub patch: u32,
}

impl GitVersion {
    /// Parses `git version 2.43.0`, `git version 2.45.1.windows.1`,
    /// `git version 2.39.5 (Apple Git-154)`.
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.trim().strip_prefix("git version ")?;
        let mut nums = rest
            .split(|c: char| !c.is_ascii_digit())
            .map(|n| n.parse::<u32>());
        let major = nums.next()?.ok()?;
        let minor = nums.next()?.ok()?;
        let patch = nums.next().and_then(Result::ok).unwrap_or(0);
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for GitVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Why a Git call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GitError {
    /// No `git` binary in a trusted location.
    NotFound,
    /// The resolved `git` is older than [`MIN_GIT_VERSION`], or its version couldn't be read.
    UnsupportedVersion {
        /// The binary.
        binary: PathBuf,
        /// What `git --version` printed.
        reported: String,
    },
    /// The private sandbox (empty `HOME`, hooks dir, ...) couldn't be created.
    Sandbox(String),
    /// An argument was refused (it could be read as an option or is malformed).
    InvalidArgument(String),
    /// The path is not a folder.
    NotADirectory(PathBuf),
    /// The folder is not the top of a Git work tree or a Git dir.
    NotARepository(PathBuf),
    /// Git refused the repository because it is owned by someone else (`safe.directory`).
    DubiousOwnership(PathBuf),
    /// The repository's config contains something that can't be neutralized safely.
    UnsafeConfig(String),
    /// Spawning or waiting for `git` failed.
    Io(String),
    /// The call didn't finish within the timeout and was killed.
    Timeout {
        /// The Git subcommand.
        command: String,
        /// The timeout.
        after: Duration,
    },
    /// The call printed more than the output cap.
    OutputTooLarge {
        /// The Git subcommand.
        command: String,
    },
    /// `git` exited unsuccessfully.
    Failed {
        /// The Git subcommand.
        command: String,
        /// Exit code, if any.
        code: Option<i32>,
        /// Standard error (truncated).
        stderr: String,
    },
    /// The output couldn't be parsed.
    Parse(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::NotFound => write!(f, "no trusted `git` binary found on PATH"),
            GitError::UnsupportedVersion { binary, reported } => write!(
                f,
                "{}: unsupported Git ({}); need {MIN_GIT_VERSION} or newer",
                binary.display(),
                reported.trim()
            ),
            GitError::Sandbox(e) => write!(f, "can't create Git sandbox: {e}"),
            GitError::InvalidArgument(e) => write!(f, "invalid Git argument: {e}"),
            GitError::NotADirectory(p) => write!(f, "{}: not a folder", p.display()),
            GitError::NotARepository(p) => write!(f, "{}: not a Git repository", p.display()),
            GitError::DubiousOwnership(p) => write!(
                f,
                "{}: owned by another user; Git refuses it (safe.directory)",
                p.display()
            ),
            GitError::UnsafeConfig(e) => write!(f, "repository config can't be made safe: {e}"),
            GitError::Io(e) => write!(f, "running git: {e}"),
            GitError::Timeout { command, after } => {
                write!(f, "git {command} timed out after {after:?}")
            }
            GitError::OutputTooLarge { command } => write!(f, "git {command}: output too large"),
            GitError::Failed {
                command,
                code,
                stderr,
            } => write!(
                f,
                "git {command} failed ({}): {}",
                code.map_or_else(|| "killed".to_owned(), |c| format!("exit {c}")),
                stderr.trim()
            ),
            GitError::Parse(e) => write!(f, "unexpected git output: {e}"),
        }
    }
}

impl std::error::Error for GitError {}

/// How to build a [`GitRunner`].
#[derive(Debug, Clone)]
pub struct RunnerOptions {
    /// The `PATH` to search; `None` uses this process's `PATH`.
    pub path_env: Option<OsString>,
    /// The current directory to exclude from the search; `None` uses this process's.
    pub cwd: Option<PathBuf>,
    /// Folders whose contents are never trusted to provide `git`: the scan roots and any
    /// repository being analysed.
    pub excluded_roots: Vec<PathBuf>,
    /// Per-call timeout.
    pub timeout: Duration,
    /// Cap on a call's stdout.
    pub max_output: usize,
}

impl Default for RunnerOptions {
    fn default() -> Self {
        Self {
            path_env: None,
            cwd: None,
            excluded_roots: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }
}

/// Runs the system `git` with repository config treated as untrusted. See the module docs.
#[derive(Debug)]
pub struct GitRunner {
    binary: PathBuf,
    version: GitVersion,
    search_path: OsString,
    passthrough: Vec<(OsString, OsString)>,
    timeout: Duration,
    max_output: usize,
    sandbox: Sandbox,
}

/// Private, empty folders and files backing the sanitized environment.
#[derive(Debug)]
struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    hooks: PathBuf,
    cwd: PathBuf,
    tmp: PathBuf,
    empty_file: PathBuf,
}

impl Sandbox {
    fn new() -> io::Result<Self> {
        let dir = tempfile::Builder::new().prefix("scull-git-").tempdir()?;
        let root = resolve::strip_verbatim(fs::canonicalize(dir.path())?);
        let sub = |name: &str| -> io::Result<PathBuf> {
            let p = root.join(name);
            fs::create_dir(&p)?;
            Ok(p)
        };
        let (home, hooks, cwd, tmp) = (sub("home")?, sub("hooks")?, sub("cwd")?, sub("tmp")?);
        let empty_file = root.join("empty");
        fs::write(&empty_file, b"")?;
        Ok(Self {
            _dir: dir,
            home,
            hooks,
            cwd,
            tmp,
            empty_file,
        })
    }
}

/// Captured result of one call.
struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl Output {
    fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl GitRunner {
    /// Resolves `git` from this process's `PATH` (trusted entries only) and checks its version.
    pub fn new() -> Result<Self, GitError> {
        Self::with_options(RunnerOptions::default())
    }

    /// Like [`GitRunner::new`] with explicit options. Pass the scan roots as
    /// [`RunnerOptions::excluded_roots`].
    pub fn with_options(opts: RunnerOptions) -> Result<Self, GitError> {
        let path_env = opts
            .path_env
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_default();
        let cwd = opts.cwd.or_else(|| std::env::current_dir().ok());
        let resolved = resolve::resolve(&path_env, cwd.as_deref(), &opts.excluded_roots);
        let binary = resolved.binary.ok_or(GitError::NotFound)?;
        let sandbox = Sandbox::new().map_err(|e| GitError::Sandbox(e.to_string()))?;

        let mut passthrough = Vec::new();
        if cfg!(windows) {
            // Windows processes need SystemRoot to load system DLLs (Winsock, crypto).
            if let Some(v) = std::env::var_os("SystemRoot") {
                passthrough.push((OsString::from("SystemRoot"), v));
            }
        }

        let mut runner = Self {
            binary,
            version: MIN_GIT_VERSION,
            search_path: resolve::join_dirs(&resolved.trusted_dirs),
            passthrough,
            timeout: opts.timeout,
            max_output: opts.max_output,
            sandbox,
        };
        let out = runner.exec(None, &[], &["--version".into()], "--version")?;
        let reported = String::from_utf8_lossy(&out.stdout).into_owned();
        match GitVersion::parse(&reported) {
            Some(v) if out.success() && v >= MIN_GIT_VERSION => runner.version = v,
            _ => {
                return Err(GitError::UnsupportedVersion {
                    binary: runner.binary,
                    reported,
                });
            }
        }
        Ok(runner)
    }

    /// Absolute path of the `git` binary that every call runs (show it in previews, SAFE-12).
    pub fn resolved_binary(&self) -> &Path {
        &self.binary
    }

    /// The binary's version.
    pub fn version(&self) -> GitVersion {
        self.version
    }

    /// `git status` for the work tree whose top folder is `worktree`, with filter drivers
    /// neutralized and submodules not entered. Renames are not detected (a rename is a
    /// deletion plus an addition).
    ///
    /// `worktree` must contain the `.git` folder or file: the work tree is always pinned to
    /// it, so `core.worktree` in repository config can't redirect status to another folder,
    /// and a bare repository or a Git dir is refused rather than scanned as a work tree.
    pub fn status(&self, worktree: &Path, opts: &StatusOptions) -> Result<Status, GitError> {
        let repo = repo_dir(worktree)?;
        if fs::symlink_metadata(repo.join(".git")).is_err() {
            return Err(GitError::NotARepository(worktree.to_path_buf()));
        }
        let extra = self.filter_overrides(&repo)?;
        let mut args: Vec<OsString> = vec![
            {
                let mut w = OsString::from("--work-tree=");
                w.push(&repo);
                w
            },
            "status".into(),
            "--porcelain=v2".into(),
            "-z".into(),
            "--branch".into(),
            "--no-renames".into(),
            "--ignore-submodules=dirty".into(),
        ];
        args.push(
            match opts.untracked {
                UntrackedMode::No => "--untracked-files=no",
                UntrackedMode::Normal => "--untracked-files=normal",
                UntrackedMode::All => "--untracked-files=all",
            }
            .into(),
        );
        args.push(
            match opts.ignored {
                IgnoredMode::No => "--ignored=no",
                IgnoredMode::Traditional => "--ignored=traditional",
                IgnoredMode::Matching => "--ignored=matching",
            }
            .into(),
        );
        let out = self.run_ok(Some(&repo), &extra, &args, "status")?;
        status::parse_status(&out).map_err(GitError::Parse)
    }

    /// Resolves `rev` (e.g. `HEAD`, `main`, `HEAD^{commit}`) in `repo`; `None` when it doesn't
    /// resolve.
    pub fn rev_parse(&self, repo: &Path, rev: &str) -> Result<Option<ObjectId>, GitError> {
        check_arg(rev)?;
        let repo = repo_dir(repo)?;
        let args = ["rev-parse", "--verify", "--quiet", rev].map(OsString::from);
        let out = self.exec(Some(&repo), &[], &args, "rev-parse")?;
        match out.code {
            Some(0) => Ok(refs::parse_oid_lines(&out.stdout)
                .map_err(GitError::Parse)?
                .into_iter()
                .next()),
            Some(1) => Ok(None),
            _ => Err(self.failure(&repo, "rev-parse", out)),
        }
    }

    /// All refs under `prefixes` (e.g. `refs/heads/`, `refs/remotes/`); every ref when empty.
    pub fn for_each_ref(&self, repo: &Path, prefixes: &[&str]) -> Result<Vec<RefEntry>, GitError> {
        self.list_refs(repo, None, prefixes)
    }

    /// Refs under `prefixes` whose history contains `commit` (every ref when `prefixes` is
    /// empty). A commit contained in a `refs/remotes/` ref has been pushed.
    pub fn refs_containing(
        &self,
        repo: &Path,
        commit: &ObjectId,
        prefixes: &[&str],
    ) -> Result<Vec<RefEntry>, GitError> {
        self.list_refs(repo, Some(commit), prefixes)
    }

    /// The configured upstream of local branch `branch` (`main` or `refs/heads/main`), as a
    /// full ref name; `None` when the branch has none or doesn't exist.
    pub fn upstream(&self, repo: &Path, branch: &str) -> Result<Option<String>, GitError> {
        let full = if branch.starts_with("refs/heads/") {
            branch.to_owned()
        } else {
            format!("refs/heads/{branch}")
        };
        let refs = self.for_each_ref(repo, &["refs/heads/"])?;
        Ok(refs
            .into_iter()
            .find(|r| r.name == full)
            .and_then(|r| r.upstream))
    }

    /// Whether `ancestor` is reachable from `descendant` (a commit is its own ancestor).
    pub fn is_ancestor(
        &self,
        repo: &Path,
        ancestor: &ObjectId,
        descendant: &ObjectId,
    ) -> Result<bool, GitError> {
        let repo = repo_dir(repo)?;
        let args = [
            "merge-base",
            "--is-ancestor",
            ancestor.as_str(),
            descendant.as_str(),
        ]
        .map(OsString::from);
        let out = self.exec(Some(&repo), &[], &args, "merge-base")?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self.failure(&repo, "merge-base", out)),
        }
    }

    /// The best common ancestor of `a` and `b`; `None` when they share no history.
    pub fn merge_base(
        &self,
        repo: &Path,
        a: &ObjectId,
        b: &ObjectId,
    ) -> Result<Option<ObjectId>, GitError> {
        let repo = repo_dir(repo)?;
        let args = ["merge-base", a.as_str(), b.as_str()].map(OsString::from);
        let out = self.exec(Some(&repo), &[], &args, "merge-base")?;
        match out.code {
            Some(0) => Ok(refs::parse_oid_lines(&out.stdout)
                .map_err(GitError::Parse)?
                .into_iter()
                .next()),
            Some(1) if out.stdout.is_empty() => Ok(None),
            _ => Err(self.failure(&repo, "merge-base", out)),
        }
    }

    /// Commits reachable from `tip` but from no remote-tracking ref (`refs/remotes/*`), newest
    /// first, at most `limit` when given. Empty means everything on `tip` has been pushed
    /// somewhere.
    pub fn commits_not_on_remotes(
        &self,
        repo: &Path,
        tip: &ObjectId,
        limit: Option<usize>,
    ) -> Result<Vec<ObjectId>, GitError> {
        let repo = repo_dir(repo)?;
        let mut args: Vec<OsString> = vec!["rev-list".into()];
        if let Some(n) = limit {
            args.push(format!("--max-count={n}").into());
        }
        args.extend([tip.as_str(), "--not", "--remotes"].map(OsString::from));
        let out = self.run_ok(Some(&repo), &[], &args, "rev-list")?;
        refs::parse_oid_lines(&out).map_err(GitError::Parse)
    }

    /// `git cherry <upstream> <head>`: the commits on `head` that are not on `upstream`, oldest
    /// first, each marked with whether `upstream` has a **patch-equivalent** commit (same
    /// `git patch-id`), as after a squash or rebase merge.
    ///
    /// Patch equivalence is a heuristic, never proof of a merge (GIT-03, T-02). Patch ids are
    /// computed from objects already in the repository: no work-tree file is read, so no filter
    /// driver runs, and neither textconv nor an external diff is used.
    pub fn cherry(
        &self,
        repo: &Path,
        upstream: &ObjectId,
        head: &ObjectId,
    ) -> Result<Vec<CherryCommit>, GitError> {
        let repo = repo_dir(repo)?;
        let args = ["cherry", upstream.as_str(), head.as_str()].map(OsString::from);
        let out = self.run_ok(Some(&repo), &[], &args, "cherry")?;
        refs::parse_cherry(&out).map_err(GitError::Parse)
    }

    /// Paths of the submodules (gitlinks, mode `160000`) recorded in the index of the work tree
    /// whose top folder is `worktree`, in index order. Whether each one is initialized (has a
    /// `.git`) is up to the caller to check.
    pub fn gitlinks(&self, worktree: &Path) -> Result<Vec<GitPath>, GitError> {
        let repo = repo_dir(worktree)?;
        let args = ["ls-files", "--stage", "-z"].map(OsString::from);
        let out = self.run_ok(Some(&repo), &[], &args, "ls-files")?;
        status::parse_gitlinks(&out).map_err(GitError::Parse)
    }

    fn list_refs(
        &self,
        repo: &Path,
        contains: Option<&ObjectId>,
        prefixes: &[&str],
    ) -> Result<Vec<RefEntry>, GitError> {
        for p in prefixes {
            check_arg(p)?;
            if !p.starts_with("refs/") {
                return Err(GitError::InvalidArgument(format!(
                    "{p:?}: ref prefix must start with refs/"
                )));
            }
        }
        let repo = repo_dir(repo)?;
        let mut args: Vec<OsString> = vec!["for-each-ref".into(), REF_FORMAT.into()];
        if let Some(c) = contains {
            args.push("--contains".into());
            args.push(c.as_str().into());
        }
        args.extend(prefixes.iter().map(OsString::from));
        let out = self.run_ok(Some(&repo), &[], &args, "for-each-ref")?;
        refs::parse_refs(&out).map_err(GitError::Parse)
    }

    /// `-c` overrides that disable every filter driver defined anywhere in `repo`'s config.
    fn filter_overrides(&self, repo: &Path) -> Result<Vec<String>, GitError> {
        let args = ["config", "--list", "--name-only", "-z"].map(OsString::from);
        let out = self.exec(Some(repo), &[], &args, "config")?;
        // `git config --list` exits 0 even with no config; any failure is fatal here, since
        // we can't prove which filters exist.
        if !out.success() {
            return Err(self.failure(repo, "config", out));
        }
        filter_overrides_from_names(&out.stdout)
    }

    fn run_ok(
        &self,
        repo: Option<&Path>,
        extra: &[String],
        args: &[OsString],
        command: &str,
    ) -> Result<Vec<u8>, GitError> {
        let out = self.exec(repo, extra, args, command)?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(self.failure(repo.unwrap_or(Path::new("")), command, out))
        }
    }

    fn failure(&self, repo: &Path, command: &str, out: Output) -> GitError {
        let e = &out.stderr;
        if e.contains("dubious ownership") {
            GitError::DubiousOwnership(repo.to_path_buf())
        } else if e.contains("not a git repository") {
            GitError::NotARepository(repo.to_path_buf())
        } else {
            GitError::Failed {
                command: command.to_owned(),
                code: out.code,
                stderr: out.stderr,
            }
        }
    }

    /// Builds the hardened command line.
    fn command(&self, repo: Option<&Path>, extra: &[String], args: &[OsString]) -> Command {
        let sb = &self.sandbox;
        let mut cmd = Command::new(&self.binary);
        cmd.env_clear()
            .current_dir(&sb.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.passthrough {
            cmd.env(k, v);
        }
        cmd.env("PATH", &self.search_path)
            .env("HOME", &sb.home)
            .env("XDG_CONFIG_HOME", &sb.home)
            .env("TMPDIR", &sb.tmp)
            .env("TEMP", &sb.tmp)
            .env("TMP", &sb.tmp)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &sb.empty_file)
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_PROTOCOL_FROM_USER", "0")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env("LC_ALL", "C")
            .env("LANG", "C");
        if let Some(repo) = repo
            && let Some(parent) = repo.parent()
            && let Ok(ceiling) = std::env::join_paths([parent])
        {
            cmd.env("GIT_CEILING_DIRECTORIES", ceiling);
        }

        cmd.args(["--no-pager", "--no-optional-locks", "--no-replace-objects"]);
        let mut hooks = OsString::from("core.hooksPath=");
        hooks.push(&sb.hooks);
        let mut attrs = OsString::from("core.attributesFile=");
        attrs.push(&sb.empty_file);
        for kv in [hooks, attrs] {
            cmd.arg("-c").arg(kv);
        }
        for kv in FORCED_CONFIG
            .iter()
            .copied()
            .chain(extra.iter().map(String::as_str))
        {
            cmd.arg("-c").arg(kv);
        }
        if let Some(repo) = repo {
            cmd.arg("-C").arg(repo);
        }
        cmd.args(args);
        cmd
    }

    /// Runs one call with the timeout and output cap.
    fn exec(
        &self,
        repo: Option<&Path>,
        extra: &[String],
        args: &[OsString],
        command: &str,
    ) -> Result<Output, GitError> {
        let mut child = self
            .command(repo, extra, args)
            .spawn()
            .map_err(|e| GitError::Io(format!("{}: {e}", self.binary.display())))?;
        let (tx, rx) = mpsc::channel::<(bool, io::Result<(Vec<u8>, bool)>)>();
        if let Some(out) = child.stdout.take() {
            let (tx, cap) = (tx.clone(), self.max_output);
            thread::spawn(move || {
                let _ = tx.send((true, read_capped(out, cap)));
            });
        }
        if let Some(err) = child.stderr.take() {
            let tx = tx.clone();
            thread::spawn(move || {
                let _ = tx.send((false, read_capped(err, MAX_STDERR)));
            });
        }
        drop(tx);

        let deadline = Instant::now() + self.timeout;
        let timed_out = || GitError::Timeout {
            command: command.to_owned(),
            after: self.timeout,
        };
        let mut nap = Duration::from_millis(1);
        let status = loop {
            if let Some(s) = child.try_wait().map_err(|e| GitError::Io(e.to_string()))? {
                break s;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(timed_out());
            }
            thread::sleep(nap);
            nap = (nap * 2).min(Duration::from_millis(25));
        };

        // Collect both streams. A grandchild still holding a pipe can't stall us past the
        // deadline (plus a short grace period).
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        for _ in 0..2 {
            let left = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1));
            let (is_out, res) = rx.recv_timeout(left).map_err(|_| timed_out())?;
            let (bytes, overflow) = res.map_err(|e| GitError::Io(e.to_string()))?;
            if is_out {
                if overflow {
                    return Err(GitError::OutputTooLarge {
                        command: command.to_owned(),
                    });
                }
                stdout = bytes;
            } else {
                stderr = bytes;
            }
        }
        Ok(Output {
            code: status.code(),
            stdout,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

/// Config forced on every call. Values set to empty reset list-valued keys or disable the
/// feature.
const FORCED_CONFIG: &[&str] = &[
    "core.fsmonitor=false",
    "core.untrackedCache=false",
    "core.askPass=",
    "core.pager=cat",
    "credential.helper=",
    "diff.external=",
    "protocol.allow=never",
    "gc.auto=0",
    "maintenance.auto=false",
    "log.showSignature=false",
    "status.submoduleSummary=false",
    "submodule.recurse=false",
];

/// Reads up to `cap` bytes, then drains the rest so the child never blocks on a full pipe.
/// Returns the bytes and whether the cap was exceeded.
fn read_capped(mut r: impl Read, cap: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    (&mut r).take(cap as u64).read_to_end(&mut buf)?;
    let rest = io::copy(&mut r, &mut io::sink())?;
    Ok((buf, rest > 0))
}

/// Turns `git config --list --name-only -z` output into `-c` overrides neutralizing every
/// `filter.<driver>.*` driver.
pub(crate) fn filter_overrides_from_names(names: &[u8]) -> Result<Vec<String>, GitError> {
    let mut drivers: Vec<String> = Vec::new();
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        // Section and key are case-insensitive; the subsection (driver name) is not.
        let Some(rest) = name
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case(b"filter."))
            .map(|_| &name[7..])
        else {
            continue;
        };
        // `filter.<driver>.<key>`: the driver is everything up to the last dot. A two-part
        // `filter.<key>` defines no driver.
        let Some(dot) = rest.iter().rposition(|&b| b == b'.') else {
            continue;
        };
        let driver = &rest[..dot];
        let driver = std::str::from_utf8(driver).map_err(|_| {
            GitError::UnsafeConfig(format!(
                "filter driver name is not UTF-8: {:?}",
                String::from_utf8_lossy(driver)
            ))
        })?;
        if driver.is_empty() || driver.contains(['=', '\n', '\r', '\0']) {
            return Err(GitError::UnsafeConfig(format!(
                "filter driver name {driver:?} can't be overridden safely"
            )));
        }
        if !drivers.iter().any(|d| d == driver) {
            drivers.push(driver.to_owned());
        }
    }
    Ok(drivers
        .iter()
        .flat_map(|d| {
            [
                format!("filter.{d}.clean="),
                format!("filter.{d}.smudge="),
                format!("filter.{d}.process="),
                format!("filter.{d}.required=false"),
            ]
        })
        .collect())
}

/// Refuses arguments Git could read as an option, and malformed ones.
fn check_arg(arg: &str) -> Result<(), GitError> {
    if arg.is_empty() || arg.starts_with('-') || arg.contains(['\0', '\n']) {
        return Err(GitError::InvalidArgument(format!("{arg:?}")));
    }
    Ok(())
}

/// Canonical form of a repository folder argument.
fn repo_dir(p: &Path) -> Result<PathBuf, GitError> {
    let c = fs::canonicalize(p).map_err(|_| GitError::NotADirectory(p.to_path_buf()))?;
    let c = resolve::strip_verbatim(c);
    if !c.is_dir() {
        return Err(GitError::NotADirectory(p.to_path_buf()));
    }
    Ok(c)
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<GitRunner>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        let v = |s| GitVersion::parse(s);
        assert_eq!(
            v("git version 2.43.0\n"),
            Some(GitVersion {
                major: 2,
                minor: 43,
                patch: 0
            })
        );
        assert_eq!(v("git version 2.45.1.windows.1").map(|v| v.minor), Some(45));
        assert_eq!(
            v("git version 2.39.5 (Apple Git-154)").map(|v| v.patch),
            Some(5)
        );
        assert_eq!(v("git version 3.0").map(|v| v.major), Some(3));
        assert_eq!(v("hg version 2.4"), None);
        assert!(v("git version 2.19.9").expect("parse") < MIN_GIT_VERSION);
    }

    #[test]
    fn filter_drivers_are_enumerated_and_neutralized() {
        let names = b"core.bare\0filter.lfs.clean\0filter.lfs.smudge\0Filter.My.Drv.process\0\
                      filter.required\0diff.x.textconv\0";
        let o = filter_overrides_from_names(names).expect("ok");
        assert_eq!(
            o,
            vec![
                "filter.lfs.clean=",
                "filter.lfs.smudge=",
                "filter.lfs.process=",
                "filter.lfs.required=false",
                "filter.My.Drv.clean=",
                "filter.My.Drv.smudge=",
                "filter.My.Drv.process=",
                "filter.My.Drv.required=false",
            ]
        );
        assert!(
            filter_overrides_from_names(b"core.bare\0")
                .expect("ok")
                .is_empty()
        );
    }

    #[test]
    fn unsafe_driver_names_are_refused() {
        for bad in [
            &b"filter.a=b.clean\0"[..],
            b"filter.\xff.clean\0",
            b"filter..clean\0",
        ] {
            assert!(
                matches!(
                    filter_overrides_from_names(bad),
                    Err(GitError::UnsafeConfig(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn option_like_arguments_are_refused() {
        assert!(check_arg("HEAD").is_ok());
        assert!(check_arg("refs/heads/main").is_ok());
        for bad in ["", "-x", "--upload-pack=evil", "a\nb", "a\0b"] {
            assert!(check_arg(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn read_capped_reports_overflow_and_drains() {
        let (b, over) = read_capped(&b"hello world"[..], 5).expect("read");
        assert_eq!((b.as_slice(), over), (&b"hello"[..], true));
        let (b, over) = read_capped(&b"hi"[..], 5).expect("read");
        assert_eq!((b.as_slice(), over), (&b"hi"[..], false));
    }
}
