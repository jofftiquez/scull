//! Trusted resolution of the `git` binary (SAFE-12, T-22).
//!
//! The binary is looked up once, by walking `PATH` explicitly instead of letting the OS search
//! for it. An entry is skipped when it is:
//!
//! * empty or relative (`""`, `.`, `bin`): these resolve against the current directory, which
//!   may be a scan root or a malicious checkout;
//! * the current directory itself;
//! * inside a `node_modules` folder (`node_modules/.bin` is package-controlled);
//! * inside one of the caller's excluded roots (the scan roots and any repository being
//!   analysed);
//! * not an existing directory.
//!
//! The candidate is canonicalized and re-checked, so a symlink in a trusted directory that
//! points into an excluded root is rejected too. Only `git` (Unix) / `git.exe` (Windows) is
//! accepted: never `git.cmd` / `git.bat`, which would need a shell.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) use crate::meta::strip_verbatim;

/// The outcome of a trusted lookup.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    /// Canonical absolute path of the `git` binary, if a trusted one exists.
    pub binary: Option<PathBuf>,
    /// The trusted `PATH` entries, in order, used as the child's `PATH`.
    pub trusted_dirs: Vec<PathBuf>,
}

/// File name of the Git executable on this OS.
pub(crate) fn git_file_name() -> String {
    format!("git{}", std::env::consts::EXE_SUFFIX)
}

/// Resolves `git` from `path_env`, never from `cwd`, relative/empty entries, `node_modules`,
/// or anything inside `excluded`. `binary` is `None` when no trusted `git` exists.
pub(crate) fn resolve(path_env: &OsStr, cwd: Option<&Path>, excluded: &[PathBuf]) -> Resolved {
    let cwd = cwd
        .and_then(|c| fs::canonicalize(c).ok())
        .map(strip_verbatim);
    let excluded: Vec<PathBuf> = excluded
        .iter()
        .filter_map(|r| fs::canonicalize(r).ok())
        .map(strip_verbatim)
        .collect();
    let untrusted = |p: &Path| -> bool {
        has_node_modules(p)
            || cwd.as_deref() == Some(p)
            || excluded.iter().any(|root| p.starts_with(root))
    };

    let mut trusted_dirs: Vec<PathBuf> = Vec::new();
    for entry in std::env::split_paths(path_env) {
        if entry.as_os_str().is_empty() || !entry.is_absolute() || has_node_modules(&entry) {
            continue;
        }
        let Ok(dir) = fs::canonicalize(&entry) else {
            continue;
        };
        let dir = strip_verbatim(dir);
        if !dir.is_dir() || untrusted(&dir) || trusted_dirs.contains(&dir) {
            continue;
        }
        trusted_dirs.push(dir);
    }

    let name = git_file_name();
    let binary = trusted_dirs.iter().find_map(|dir| {
        let candidate = dir.join(&name);
        if !candidate.is_file() {
            return None;
        }
        let real = strip_verbatim(fs::canonicalize(&candidate).ok()?);
        let parent = real.parent()?;
        (!untrusted(parent) && !untrusted(&real)).then_some(real)
    });

    Resolved {
        binary,
        trusted_dirs,
    }
}

/// Joins `dirs` into a `PATH` value (entries that can't be joined are dropped).
pub(crate) fn join_dirs(dirs: &[PathBuf]) -> OsString {
    let joinable: Vec<&PathBuf> = dirs
        .iter()
        .filter(|d| std::env::join_paths([d.as_os_str()]).is_ok())
        .collect();
    std::env::join_paths(joinable).unwrap_or_default()
}

fn has_node_modules(p: &Path) -> bool {
    p.components().any(|c| match c {
        Component::Normal(s) => s
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("node_modules")),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_modules_is_detected_case_insensitively() {
        assert!(has_node_modules(Path::new("/a/node_modules/.bin")));
        assert!(has_node_modules(Path::new("/a/Node_Modules/x")));
        assert!(!has_node_modules(Path::new("/a/node_modules_x/bin")));
    }

    #[test]
    fn empty_and_relative_entries_are_never_trusted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).expect("mkdir");
        fs::write(bin.join(git_file_name()), "").expect("write");
        let path = std::env::join_paths([OsStr::new(""), OsStr::new("."), OsStr::new("bin")])
            .expect("join");
        let r = resolve(&path, Some(tmp.path()), &[]);
        assert_eq!(r.binary, None);
        assert!(r.trusted_dirs.is_empty());
    }

    #[test]
    fn cwd_excluded_roots_and_node_modules_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mk = |rel: &str| {
            let d = tmp.path().join(rel);
            fs::create_dir_all(&d).expect("mkdir");
            fs::write(d.join(git_file_name()), "").expect("write");
            d
        };
        let cwd = mk("cwd");
        let scan = mk("scan/repo");
        let nm = mk("proj/node_modules/.bin");
        let good = mk("trusted");
        let path = std::env::join_paths([&cwd, &scan, &nm, &good]).expect("join");
        let r = resolve(&path, Some(&cwd), &[tmp.path().join("scan")]);
        let good = strip_verbatim(fs::canonicalize(good).expect("canon"));
        assert_eq!(r.binary, Some(good.join(git_file_name())));
        assert_eq!(r.trusted_dirs, vec![good]);
    }
}
