//! Worktree discovery from Git metadata (GIT-01).
//!
//! Worktrees are enumerated from the repository's own administrative files, never from
//! agent-specific folder names, and **without running `git`** (SAFE-15, T-21): nothing here
//! spawns a process or interprets repository config beyond reading `core.bare`.
//!
//! Layout read (all paths relative to the common Git dir):
//!
//! * `HEAD`, `config`: the main (or bare) worktree.
//! * `worktrees/<name>/gitdir`: path of the linked worktree's `.git` file.
//! * `worktrees/<name>/commondir`: path back to the common dir (validated).
//! * `worktrees/<name>/HEAD`: the linked worktree's HEAD.
//! * `worktrees/<name>/locked`: present when locked; its contents are the reason.
//!
//! and, in each linked worktree, the `.git` file (`gitdir: <admin dir>`).
//!
//! Every pointer is validated in both directions before a worktree is reported: a linked
//! worktree's `.git` file must point back to the admin dir that names it. A crafted `gitdir`
//! that points at an unrelated folder (for example someone's project) is reported as a
//! [`WorktreeProblem`], never as a worktree, so later stages can't be tricked into treating that
//! folder as removable (THREAT_MODEL S2, FX-GIT-16).

use crate::meta::{self, ReadError};
use schemars::JsonSchema;
use scull_core::FsPath;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// All worktrees of one repository, as recorded in its Git metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoWorktrees {
    /// The common Git dir: `<repo>/.git` for a normal repository, the repository folder itself
    /// for a bare one. This identifies the parent repository of every worktree below.
    pub common_dir: FsPath,
    /// Whether the repository is bare (no main working tree).
    pub bare: bool,
    /// The main (or bare) entry first, then linked worktrees sorted by path, the same order
    /// as `git worktree list`.
    pub worktrees: Vec<Worktree>,
    /// Linked-worktree registrations whose metadata is malformed, inconsistent or unreadable.
    /// These are never reported as worktrees; callers should surface them and treat them as
    /// protected. Sorted by admin-dir name.
    pub problems: Vec<WorktreeProblem>,
}

/// One worktree of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Worktree {
    /// Main, bare or linked.
    pub kind: WorktreeKind,
    /// Admin-dir name under `<common dir>/worktrees/` (linked worktrees only).
    pub name: Option<String>,
    /// The working tree folder. For a bare repository's entry, the repository folder. For a
    /// prunable worktree this folder no longer exists.
    pub path: FsPath,
    /// This worktree's Git dir: the common dir for the main/bare entry, the admin dir
    /// (`<common dir>/worktrees/<name>`) for a linked worktree.
    pub git_dir: FsPath,
    /// What HEAD points at.
    pub head: WorktreeHead,
    /// Lock state (`git worktree lock`). Main and bare entries are never locked.
    pub lock: LockState,
    /// Set when the registration is stale and `git worktree prune` would remove it
    /// (metadata only; FX-GIT-10). Locked worktrees are never prunable.
    pub prunable: Option<PruneReason>,
}

impl Worktree {
    /// Whether this worktree's folder lies inside `root` (compared after resolving symlinks).
    ///
    /// Use this to tell a scan root's own worktrees from ones that live elsewhere but share
    /// the same repository (FX-GIT-16).
    pub fn is_within(&self, root: &Path) -> bool {
        meta::clean_path(&self.path.to_path_buf()).starts_with(meta::clean_path(root))
    }
}

/// The role of a worktree in its repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeKind {
    /// The main working tree of a non-bare repository.
    Main,
    /// The entry for a bare repository itself (no working tree).
    Bare,
    /// A linked worktree created with `git worktree add`.
    Linked,
}

/// What a worktree's HEAD points at, read from its `HEAD` file. Refs are not resolved here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorktreeHead {
    /// HEAD is a symbolic ref, e.g. `refs/heads/main`. The branch may be unborn.
    Branch {
        /// Full ref name.
        name: String,
    },
    /// HEAD is detached at a commit.
    Detached {
        /// Object id (hex, SHA-1 or SHA-256).
        oid: String,
    },
    /// HEAD is missing or unparseable. Treat as unknown (protective).
    Invalid {
        /// What was wrong.
        detail: String,
    },
}

/// Lock state of a linked worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LockState {
    /// Not locked.
    Unlocked,
    /// Locked with `git worktree lock`; must be shown with its reason (FX-GIT-09).
    Locked {
        /// The reason given with `--reason`, if any.
        reason: Option<String>,
    },
}

impl LockState {
    /// Whether the worktree is locked.
    pub fn is_locked(&self) -> bool {
        matches!(self, LockState::Locked { .. })
    }
}

/// Why a worktree registration is prunable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PruneReason {
    /// The registered worktree folder no longer exists. Git reports this as "gitdir file points
    /// to non-existent location". (A folder that still exists but lost its `.git` link is
    /// reported as a [`ProblemKind::BackLinkMismatch`] instead, to stay protective.)
    WorktreeMissing,
}

/// A linked-worktree registration that could not be trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorktreeProblem {
    /// Admin-dir name under `<common dir>/worktrees/`.
    pub name: String,
    /// The admin dir.
    pub admin_dir: FsPath,
    /// The worktree folder it claims, when that much could be read.
    pub claimed_path: Option<FsPath>,
    /// Category.
    pub kind: ProblemKind,
    /// Human-readable detail.
    pub detail: String,
}

/// Category of a [`WorktreeProblem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// A required metadata file (`gitdir`, `commondir`) is missing.
    MissingFile,
    /// A metadata file has unexpected contents, is not a regular file, or is too large.
    Malformed,
    /// A metadata file or the worktree's `.git` file could not be read.
    Unreadable,
    /// `commondir` resolves to a different repository than the one it is registered in.
    CommonDirMismatch,
    /// The claimed worktree's `.git` does not point back to this admin dir (moved, copied,
    /// or crafted). The folder is *not* treated as a worktree of this repository.
    BackLinkMismatch,
}

/// Why discovery failed as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiscoverError {
    /// The start path does not exist.
    NotFound {
        /// The path given.
        path: FsPath,
    },
    /// No repository was found at or above the start path.
    NotARepository {
        /// The path given.
        path: FsPath,
    },
    /// A `.git` file or Git dir was found but its metadata is malformed or points somewhere
    /// that isn't a valid worktree admin dir or Git dir.
    Malformed {
        /// The offending file or folder.
        path: FsPath,
        /// What was wrong.
        detail: String,
    },
    /// An I/O error while reading metadata.
    Io {
        /// The file or folder being read.
        path: FsPath,
        /// The error.
        detail: String,
    },
}

impl fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoverError::NotFound { path } => write!(f, "{path}: path does not exist"),
            DiscoverError::NotARepository { path } => {
                write!(f, "{path}: not inside a Git repository or worktree")
            }
            DiscoverError::Malformed { path, detail } => {
                write!(f, "{path}: malformed Git metadata: {detail}")
            }
            DiscoverError::Io { path, detail } => write!(f, "{path}: {detail}"),
        }
    }
}

impl std::error::Error for DiscoverError {}

fn malformed(path: &Path, detail: impl Into<String>) -> DiscoverError {
    DiscoverError::Malformed {
        path: FsPath::from_path(path),
        detail: detail.into(),
    }
}

/// Finds the repository that contains `start` (any path inside a working tree, a linked
/// worktree, a `.git` dir or a bare repository) and lists all of its worktrees.
///
/// Read-only; never runs `git`.
pub fn discover(start: &Path) -> Result<RepoWorktrees, DiscoverError> {
    list_worktrees(&find_common_dir(start)?)
}

/// Finds the common Git dir of the repository that contains `start`.
///
/// Walks up from `start`. A `.git` *file* is followed only if it points at either a
/// `worktrees/<name>` admin dir whose `commondir` leads back to the Git dir that contains it, or
/// directly at a Git dir (separate-git-dir layouts and submodules).
pub fn find_common_dir(start: &Path) -> Result<PathBuf, DiscoverError> {
    if meta::definitely_missing(start) {
        return Err(DiscoverError::NotFound {
            path: FsPath::from_path(start),
        });
    }
    let start = meta::clean_path(start);
    let first = if meta::is_dir(&start) {
        start.as_path()
    } else {
        start.parent().unwrap_or(&start)
    };

    for dir in first.ancestors() {
        let dotgit = dir.join(".git");
        match fs::metadata(&dotgit) {
            Ok(md) if md.is_dir() => {
                if meta::is_git_dir(&dotgit) {
                    return Ok(meta::clean_path(&dotgit));
                }
                return Err(malformed(&dotgit, "`.git` folder is not a Git directory"));
            }
            Ok(md) if md.is_file() => {
                let target = read_gitfile(&dotgit)?;
                return common_dir_of_git_dir(&target);
            }
            Ok(_) => return Err(malformed(&dotgit, "`.git` is not a file or folder")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(DiscoverError::Io {
                    path: FsPath::from_path(&dotgit),
                    detail: e.to_string(),
                });
            }
        }
        // Inside a bare repository, a `.git` dir, or a worktree admin dir.
        if meta::is_git_dir(dir) || is_admin_dir(dir) {
            return common_dir_of_git_dir(dir);
        }
    }
    Err(DiscoverError::NotARepository {
        path: FsPath::from_path(&start),
    })
}

/// Whether `dir` looks like `<common>/worktrees/<name>`.
fn is_admin_dir(dir: &Path) -> bool {
    meta::is_file(&dir.join("commondir"))
        && meta::is_file(&dir.join("gitdir"))
        && dir.parent().and_then(Path::file_name) == Some("worktrees".as_ref())
}

/// Parses a `.git` file (`gitdir: <path>`) and returns the Git dir it names.
fn read_gitfile(dotgit: &Path) -> Result<PathBuf, DiscoverError> {
    let text = meta::read_text(dotgit).map_err(|e| read_err(dotgit, e))?;
    let base = dotgit.parent().unwrap_or(Path::new(""));
    parse_gitfile(&text, base).ok_or_else(|| malformed(dotgit, "expected `gitdir: <path>`"))
}

fn parse_gitfile(text: &str, base: &Path) -> Option<PathBuf> {
    let rest = text.strip_prefix("gitdir:")?;
    let value = rest.trim();
    if value.is_empty() {
        return None;
    }
    Some(meta::path_from_meta(value, base))
}

fn read_err(path: &Path, e: ReadError) -> DiscoverError {
    match e {
        ReadError::Io(io) => DiscoverError::Io {
            path: FsPath::from_path(path),
            detail: io.to_string(),
        },
        other => malformed(path, other.to_string()),
    }
}

/// Given a Git dir (a common dir or a worktree admin dir), returns the validated common dir.
fn common_dir_of_git_dir(git_dir: &Path) -> Result<PathBuf, DiscoverError> {
    let commondir_file = git_dir.join("commondir");
    match meta::read_text(&commondir_file) {
        Err(ReadError::Missing) => {
            if meta::is_git_dir(git_dir) {
                Ok(meta::clean_path(git_dir))
            } else {
                Err(malformed(
                    git_dir,
                    "does not point at a Git directory or a worktree admin dir",
                ))
            }
        }
        Err(e) => Err(read_err(&commondir_file, e)),
        Ok(text) => {
            let value = text.trim_end_matches(['\n', '\r']);
            if value.is_empty() {
                return Err(malformed(&commondir_file, "empty commondir"));
            }
            let common = meta::path_from_meta(value, git_dir);
            // The admin dir must physically live at `<common>/worktrees/<name>`.
            let container = git_dir
                .parent()
                .filter(|p| p.file_name() == Some("worktrees".as_ref()));
            let Some(expected) = container.and_then(Path::parent) else {
                return Err(malformed(
                    git_dir,
                    "admin dir is not inside a `worktrees/` folder",
                ));
            };
            if !meta::same_location(&common, expected) {
                return Err(malformed(
                    &commondir_file,
                    "commondir does not lead to the Git dir that contains this admin dir",
                ));
            }
            if !meta::is_git_dir(&common) {
                return Err(malformed(
                    &commondir_file,
                    "commondir is not a Git directory",
                ));
            }
            Ok(meta::clean_path(&common))
        }
    }
}

/// Lists all worktrees registered in the common Git dir `common_dir`.
///
/// Read-only; never runs `git`. Malformed linked registrations become
/// [`RepoWorktrees::problems`] instead of failing the whole listing.
pub fn list_worktrees(common_dir: &Path) -> Result<RepoWorktrees, DiscoverError> {
    if !meta::is_git_dir(common_dir) {
        return Err(DiscoverError::NotARepository {
            path: FsPath::from_path(common_dir),
        });
    }
    let common = meta::clean_path(common_dir);
    let named_dot_git = common.file_name() == Some(".git".as_ref());

    let bare = match meta::read_text(&common.join("config")) {
        Ok(text) => meta::config_core_bare(&text),
        Err(_) => None,
    }
    .unwrap_or(!named_dot_git);

    // Like Git: the main worktree is the common dir with a trailing `/.git` removed.
    let main_path = if named_dot_git {
        common.parent().unwrap_or(&common).to_path_buf()
    } else {
        common.clone()
    };
    let mut worktrees = vec![Worktree {
        kind: if bare {
            WorktreeKind::Bare
        } else {
            WorktreeKind::Main
        },
        name: None,
        path: FsPath::from_path(&main_path),
        git_dir: FsPath::from_path(&common),
        head: read_head(&common.join("HEAD")),
        lock: LockState::Unlocked,
        prunable: None,
    }];
    let mut problems = Vec::new();

    let admin_root = common.join("worktrees");
    if meta::is_dir(&admin_root) {
        let entries = fs::read_dir(&admin_root).map_err(|e| DiscoverError::Io {
            path: FsPath::from_path(&admin_root),
            detail: e.to_string(),
        })?;
        let mut admins: Vec<(String, PathBuf)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| DiscoverError::Io {
                path: FsPath::from_path(&admin_root),
                detail: e.to_string(),
            })?;
            let path = entry.path();
            if meta::is_dir(&path) {
                admins.push((entry.file_name().to_string_lossy().into_owned(), path));
            }
        }
        admins.sort();
        for (name, admin) in admins {
            match linked_worktree(&common, &name, &admin) {
                Ok(wt) => worktrees.push(wt),
                Err(p) => problems.push(*p),
            }
        }
        // Like `git worktree list`: main entry first, linked worktrees sorted by path.
        worktrees[1..].sort_by(|a, b| a.path.cmp(&b.path));
    }

    Ok(RepoWorktrees {
        common_dir: FsPath::from_path(&common),
        bare,
        worktrees,
        problems,
    })
}

/// Reads and validates one `worktrees/<name>` registration.
fn linked_worktree(
    common: &Path,
    name: &str,
    admin: &Path,
) -> Result<Worktree, Box<WorktreeProblem>> {
    let problem = |kind, claimed: Option<&Path>, detail: String| {
        Box::new(WorktreeProblem {
            name: name.to_owned(),
            admin_dir: FsPath::from_path(admin),
            claimed_path: claimed.map(FsPath::from_path),
            kind,
            detail,
        })
    };
    let file_problem = |file: &str, e: ReadError| {
        let kind = match e {
            ReadError::Missing => ProblemKind::MissingFile,
            ReadError::Io(_) => ProblemKind::Unreadable,
            _ => ProblemKind::Malformed,
        };
        problem(kind, None, format!("{file}: {e}"))
    };

    // commondir must lead back to the repository we're listing.
    let commondir =
        meta::read_text(&admin.join("commondir")).map_err(|e| file_problem("commondir", e))?;
    let commondir = commondir.trim_end_matches(['\n', '\r']);
    if commondir.is_empty() {
        return Err(problem(
            ProblemKind::Malformed,
            None,
            "commondir: empty".into(),
        ));
    }
    if !meta::same_location(&meta::path_from_meta(commondir, admin), common) {
        return Err(problem(
            ProblemKind::CommonDirMismatch,
            None,
            format!("commondir `{commondir}` does not lead to this repository"),
        ));
    }

    // gitdir names the worktree's `.git` file.
    let gitdir = meta::read_text(&admin.join("gitdir")).map_err(|e| file_problem("gitdir", e))?;
    let gitdir = gitdir.trim_end();
    if gitdir.is_empty() {
        return Err(problem(
            ProblemKind::Malformed,
            None,
            "gitdir: empty".into(),
        ));
    }
    let dotgit = meta::clean_path(&meta::path_from_meta(gitdir, admin));
    if dotgit.file_name() != Some(".git".as_ref()) {
        return Err(problem(
            ProblemKind::Malformed,
            None,
            format!("gitdir `{gitdir}` does not name a `.git` file"),
        ));
    }
    let wt_path = dotgit.parent().unwrap_or(&dotgit).to_path_buf();

    // Git checks only for the lock file's existence; any error other than "not found" is
    // treated as locked (protective).
    let lock_file = admin.join("locked");
    let lock = if meta::definitely_missing(&lock_file) {
        LockState::Unlocked
    } else {
        let reason = meta::read_text(&lock_file)
            .ok()
            .map(|r| r.trim_end_matches(['\n', '\r']).to_owned())
            .filter(|r| !r.is_empty());
        LockState::Locked { reason }
    };

    let prunable = if meta::definitely_missing(&dotgit) {
        if !meta::definitely_missing(&wt_path) {
            // Git would call this prunable, but the folder still exists and nothing links it to
            // this repository. It could be a crafted `gitdir` naming someone's project, so it
            // is reported as a problem rather than a (removable-looking) worktree.
            return Err(problem(
                ProblemKind::BackLinkMismatch,
                Some(&wt_path),
                format!(
                    "{} exists but has no `.git` link back to this repository",
                    wt_path.display()
                ),
            ));
        }
        if lock.is_locked() {
            None
        } else {
            Some(PruneReason::WorktreeMissing)
        }
    } else {
        // The worktree exists: its `.git` file must point back at this admin dir.
        match meta::read_text(&dotgit) {
            Ok(text) => match parse_gitfile(&text, &wt_path) {
                Some(target) if meta::same_location(&target, admin) => None,
                Some(target) => {
                    return Err(problem(
                        ProblemKind::BackLinkMismatch,
                        Some(&wt_path),
                        format!(
                            "{} points to `{}`, not to this admin dir",
                            dotgit.display(),
                            target.display()
                        ),
                    ));
                }
                None => {
                    return Err(problem(
                        ProblemKind::BackLinkMismatch,
                        Some(&wt_path),
                        format!("{} is not a `gitdir:` link", dotgit.display()),
                    ));
                }
            },
            Err(ReadError::NotAFile) => {
                return Err(problem(
                    ProblemKind::BackLinkMismatch,
                    Some(&wt_path),
                    format!(
                        "{} is a folder (an independent repository), not a link to this one",
                        dotgit.display()
                    ),
                ));
            }
            Err(ReadError::Io(e)) => {
                return Err(problem(
                    ProblemKind::Unreadable,
                    Some(&wt_path),
                    format!("{}: {e}", dotgit.display()),
                ));
            }
            Err(e) => {
                return Err(problem(
                    ProblemKind::BackLinkMismatch,
                    Some(&wt_path),
                    format!("{}: {e}", dotgit.display()),
                ));
            }
        }
    };

    Ok(Worktree {
        kind: WorktreeKind::Linked,
        name: Some(name.to_owned()),
        path: FsPath::from_path(&wt_path),
        git_dir: FsPath::from_path(&meta::clean_path(admin)),
        head: read_head(&admin.join("HEAD")),
        lock,
        prunable,
    })
}

/// Reads a `HEAD` file.
fn read_head(path: &Path) -> WorktreeHead {
    match meta::read_text(path) {
        Ok(text) => parse_head(&text),
        Err(e) => WorktreeHead::Invalid {
            detail: format!("HEAD: {e}"),
        },
    }
}

fn parse_head(text: &str) -> WorktreeHead {
    let t = text.trim();
    if let Some(r) = t.strip_prefix("ref:") {
        let r = r.trim();
        if r.starts_with("refs/") && !r.contains(char::is_whitespace) {
            return WorktreeHead::Branch { name: r.to_owned() };
        }
    } else if (t.len() == 40 || t.len() == 64) && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return WorktreeHead::Detached {
            oid: t.to_ascii_lowercase(),
        };
    }
    WorktreeHead::Invalid {
        detail: "HEAD is neither `ref: refs/...` nor an object id".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_parsing() {
        assert_eq!(
            parse_head("ref: refs/heads/main\n"),
            WorktreeHead::Branch {
                name: "refs/heads/main".into()
            }
        );
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            parse_head(&format!("{sha}\n")),
            WorktreeHead::Detached { oid: sha.into() }
        );
        assert!(matches!(
            parse_head("garbage"),
            WorktreeHead::Invalid { .. }
        ));
        assert!(matches!(
            parse_head("ref: HEAD"),
            WorktreeHead::Invalid { .. }
        ));
        assert!(matches!(parse_head(""), WorktreeHead::Invalid { .. }));
    }

    #[test]
    fn gitfile_parsing() {
        let base = Path::new("w");
        assert_eq!(
            parse_gitfile("gitdir: ../r/.git/worktrees/a\n", base),
            Some(Path::new("w").join("../r/.git/worktrees/a"))
        );
        assert_eq!(parse_gitfile("gitdir:   \n", base), None);
        assert_eq!(parse_gitfile("nonsense", base), None);
    }
}
