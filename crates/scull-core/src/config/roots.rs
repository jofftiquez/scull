//! Scan root validation (CFG-04) and `~` expansion.

use crate::error::{ErrorKind, ScullError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf, Prefix};

/// Why a scan root needs explicit confirmation before scanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootConcern {
    /// A filesystem or volume root (`/`, `C:\`, or one the caller listed).
    FilesystemRoot,
    /// The user's home directory itself.
    HomeRoot,
    /// A parent of the home directory (e.g. `/home`, `C:\Users`).
    ContainsHome,
}

/// A scan root that must not be scanned without the user's explicit confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RootConfirmation {
    /// The root as given.
    pub root: PathBuf,
    /// Why it needs confirmation.
    pub concern: RootConcern,
    /// Human explanation for the prompt.
    pub message: String,
}

/// Replace a leading `~` (alone, or followed by `/` or `\`) with `home`. Other paths, including
/// `~user/...`, are returned unchanged.
pub fn expand_home(path: &Path, home: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if s == "~" {
        return home.to_path_buf();
    }
    match s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        Some(rest) => home.join(rest),
        None => path.to_path_buf(),
    }
}

/// A comparable form of one path component: ASCII case-folded (so matching errs toward
/// flagging on case-insensitive filesystems) with `\\?\C:` and `C:` treated alike.
fn norm(c: Component<'_>) -> Option<String> {
    match c {
        Component::CurDir => None,
        Component::RootDir => Some("/".into()),
        Component::Prefix(p) => Some(match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                format!("{}:", char::from(d).to_ascii_uppercase())
            }
            _ => p.as_os_str().to_string_lossy().to_ascii_lowercase(),
        }),
        other => Some(other.as_os_str().to_string_lossy().to_ascii_lowercase()),
    }
}

fn components(p: &Path) -> Vec<String> {
    p.components().filter_map(norm).collect()
}

/// Whether `p` has no named components (`/`, `C:\`, `\\server\share\`).
fn is_bare_root(p: &Path) -> bool {
    !p.components().any(|c| matches!(c, Component::Normal(_)))
}

/// Check scan roots before scanning (CFG-04).
///
/// * `roots` must be non-empty, absolute, `~`-expanded and free of `..` (callers canonicalize
///   first); otherwise an [`ErrorKind::Usage`] error (exit code 2).
/// * Every root that is a filesystem root (no named components, or equal to one of `fs_roots`,
///   e.g. mount points from `scull-platform`), the `home` directory, or an ancestor of `home`
///   is returned as a [`RootConfirmation`]. The caller must get explicit confirmation for each
///   (an interactive prompt or a flag) before scanning it, and must never add such a root by
///   default.
///
/// Comparison is lexical and ASCII case-insensitive, erring toward asking. Protection itself
/// is enforced by identity in `scull-safety` (SAFE-03, SAFE-13); this check is about scope.
pub fn validate_roots(
    roots: &[PathBuf],
    home: &Path,
    fs_roots: &[PathBuf],
) -> Result<Vec<RootConfirmation>, ScullError> {
    if roots.is_empty() {
        return Err(ScullError::new(
            ErrorKind::Usage,
            "no scan roots: pass a directory to scan or set scan.roots in the config file",
        ));
    }
    let home_c = components(home);
    let fs_c: Vec<Vec<String>> = fs_roots.iter().map(|p| components(p)).collect();
    let mut out = Vec::new();
    for root in roots {
        let shown = root.display();
        if root.to_str().is_some_and(|s| s.starts_with('~')) {
            return Err(ScullError::new(
                ErrorKind::Usage,
                format!(
                    "scan root `{shown}` starts with `~`; expand it to the home directory first"
                ),
            ));
        }
        if !root.is_absolute() {
            return Err(ScullError::new(
                ErrorKind::Usage,
                format!("scan root `{shown}` is not an absolute path"),
            ));
        }
        if root.components().any(|c| c == Component::ParentDir) {
            return Err(ScullError::new(
                ErrorKind::Usage,
                format!("scan root `{shown}` contains `..`; pass a normalized path"),
            ));
        }
        let rc = components(root);
        let concern = if is_bare_root(root) || fs_c.contains(&rc) {
            Some((
                RootConcern::FilesystemRoot,
                format!("`{shown}` is a filesystem root; scanning it needs explicit confirmation"),
            ))
        } else if rc == home_c {
            Some((
                RootConcern::HomeRoot,
                format!(
                    "`{shown}` is your home directory; scanning all of it needs explicit confirmation"
                ),
            ))
        } else if home_c.len() > rc.len() && home_c.starts_with(&rc) {
            Some((
                RootConcern::ContainsHome,
                format!(
                    "`{shown}` contains your home directory; scanning it needs explicit confirmation"
                ),
            ))
        } else {
            None
        };
        if let Some((concern, message)) = concern {
            out.push(RootConfirmation {
                root: root.clone(),
                concern,
                message,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_only_touches_leading_tilde() {
        let home = Path::new("/home/me");
        assert_eq!(expand_home(Path::new("~"), home), home);
        assert_eq!(expand_home(Path::new("~/p"), home), home.join("p"));
        assert_eq!(expand_home(Path::new("~bob/p"), home), Path::new("~bob/p"));
        assert_eq!(expand_home(Path::new("/x/~"), home), Path::new("/x/~"));
    }

    #[test]
    fn components_are_normalized() {
        assert_eq!(components(Path::new("/")), vec!["/".to_string()]);
        assert_eq!(
            components(Path::new("/Home/./Me")),
            vec!["/".to_string(), "home".into(), "me".into()]
        );
    }
}
