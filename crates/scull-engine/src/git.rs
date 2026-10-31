//! Git signals: whether anything inside a candidate is tracked (PRD §8.3 first row), and the
//! worktree analysis (GIT-02/03) over every repository the walk met.

use crate::paths::is_within;
use scull_core::Evidence;
use scull_git::{GitRunner, IgnoredMode, Status, StatusOptions, UntrackedMode};
use scull_safety::GitTracking;
use scull_scanner::DirSummary;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Signal name for Git observations.
pub const GIT_SIGNAL: &str = "git";

/// Decides Git tracking for candidate directories, one `git status` per work tree.
///
/// A directory holds no tracked file exactly when Git lists it (or a folder above it) as a
/// whole untracked (`?? dir/`) or ignored (`!! dir/`) entry: Git never collapses a folder that
/// has entries in the index. A non-empty folder Git does not collapse therefore holds tracked
/// files. Anything Git can't answer is [`GitTracking::Unknown`] (S2), never "not tracked".
pub(crate) struct Tracking<'a> {
    runner: Option<&'a GitRunner>,
    /// Work-tree roots (folders holding `.git`), longest first.
    roots: Vec<PathBuf>,
    status: HashMap<PathBuf, Result<Status, String>>,
}

impl<'a> Tracking<'a> {
    pub fn new(runner: Option<&'a GitRunner>, roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut roots: Vec<PathBuf> = roots.into_iter().collect();
        roots.sort_by_key(|r| std::cmp::Reverse(r.components().count()));
        Self {
            runner,
            roots,
            status: HashMap::new(),
        }
    }

    /// The innermost work tree containing `path` (or `path` itself).
    pub fn worktree_of(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .find(|r| is_within(path, r))
            .map(PathBuf::as_path)
    }

    /// Git tracking of `dir`, with evidence explaining an unknown or tracked answer.
    pub fn of(&mut self, dir: &Path, summary: Option<&DirSummary>) -> (GitTracking, Vec<Evidence>) {
        let Some(wt) = self.worktree_of(dir).map(Path::to_path_buf) else {
            return (GitTracking::NotTracked, Vec::new());
        };
        if wt == dir {
            // The candidate is itself a work tree (holds `.git`): everything in it may be
            // tracked.
            return (
                GitTracking::Tracked { files: 1 },
                vec![Evidence::Note {
                    text: "contains a Git repository (`.git`)".into(),
                }],
            );
        }
        let Some(runner) = self.runner else {
            return unknown(
                "`git` was not found on a trusted PATH entry, so Git tracking could not be checked",
            );
        };
        let status = self.status.entry(wt.clone()).or_insert_with(|| {
            runner
                .status(
                    &wt,
                    &StatusOptions {
                        untracked: UntrackedMode::Normal,
                        ignored: IgnoredMode::Traditional,
                    },
                )
                .map_err(|e| e.to_string())
        });
        let status = match status {
            Ok(s) => s,
            Err(e) => return unknown(&format!("`git status` failed in {}: {e}", wt.display())),
        };
        let Some(rel) = rel_bytes(dir, &wt) else {
            return unknown("the path is not valid UTF-8 relative to its work tree");
        };
        let collapsed = status
            .untracked()
            .chain(status.ignored())
            .filter(|e| e.path.is_dir())
            .any(|e| {
                let p = e.path.as_bytes();
                let p = p.strip_suffix(b"/").unwrap_or(p);
                rel == p || (rel.starts_with(p) && rel.get(p.len()) == Some(&b'/'))
            });
        if collapsed {
            return (GitTracking::NotTracked, Vec::new());
        }
        let Some(s) = summary else {
            return unknown("the folder was not measured, so Git tracking could not be decided");
        };
        if s.totals.files == 0 && s.totals.links == 0 {
            // Git tracks files and links only; an empty folder holds nothing tracked. But "no
            // files seen" is only "no files" when the scanner saw everything: an unreadable,
            // timed-out or not-descended folder inside may hide tracked files.
            if !s.complete || s.totals.boundaries > 0 {
                return unknown(
                    "part of the folder could not be read or was not descended, so Git \
                     tracking could not be decided",
                );
            }
            return (GitTracking::NotTracked, Vec::new());
        }
        let changed = status
            .tracked_changes()
            .filter(|e| {
                let p = e.path.as_bytes();
                p.starts_with(&rel) && p.get(rel.len()) == Some(&b'/')
            })
            .count() as u64;
        let files = changed.max(1);
        (
            GitTracking::Tracked { files },
            vec![Evidence::GitTracked { files }],
        )
    }
}

fn unknown(reason: &str) -> (GitTracking, Vec<Evidence>) {
    (
        GitTracking::Unknown,
        vec![Evidence::SignalUnavailable {
            signal: GIT_SIGNAL.into(),
            reason: reason.to_owned(),
        }],
    )
}

/// `dir` relative to `wt` as Git spells it (`a/b`), or `None` if not UTF-8.
fn rel_bytes(dir: &Path, wt: &Path) -> Option<Vec<u8>> {
    let rel = dir.strip_prefix(wt).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(n) => parts.push(n.to_str()?.to_owned()),
            _ => return None,
        }
    }
    Some(parts.join("/").into_bytes())
}

/// Maps the safety crate's tri-state onto the Git crate's.
pub(crate) fn git_tri(t: scull_safety::Tri) -> scull_git::Tri {
    match t {
        scull_safety::Tri::No => scull_git::Tri::No,
        scull_safety::Tri::Yes => scull_git::Tri::Yes,
        scull_safety::Tri::Unknown => scull_git::Tri::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_use_forward_slashes() {
        let wt = Path::new("/r/repo");
        assert_eq!(
            rel_bytes(&wt.join("web").join("dist"), wt),
            Some(b"web/dist".to_vec())
        );
        assert_eq!(rel_bytes(Path::new("/elsewhere"), wt), None);
    }

    #[test]
    fn innermost_worktree_wins() {
        let t = Tracking::new(None, [PathBuf::from("/r"), PathBuf::from("/r/sub")]);
        assert_eq!(
            t.worktree_of(Path::new("/r/sub/x")),
            Some(Path::new("/r/sub"))
        );
        assert_eq!(t.worktree_of(Path::new("/r/x")), Some(Path::new("/r")));
        assert_eq!(t.worktree_of(Path::new("/q")), None);
    }

    #[test]
    fn without_git_tracking_is_unknown_inside_a_repo() {
        let mut t = Tracking::new(None, [PathBuf::from("/r")]);
        let (g, ev) = t.of(Path::new("/r/target"), None);
        assert_eq!(g, GitTracking::Unknown);
        assert!(
            matches!(&ev[0], Evidence::SignalUnavailable { signal, .. } if signal == GIT_SIGNAL)
        );
        let (g, ev) = t.of(Path::new("/q/target"), None);
        assert_eq!(g, GitTracking::NotTracked);
        assert!(ev.is_empty());
    }
}
