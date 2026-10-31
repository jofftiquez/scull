//! Last observed activity in a worktree: the newest modification time, sampled with a bound
//! (PRD §8.4, GIT-02).
//!
//! Metadata comes from [`scull_platform::Platform`] (no OS APIs here, PLT-11): links are never
//! followed and special files are never opened. The walk is breadth-first, so a bounded sample
//! covers the shallow levels (where people edit) before deep dependency trees.

use crate::state::Observed;
use schemars::JsonSchema;
use scull_core::FsPath;
use scull_platform::{EntryKind, Platform};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;

/// Default bound on the number of entries [`sample_activity`] looks at.
pub const DEFAULT_ACTIVITY_SAMPLE: usize = 20_000;

/// The newest modification observed inside a worktree.
///
/// Missing evidence never counts as evidence of inactivity (PRD §8.4): when the sample is
/// incomplete, the true newest modification may be later than [`Activity::newest_unix`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Activity {
    /// Unix timestamp (seconds, UTC) of the newest modification time seen, if any entry had one.
    pub newest_unix: Option<i64>,
    /// The entry that had it.
    pub newest_path: Option<FsPath>,
    /// Entries whose metadata was read.
    pub entries_sampled: u64,
    /// Whether every entry was looked at. `false` when the bound was hit or some folder or entry
    /// couldn't be read: the newest modification is then a lower bound.
    pub complete: bool,
}

/// Samples the newest modification time under `root` (included), reading at most `max_entries`
/// entries through `platform`. Never follows links.
///
/// Returns [`Observed::Unknown`] when `root` itself can't be read or isn't a folder.
pub fn sample_activity(
    platform: &dyn Platform,
    root: &Path,
    max_entries: usize,
) -> Observed<Activity> {
    let meta = match platform.metadata(root) {
        Ok(m) => m,
        Err(e) => return Observed::unknown(format!("can't read worktree folder: {e}")),
    };
    if meta.kind != EntryKind::Dir {
        return Observed::unknown(format!(
            "{} is not a folder ({})",
            root.display(),
            meta.kind.as_str()
        ));
    }
    let mut act = Activity {
        newest_unix: None,
        newest_path: None,
        entries_sampled: 1,
        complete: true,
    };
    let note = |act: &mut Activity, mtime: Option<i64>, path: &Path| {
        if let Some(t) = mtime
            && act.newest_unix.is_none_or(|n| t > n)
        {
            act.newest_unix = Some(t);
            act.newest_path = Some(FsPath::from_path(path));
        }
    };
    note(&mut act, meta.mtime_unix, root);

    let mut queue = VecDeque::from([root.to_path_buf()]);
    'walk: while let Some(dir) = queue.pop_front() {
        let entries = match platform.read_dir(&dir) {
            Ok(it) => it,
            Err(_) => {
                act.complete = false;
                continue;
            }
        };
        for entry in entries {
            if act.entries_sampled >= max_entries as u64 {
                act.complete = false;
                break 'walk;
            }
            let Ok(entry) = entry else {
                act.complete = false;
                continue;
            };
            act.entries_sampled += 1;
            match platform.metadata(&entry.path) {
                Ok(m) => note(&mut act, m.mtime_unix, &entry.path),
                Err(_) => act.complete = false,
            }
            if entry.kind == EntryKind::Dir {
                queue.push_back(entry.path);
            }
        }
    }
    Observed::known(act)
}
