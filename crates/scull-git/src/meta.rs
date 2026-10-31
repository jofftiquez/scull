//! Small, defensive helpers for reading Git metadata files.
//!
//! Everything here only reads. Files are size-capped and must be regular files, so a malicious
//! repository can't make discovery hang on a FIFO or allocate unbounded memory (THREAT_MODEL S2).

use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

/// Upper bound for any single metadata file we read (`gitdir`, `HEAD`, `locked`, `config`, ...).
/// Real files are a few hundred bytes; `config` can grow, but 1 MiB is far beyond normal use.
pub(crate) const MAX_META_BYTES: u64 = 1024 * 1024;

/// Why a metadata file could not be read as text.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// The file does not exist.
    Missing,
    /// It exists but is not a regular file (directory, FIFO, socket, device).
    NotAFile,
    /// It is larger than [`MAX_META_BYTES`].
    TooLarge,
    /// The bytes are not valid UTF-8. Paths are never guessed from lossy text.
    NotUtf8,
    /// Any other I/O error (permissions, ...).
    Io(io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Missing => f.write_str("file does not exist"),
            ReadError::NotAFile => f.write_str("not a regular file"),
            ReadError::TooLarge => write!(f, "file larger than {MAX_META_BYTES} bytes"),
            ReadError::NotUtf8 => f.write_str("contents are not valid UTF-8"),
            ReadError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Reads a small metadata file as UTF-8 text.
///
/// Follows symlinks (as Git does) but refuses anything that isn't a regular file.
pub(crate) fn read_text(path: &Path) -> Result<String, ReadError> {
    let md = match fs::metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(ReadError::Missing),
        Err(e) => return Err(ReadError::Io(e)),
    };
    if !md.is_file() {
        return Err(ReadError::NotAFile);
    }
    if md.len() > MAX_META_BYTES {
        return Err(ReadError::TooLarge);
    }
    let file = fs::File::open(path).map_err(ReadError::Io)?;
    let mut buf = Vec::new();
    file.take(MAX_META_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(ReadError::Io)?;
    if buf.len() as u64 > MAX_META_BYTES {
        return Err(ReadError::TooLarge);
    }
    String::from_utf8(buf).map_err(|_| ReadError::NotUtf8)
}

/// Whether `path` exists as a regular file (following symlinks).
pub(crate) fn is_file(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

/// Whether `path` exists as a directory (following symlinks).
pub(crate) fn is_dir(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// Existence as Git checks it for pruning: only a definite "not found" counts as missing.
/// Any other error (e.g. permission denied) is treated as present, which is the protective
/// answer (a worktree we can't see into is never a prune candidate).
pub(crate) fn definitely_missing(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(_) => false,
        Err(e) => e.kind() == io::ErrorKind::NotFound,
    }
}

/// Whether `dir` looks like a Git directory (common dir): `HEAD` file plus `objects/` and
/// `refs/` directories. This mirrors Git's own `is_git_directory` check, minus parsing HEAD.
pub(crate) fn is_git_dir(dir: &Path) -> bool {
    is_file(&dir.join("HEAD")) && is_dir(&dir.join("objects")) && is_dir(&dir.join("refs"))
}

/// Interprets a path written inside a Git metadata file.
///
/// Git for Windows writes forward slashes (`C:/Users/me/wt/.git`); those are accepted as-is by
/// `PathBuf` on Windows. Relative paths (Git's `worktree.useRelativePaths`) are resolved against
/// `base`.
pub(crate) fn path_from_meta(text: &str, base: &Path) -> PathBuf {
    let p = PathBuf::from(text);
    if p.is_absolute() { p } else { base.join(p) }
}

/// A clean, comparable form of `path`.
///
/// Canonicalizes when the path exists (resolving symlinks, `..` and, on macOS, `/var` vs
/// `/private/var`), so that two spellings of the same location compare equal. Otherwise falls
/// back to the canonical form of the nearest existing ancestor joined with the remaining
/// components, normalized lexically. Windows verbatim prefixes (`\\?\C:\`) are removed so output
/// matches what users and other tools see.
pub(crate) fn clean_path(path: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(path) {
        return strip_verbatim(c);
    }
    // Walk up to the nearest existing ancestor.
    let lexical = normalize_lexically(path);
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = lexical.as_path();
    loop {
        if let Ok(c) = fs::canonicalize(cur) {
            let mut out = strip_verbatim(c);
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return lexical,
        }
    }
}

/// Removes `.` and resolves `..` components without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\srv\share\x` → `\\srv\share\x`. No-op elsewhere.
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

/// Whether two paths name the same location, after [`clean_path`].
pub(crate) fn same_location(a: &Path, b: &Path) -> bool {
    clean_path(a) == clean_path(b)
}

/// Reads `[core] bare` from a Git config file, without executing anything.
///
/// Returns `None` when unset or unparseable. Only the minimal subset of Git's config syntax
/// needed for this key is understood; includes are deliberately not followed.
pub(crate) fn config_core_bare(config_text: &str) -> Option<bool> {
    let mut in_core = false;
    let mut result = None;
    for raw in config_text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let section = rest.split(']').next().unwrap_or("").trim();
            in_core = section.eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim())),
            None => (line, None),
        };
        if !key.eq_ignore_ascii_case("bare") {
            continue;
        }
        // Strip a trailing comment.
        let value = value.map(|v| v.split(['#', ';']).next().unwrap_or("").trim());
        result = match value {
            None => Some(true), // `bare` alone means true in Git config.
            Some(v)
                if v.eq_ignore_ascii_case("true")
                    || v.eq_ignore_ascii_case("yes")
                    || v.eq_ignore_ascii_case("on")
                    || v == "1" =>
            {
                Some(true)
            }
            Some(v)
                if v.eq_ignore_ascii_case("false")
                    || v.eq_ignore_ascii_case("no")
                    || v.eq_ignore_ascii_case("off")
                    || v == "0"
                    || v.is_empty() =>
            {
                Some(false)
            }
            Some(_) => result,
        };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_bare_parsing() {
        assert_eq!(config_core_bare("[core]\n\tbare = true\n"), Some(true));
        assert_eq!(config_core_bare("[core]\n\tbare = false\n"), Some(false));
        assert_eq!(config_core_bare("[Core]\nBare\n"), Some(true));
        assert_eq!(config_core_bare("[core]\nbare = yes ; c\n"), Some(true));
        assert_eq!(config_core_bare("[user]\nbare = true\n"), None);
        assert_eq!(config_core_bare(""), None);
    }

    #[test]
    fn lexical_normalization() {
        let p = normalize_lexically(Path::new("/a/b/./c/../../d"));
        assert_eq!(p, PathBuf::from("/a/d"));
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\Users\me")),
            PathBuf::from(r"C:\Users\me")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share")),
            PathBuf::from(r"\\srv\share")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from("/home/me")),
            PathBuf::from("/home/me")
        );
    }
}
