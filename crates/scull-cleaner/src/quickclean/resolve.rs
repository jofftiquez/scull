//! Trusted resolution of package-manager binaries (SAFE-12, T-22).
//!
//! A local copy of the approach of `scull-git`'s `resolve.rs` (crate-private there),
//! generalized to any tool name. The duplication is deliberate for now: extracting a shared
//! helper (e.g. into `scull-platform`) would touch two other lanes.
//!
//! The search list (normally the user's `PATH`, injected by the caller so tests can point it
//! at a temp dir) is walked explicitly instead of letting the OS search. An entry is skipped
//! when it is:
//!
//! * empty or relative (`""`, `.`, `bin`): these resolve against the current directory, which
//!   may be a scan root or a malicious checkout;
//! * the current directory itself;
//! * inside a `node_modules` folder (`node_modules/.bin` is package-controlled);
//! * inside one of the caller's excluded roots (the scan roots);
//! * not an existing directory.
//!
//! The candidate binary is canonicalized and re-checked, so a symlink in a trusted directory
//! that points into an excluded root, the current directory or a `node_modules` folder is
//! rejected too. Only `<tool>` (Unix) / `<tool>.exe` (Windows) is accepted: never a
//! `.cmd`/`.bat`/`.ps1` shim, which would need a shell. Such shims are reported so the
//! preview can say why the tool was skipped.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Script shims that would need a shell to run (Windows npm/pnpm/yarn installs ship these).
const SHELL_SHIMS: [&str; 3] = ["cmd", "bat", "ps1"];

/// The outcome of looking one tool up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// A trusted executable: its canonical absolute path.
    Found(PathBuf),
    /// No trusted executable, but a script shim that needs a shell exists in a trusted
    /// directory (SAFE-12: no shell); its path.
    ShellShimOnly(PathBuf),
    /// No trusted executable at all.
    NotFound,
}

/// The trusted directories of a search list, resolved once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedDirs {
    dirs: Vec<PathBuf>,
    untrusted: Untrusted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Untrusted {
    cwd: Option<PathBuf>,
    excluded: Vec<PathBuf>,
}

impl Untrusted {
    fn contains(&self, p: &Path) -> bool {
        has_node_modules(p)
            || self.cwd.as_deref() == Some(p)
            || self.excluded.iter().any(|root| p.starts_with(root))
    }
}

impl TrustedDirs {
    /// Filters `search_path` (a `PATH`-style list) as described in the module docs. `cwd` is
    /// the process's current directory and `excluded` the scan roots.
    pub fn new(search_path: &OsStr, cwd: Option<&Path>, excluded: &[PathBuf]) -> Self {
        let untrusted = Untrusted {
            cwd: cwd.and_then(canonical),
            excluded: excluded.iter().filter_map(|r| canonical(r)).collect(),
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in std::env::split_paths(search_path) {
            if entry.as_os_str().is_empty() || !entry.is_absolute() || has_node_modules(&entry) {
                continue;
            }
            let Some(dir) = canonical(&entry) else {
                continue;
            };
            if !dir.is_dir() || untrusted.contains(&dir) || dirs.contains(&dir) {
                continue;
            }
            dirs.push(dir);
        }
        Self { dirs, untrusted }
    }

    /// The trusted directories, in search order (also the child's `PATH`).
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// The directories joined into a `PATH` value (entries that can't be joined are dropped).
    pub fn joined(&self) -> OsString {
        let joinable: Vec<&PathBuf> = self
            .dirs
            .iter()
            .filter(|d| std::env::join_paths([d.as_os_str()]).is_ok())
            .collect();
        std::env::join_paths(joinable).unwrap_or_default()
    }

    /// Looks `tool` (a bare name such as `npm`) up in the trusted directories.
    pub fn find(&self, tool: &str) -> Lookup {
        if tool.is_empty() || tool.contains(['/', '\\']) || tool.starts_with('.') {
            return Lookup::NotFound;
        }
        let name = format!("{tool}{}", std::env::consts::EXE_SUFFIX);
        for dir in &self.dirs {
            let candidate = dir.join(&name);
            if !candidate.is_file() {
                continue;
            }
            let Some(real) = canonical(&candidate) else {
                continue;
            };
            let parent_ok = real.parent().is_some_and(|p| !self.untrusted.contains(p));
            if parent_ok && !self.untrusted.contains(&real) && real.is_file() {
                return Lookup::Found(real);
            }
        }
        if cfg!(windows) {
            for dir in &self.dirs {
                for ext in SHELL_SHIMS {
                    let shim = dir.join(format!("{tool}.{ext}"));
                    if shim.is_file() {
                        return Lookup::ShellShimOnly(shim);
                    }
                }
            }
        }
        Lookup::NotFound
    }
}

/// Canonical form of an existing path, without Windows' verbatim prefix.
pub(crate) fn canonical(p: &Path) -> Option<PathBuf> {
    fs::canonicalize(p).ok().map(strip_verbatim)
}

/// Removes Windows' `\\?\` prefix from a canonicalized path where it isn't needed.
pub(crate) fn strip_verbatim(p: PathBuf) -> PathBuf {
    let Some(s) = p.to_str() else { return p };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return PathBuf::from(rest);
        }
    }
    p
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

    fn exe(tool: &str) -> String {
        format!("{tool}{}", std::env::consts::EXE_SUFFIX)
    }

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
        fs::write(bin.join(exe("npm")), "").expect("write");
        let path = std::env::join_paths([OsStr::new(""), OsStr::new("."), OsStr::new("bin")])
            .expect("join");
        let t = TrustedDirs::new(&path, Some(tmp.path()), &[]);
        assert_eq!(t.find("npm"), Lookup::NotFound);
        assert!(t.dirs().is_empty());
    }

    #[test]
    fn cwd_excluded_roots_and_node_modules_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mk = |rel: &str| {
            let d = tmp.path().join(rel);
            fs::create_dir_all(&d).expect("mkdir");
            fs::write(d.join(exe("npm")), "").expect("write");
            d
        };
        let cwd = mk("cwd");
        let scan = mk("scan/repo");
        let nm = mk("proj/node_modules/.bin");
        let good = mk("trusted");
        let path = std::env::join_paths([&cwd, &scan, &nm, &good]).expect("join");
        let t = TrustedDirs::new(&path, Some(&cwd), &[tmp.path().join("scan")]);
        let good = canonical(&good).expect("canon");
        assert_eq!(t.find("npm"), Lookup::Found(good.join(exe("npm"))));
        assert_eq!(t.dirs(), [good]);
    }

    #[test]
    fn names_with_separators_are_never_looked_up() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = std::env::join_paths([tmp.path()]).expect("join");
        let t = TrustedDirs::new(&path, None, &[]);
        for bad in ["", "../npm", "a/npm", r"a\npm", ".npm"] {
            assert_eq!(t.find(bad), Lookup::NotFound, "{bad}");
        }
    }
}
