//! Test helpers: sandboxed `git` invocations and tree snapshots.
//!
//! Tests may use the real `git` binary to *build* fixtures (the library never runs it). Every
//! invocation is isolated from the machine's configuration: `HOME`/`XDG_CONFIG_HOME` point into
//! the temp dir, system config is disabled, and identity/branch defaults are passed with `-c`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether a usable `git` binary is on `PATH`. Tests skip (pass) when it isn't.
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A temporary sandbox with an isolated Git environment.
pub struct Sandbox {
    tmp: tempfile::TempDir,
}

impl Sandbox {
    /// Creates a sandbox, or `None` (and a note on stderr) when `git` is missing.
    pub fn new_with_git() -> Option<Self> {
        if !git_available() {
            eprintln!("skipping: `git` not found on PATH");
            return None;
        }
        Some(Self::new())
    }

    /// Creates a sandbox for tests that don't need `git`.
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("create temp dir");
        fs::create_dir_all(tmp.path().join("home")).expect("create home");
        fs::write(tmp.path().join("home").join(".gitconfig"), "").expect("write gitconfig");
        Self { tmp }
    }

    /// The sandbox root (canonicalized, so it compares equal to discovery output).
    pub fn path(&self) -> PathBuf {
        canon(self.tmp.path())
    }

    /// Runs `git <args>` in `cwd` and returns trimmed stdout. Panics on failure.
    pub fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.git_output(cwd, args);
        assert!(
            out.status.success(),
            "git {args:?} failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
    }

    /// Runs `git <args>` in `cwd` (isolated from the machine's config but *not* hardened
    /// against the repository's config) and returns the raw output, whatever the exit code.
    pub fn git_output(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        let home = self.tmp.path().join("home");
        Command::new("git")
            .current_dir(cwd)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_CEILING_DIRECTORIES")
            .args([
                "-c",
                "user.name=scull-test",
                "-c",
                "user.email=scull-test@example.invalid",
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .output()
            .expect("spawn git")
    }

    /// `git init <name>` plus one empty commit; returns the repo folder.
    pub fn init_repo(&self, name: &str) -> PathBuf {
        let root = self.path();
        self.git(&root, &["init", "-q", name]);
        let repo = root.join(name);
        self.git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        repo
    }
}

/// Canonical form for comparisons; for a missing path, canonicalizes the nearest existing
/// ancestor. Strips Windows verbatim prefixes.
pub fn canon(p: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(p) {
        return strip_verbatim(c);
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => canon(parent).join(name),
        _ => p.to_path_buf(),
    }
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    match p.to_str() {
        Some(s) if s.starts_with(r"\\?\UNC\") => PathBuf::from(format!(r"\\{}", &s[8..])),
        Some(s) if s.starts_with(r"\\?\") => PathBuf::from(&s[4..]),
        _ => p,
    }
}

/// Writes `path` with forward slashes, as Git for Windows does in metadata files.
pub fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Default, Clone)]
pub struct PorcelainEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    /// `Some("")` when locked without a reason.
    pub locked: Option<String>,
    pub prunable: bool,
}

/// Parses `git worktree list --porcelain`.
pub fn parse_porcelain(text: &str) -> Vec<PorcelainEntry> {
    let mut out = Vec::new();
    let mut cur: Option<PorcelainEntry> = None;
    for line in text.lines() {
        if line.is_empty() {
            out.extend(cur.take());
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            out.extend(cur.take());
            cur = Some(PorcelainEntry {
                path: PathBuf::from(value),
                ..Default::default()
            });
            continue;
        }
        let e = cur.as_mut().expect("porcelain line before `worktree`");
        match key {
            "HEAD" => e.head = Some(value.to_owned()),
            "branch" => e.branch = Some(value.to_owned()),
            "detached" => e.detached = true,
            "bare" => e.bare = true,
            "locked" => e.locked = Some(value.to_owned()),
            "prunable" => e.prunable = true,
            _ => {}
        }
    }
    out.extend(cur);
    out
}

/// A snapshot of every entry under `root`: relative path → (is_dir, len, mtime, bytes).
pub type Snapshot = BTreeMap<PathBuf, (bool, u64, Option<std::time::SystemTime>, Vec<u8>)>;

/// Records the tree under `root` without following symlinks.
pub fn snapshot(root: &Path) -> Snapshot {
    let mut map = Snapshot::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            let md = fs::symlink_metadata(&path).expect("metadata");
            let rel = path.strip_prefix(root).expect("prefix").to_path_buf();
            let bytes = if md.is_file() {
                fs::read(&path).expect("read")
            } else {
                Vec::new()
            };
            let mtime = if md.is_dir() {
                None
            } else {
                md.modified().ok()
            };
            map.insert(rel, (md.is_dir(), md.len(), mtime, bytes));
            if md.is_dir() {
                stack.push(path);
            }
        }
    }
    map
}
