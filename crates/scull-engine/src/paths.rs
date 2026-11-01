//! Path helpers: canonical forms and protective (case-folding) containment checks.

use std::path::{Component, Path, PathBuf};

/// The canonical form of `p` used for every comparison in the engine: symlinks in the path
/// resolved (`/var` → `/private/var` on macOS), and on Windows without the `\\?\` prefix for
/// drive paths. A path that doesn't exist resolves its nearest existing ancestor and keeps
/// the rest lexically.
pub fn canon(p: &Path) -> PathBuf {
    match std::fs::canonicalize(p) {
        Ok(c) => strip_verbatim(c),
        Err(_) => match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) if parent != p => canon(parent).join(name),
            _ => p.to_path_buf(),
        },
    }
}

/// The real path of `p` when every component of it resolves (no lexical tail), else `None`.
pub fn canon_exact(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok().map(strip_verbatim)
}

/// `\\?\C:\x` → `C:\x` (UNC verbatim paths are kept as they are).
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p;
    };
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

/// Whether file names compare case-insensitively on this OS (macOS and Windows by default).
/// Containment checks for *protection* fold case there, which errs toward protecting.
///
/// This is per OS, not per volume. On a case-sensitive APFS volume folding only makes the
/// checks refuse more (two names that differ in case are treated as the same), which is safe.
/// On Linux, a case-insensitive volume (ext4 `casefold` directories, vfat, exFAT, NTFS via
/// ntfs3) is compared case-sensitively, so a differently cased spelling of a protected path
/// could be missed there. TODO: ask the platform per volume (statfs / FS_IOC_GETFLAGS) and
/// fold case on such volumes too.
const FOLD_CASE: bool = cfg!(windows) || cfg!(target_os = "macos");

fn comps(p: &Path) -> Vec<String> {
    p.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| {
            let s = c.as_os_str().to_string_lossy();
            if FOLD_CASE {
                s.to_lowercase()
            } else {
                s.into_owned()
            }
        })
        .collect()
}

/// Whether `inner` is `outer` or lies below it (component-wise, case-folded where the OS
/// folds case).
pub fn is_within(inner: &Path, outer: &Path) -> bool {
    let (i, o) = (comps(inner), comps(outer));
    i.len() >= o.len() && i[..o.len()] == o[..]
}

/// Whether `a` and `b` overlap: one is inside the other. Used for protection: a candidate
/// inside a protected path, or containing one, is protected.
pub fn overlaps(a: &Path, b: &Path) -> bool {
    is_within(a, b) || is_within(b, a)
}

/// Whether any component of `p` below `root` is named `.git`.
pub fn inside_git_dir(p: &Path, root: &Path) -> bool {
    p.strip_prefix(root)
        .map(|rel| rel.components().any(|c| c.as_os_str() == ".git"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment_is_component_wise() {
        assert!(is_within(Path::new("/a/b/c"), Path::new("/a/b")));
        assert!(is_within(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!is_within(Path::new("/a/bc"), Path::new("/a/b")));
        assert!(!is_within(Path::new("/a"), Path::new("/a/b")));
        assert!(overlaps(Path::new("/a"), Path::new("/a/b")));
        assert!(!overlaps(Path::new("/a/x"), Path::new("/a/b")));
    }

    #[test]
    fn git_dirs_are_recognized() {
        let root = Path::new("/r");
        assert!(inside_git_dir(Path::new("/r/p/.git/objects"), root));
        assert!(!inside_git_dir(Path::new("/r/p/src"), root));
        assert!(!inside_git_dir(Path::new("/elsewhere/.git"), root));
    }

    #[test]
    fn canon_keeps_missing_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = canon(dir.path());
        assert_eq!(
            canon(&dir.path().join("nope/x")),
            base.join("nope").join("x")
        );
    }
}
