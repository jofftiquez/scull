//! Nothing is created outside the fixture (P2-EXEC-13).
//!
//! The places scull would put things on its own, above a fixture's temp dir, are snapshotted
//! when the fixture is created and compared by [`check_invariants`](crate::check_invariants)
//! (as part of I-2: nothing outside the root changes except as allowed):
//!
//! * the automatic quarantine location: `<ancestor>/.scull-quarantine` for every ancestor of
//!   the temp dir (the platform picks the shallowest one the user owns, e.g. `/tmp` on Linux,
//!   `/var/folders/xx/<user>` on macOS, `C:\Users\<user>` on Windows);
//! * per-volume Trash directories in those ancestors: `.Trash`, `.Trash-<uid>`, `.Trashes`,
//!   `$Recycle.Bin`.
//!
//! Only names are read (three levels deep, bounded), never contents, and nothing is written.
//! Entries that appear there during the test are a failure. Entries that disappear are not
//! reported (other programs may empty a Trash). The recorded quarantine paths of audited
//! operations ([`InvariantInputs::audit`](crate::InvariantInputs::audit),
//! [`InvariantInputs::quarantine`](crate::InvariantInputs::quarantine)) must lie inside the
//! temp dir.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// How deep below each watched directory names are recorded (`.scull-quarantine/<op>/<entry>`,
/// `.Trash-<uid>/files/<name>`).
const DEPTH: usize = 3;

/// Upper bound on recorded names per fixture, so a huge Trash can't slow every test down.
const MAX_NAMES: usize = 20_000;

/// Whether `name` (a direct child of an ancestor) is a place scull may create things in.
fn watched(name: &str) -> bool {
    name == QUARANTINE_DIR || name.starts_with(".Trash") || name == "$Recycle.Bin"
}

/// `scull_platform::quarantine::QUARANTINE_DIR_NAME` (not a dependency of this crate).
const QUARANTINE_DIR: &str = ".scull-quarantine";

/// Names under the watched directories above a fixture's temp dir.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OutsideSnapshot {
    names: BTreeSet<PathBuf>,
}

impl OutsideSnapshot {
    /// Snapshot the watched directories of every ancestor of `base` (not `base` itself).
    pub(crate) fn capture(base: &Path) -> Self {
        let mut names = BTreeSet::new();
        for ancestor in base.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            // `.scull-quarantine` is recorded even when absent, so its creation shows.
            let quarantine = ancestor.join(QUARANTINE_DIR);
            let Ok(rd) = std::fs::read_dir(ancestor) else {
                continue;
            };
            let mut dirs: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| e.file_name().to_str().is_some_and(watched))
                .map(|e| e.path())
                .collect();
            if !dirs.contains(&quarantine) && std::fs::symlink_metadata(&quarantine).is_ok() {
                dirs.push(quarantine);
            }
            for d in dirs {
                names.insert(d.clone());
                walk(&d, DEPTH, &mut names);
            }
        }
        Self { names }
    }

    /// What appeared in `after` that was not here before.
    pub(crate) fn added<'a>(&self, after: &'a OutsideSnapshot) -> Vec<&'a Path> {
        after
            .names
            .iter()
            .filter(|n| !self.names.contains(*n))
            .map(PathBuf::as_path)
            .collect()
    }
}

fn walk(dir: &Path, depth: usize, out: &mut BTreeSet<PathBuf>) {
    if depth == 0 || out.len() >= MAX_NAMES {
        return;
    }
    // Never follow a link out of the watched directory.
    if std::fs::symlink_metadata(dir).is_ok_and(|m| !m.is_dir()) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if out.len() >= MAX_NAMES {
            return;
        }
        let p = e.path();
        out.insert(p.clone());
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&p, depth - 1, out);
        }
    }
}

/// Whether `path` is inside `base` (component-wise).
pub(crate) fn inside(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn additions_below_a_watched_ancestor_dir_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("a").join("fixture");
        std::fs::create_dir_all(&base).unwrap();
        let before = OutsideSnapshot::capture(&base);
        // Created beside the fixture, in its parent: the automatic quarantine location.
        let q = tmp.path().join("a").join(".scull-quarantine").join("op-1");
        std::fs::create_dir_all(&q).unwrap();
        std::fs::write(tmp.path().join("a").join("unrelated.txt"), "x").unwrap();
        let after = OutsideSnapshot::capture(&base);
        let added = before.added(&after);
        assert!(added.contains(&q.as_path()), "{added:?}");
        assert!(
            added.iter().all(|p| !p.ends_with("unrelated.txt")),
            "{added:?}"
        );
        // Inside the fixture: not watched.
        std::fs::create_dir_all(base.join(".scull-quarantine").join("op-2")).unwrap();
        let again = OutsideSnapshot::capture(&base);
        assert_eq!(after.added(&again), Vec::<&Path>::new());
    }
}
