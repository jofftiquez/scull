//! Restore from quarantine with integrity, never overwriting (SAFE-16, T-32; P2-EXEC-06).
//!
//! The **database manifest is authoritative**: restore reads only the `quarantine_manifest`
//! row. The on-disk `manifest.json` copy is never read, so editing it (e.g. to point restore
//! at `~/.ssh`) changes nothing (FX-EXEC-05). Before anything moves:
//!
//! 1. the recorded paths must be usable: absolute, valid Unicode, and the quarantine path
//!    must be an entry of `.scull-quarantine/<operation-id>/` for the entry's own operation;
//! 2. the operation folder must still be owner-only;
//! 3. the quarantined entry is checked (no-follow) against the manifest: same identity, same
//!    kind, same logical size;
//! 4. the original parent directory must still be the **same directory object** the entry was
//!    moved out of (recorded identity), opened no-follow: a parent replaced by a link to
//!    somewhere else, or by another directory, is refused.
//!
//! Then one atomic, same-volume, no-replace rename moves the entry back to its recorded path.
//! If that path exists, it is restored to the first free sibling `<name>.scull-restored-<n>`
//! instead: nothing is ever overwritten (FX-EXEC-04). Restore is idempotent: an entry that was
//! already restored is a no-op, and an entry found already moved back (e.g. after a crash
//! between the move and the database update) is recorded as restored. Every attempt, done,
//! no-op or refused, is written to the append-only quarantine log.

use crate::executor::Executor;
use crate::quarantine::{kind_from_code, measure};
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_platform::quarantine::{MoveError, MoveRequest, QUARANTINE_DIR_NAME, move_no_replace};
use scull_platform::{EntryKind, Native, Platform, ensure_owner_only};
use scull_storage::{QuarantineEntry, QuarantineEvent, QuarantineLogEntry, Store};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Suffix of a sibling restore path: `<name>.scull-restored-<n>`.
pub const RESTORED_SUFFIX: &str = ".scull-restored-";

/// Highest sibling number tried before giving up.
const MAX_SIBLINGS: u32 = 1000;

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestoreOutcome {
    /// Moved back. `sibling` is true when the original path was taken and the entry went to
    /// `<name>.scull-restored-<n>` instead.
    Restored {
        /// Where it is now.
        to: PathBuf,
        /// Whether that is a sibling of the original path.
        sibling: bool,
    },
    /// It was already restored (no-op).
    AlreadyRestored {
        /// Where it was restored to.
        to: FsPath,
    },
}

/// The result of [`Executor::restore`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RestoreReport {
    /// The manifest entry.
    pub entry_id: i64,
    /// The recorded original path.
    pub original_path: FsPath,
    /// What happened.
    pub outcome: RestoreOutcome,
}

/// A refused restore: nothing moved. `code` is stable (for the log and reports).
fn refused(
    code: &'static str,
    kind: ErrorKind,
    msg: impl Into<String>,
) -> (&'static str, ScullError) {
    (code, ScullError::new(kind, msg))
}

/// `<name>.scull-restored-<n>`.
fn sibling_name(name: &OsStr, n: u32) -> OsString {
    let mut s = name.to_owned();
    s.push(format!("{RESTORED_SUFFIX}{n}"));
    s
}

/// The recorded paths, checked: (original, original parent, original name, quarantine
/// folder, quarantine entry name).
type Checked = (PathBuf, PathBuf, OsString, PathBuf, OsString);

fn check_paths(e: &QuarantineEntry) -> Result<Checked, (&'static str, ScullError)> {
    let bad = |what: &str| {
        refused(
            "recorded_path_unusable",
            ErrorKind::SafetyBlocked,
            format!("the recorded {what} can't be used for a restore"),
        )
    };
    let (orig, quar) = (&e.entry.original_path, &e.entry.quarantine_path);
    if orig.is_lossy() || quar.is_lossy() {
        return Err(bad("path (not valid Unicode)"));
    }
    let original = orig.to_path_buf();
    let quarantined = quar.to_path_buf();
    if !original.is_absolute() || !quarantined.is_absolute() {
        return Err(bad("path (not absolute)"));
    }
    let (Some(parent), Some(name)) = (original.parent(), original.file_name()) else {
        return Err(bad("original path"));
    };
    let (Some(folder), Some(entry_name)) = (quarantined.parent(), quarantined.file_name()) else {
        return Err(bad("quarantine path"));
    };
    if let Some(why) = crate::quarantine::outside_test_sandbox(&quarantined)
        .or_else(|| crate::quarantine::outside_test_sandbox(&original))
    {
        return Err(refused(
            "outside_test_sandbox",
            ErrorKind::SafetyBlocked,
            why,
        ));
    }
    let in_quarantine = folder.file_name() == Some(OsStr::new(e.entry.operation_id.as_str()))
        && folder.parent().and_then(Path::file_name) == Some(OsStr::new(QUARANTINE_DIR_NAME));
    if !in_quarantine {
        return Err(bad(
            "quarantine path (not inside the operation's quarantine folder)",
        ));
    }
    Ok((
        original.clone(),
        parent.to_path_buf(),
        name.to_owned(),
        folder.to_path_buf(),
        entry_name.to_owned(),
    ))
}

impl Executor {
    /// Restore one quarantine entry (by manifest entry ID) under the execution lock. See the
    /// module docs for every check. `Err` means nothing moved (and the refusal was logged).
    pub fn restore(
        &self,
        store: &mut Store,
        entry_id: i64,
        now: i64,
    ) -> Result<RestoreReport, ScullError> {
        if self.lock().is_none() {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "restoring needs the execution lock (SAFE-09)",
            ));
        }
        let Some(entry) = store.quarantine_entry(entry_id).map_err(ScullError::from)? else {
            return Err(ScullError::new(
                ErrorKind::NotFound,
                format!("no quarantine entry {entry_id} (or it predates the integrity facts)"),
            ));
        };
        let log = |store: &mut Store, event, path: Option<FsPath>, reason: Option<&str>| {
            store
                .log_quarantine_event(&QuarantineLogEntry {
                    entry_id,
                    recorded_at: now,
                    event,
                    path,
                    reason: reason.map(str::to_owned),
                    bytes: None,
                })
                .map_err(ScullError::from)
        };
        if let Some((_, to)) = &entry.restored {
            log(store, QuarantineEvent::RestoreNoop, Some(to.clone()), None)?;
            return Ok(RestoreReport {
                entry_id,
                original_path: entry.entry.original_path.clone(),
                outcome: RestoreOutcome::AlreadyRestored { to: to.clone() },
            });
        }
        match restore_held(&entry) {
            Ok((to, sibling)) => {
                let to_fs = FsPath::from_path(&to);
                store
                    .mark_quarantine_restored(entry_id, now, &to_fs)
                    .map_err(|e| {
                        ScullError::new(
                            ErrorKind::Io,
                            format!(
                                "restored to {} but the manifest could not be updated ({e}); \
                                 restoring again records it",
                                to.display()
                            ),
                        )
                    })?;
                log(store, QuarantineEvent::Restored, Some(to_fs), None)?;
                Ok(RestoreReport {
                    entry_id,
                    original_path: entry.entry.original_path.clone(),
                    outcome: RestoreOutcome::Restored { to, sibling },
                })
            }
            Err((code, err)) => {
                log(store, QuarantineEvent::RestoreRefused, None, Some(code))?;
                Err(err.with_path(entry.entry.original_path.clone()))
            }
        }
    }
}

/// The checks and the move for an entry that is still held. Returns where it went.
fn restore_held(e: &QuarantineEntry) -> Result<(PathBuf, bool), (&'static str, ScullError)> {
    if e.purged.is_some() {
        return Err(refused(
            "purged",
            ErrorKind::NotFound,
            "the entry was purged; there is nothing to restore",
        ));
    }
    let (original, parent, name, folder, entry_name) = check_paths(e)?;
    let quarantined = folder.join(&entry_name);
    let kind = kind_from_code(&e.entry.entry_kind).ok_or_else(|| {
        refused(
            "manifest_invalid",
            ErrorKind::Internal,
            "the recorded entry kind is unknown",
        )
    })?;

    // The quarantined entry, checked against the manifest (no-follow).
    let md = match Native.metadata(&quarantined) {
        Ok(md) => md,
        Err(err) if err.is_not_found() => {
            // Already moved back (a crash after the move)? Look for it, by identity, at the
            // recorded path and its restore siblings.
            return find_restored(e, &original, &parent, &name).ok_or_else(|| {
                refused(
                    "missing",
                    ErrorKind::NotFound,
                    "the quarantined entry is missing and was not found at its original path",
                )
            });
        }
        Err(err) => {
            return Err(refused(
                "unreadable",
                ErrorKind::Io,
                format!("cannot inspect the quarantined entry: {err}"),
            ));
        }
    };
    if md.identity != Some(e.entry.entry_identity) || md.kind != kind {
        return Err(refused(
            "integrity_failed",
            ErrorKind::SafetyBlocked,
            "the quarantined entry is not the one recorded (identity or kind changed)",
        ));
    }
    let (logical, _) = measure(&quarantined);
    if logical != e.entry.logical_bytes {
        return Err(refused(
            "integrity_failed",
            ErrorKind::SafetyBlocked,
            format!(
                "the quarantined entry was modified ({} bytes recorded, {logical} found)",
                e.entry.logical_bytes
            ),
        ));
    }
    if let Err(err) = ensure_owner_only(&folder) {
        return Err(refused(
            "quarantine_exposed",
            ErrorKind::SafetyBlocked,
            format!(
                "the quarantine folder is not private to you, so it may have been tampered with: {err}"
            ),
        ));
    }

    // The original parent: the same directory object, never a link.
    match Native.metadata(&parent) {
        Ok(p) if p.kind == EntryKind::Dir && p.identity == Some(e.entry.parent_identity) => {}
        Ok(_) => {
            return Err(refused(
                "parent_changed",
                ErrorKind::SafetyBlocked,
                "the original folder was replaced since the item was quarantined; nothing is \
                 restored into a different folder",
            ));
        }
        Err(err) => {
            return Err(refused(
                "parent_missing",
                ErrorKind::NotFound,
                format!("the original folder is gone ({err}); nothing was restored"),
            ));
        }
    }

    // The first free name: the original, else `<name>.scull-restored-<n>`; never replacing.
    for n in 0..=MAX_SIBLINGS {
        let dest_name = if n == 0 {
            name.clone()
        } else {
            sibling_name(&name, n)
        };
        let dest = parent.join(&dest_name);
        if Native.metadata(&dest).is_ok() {
            continue;
        }
        let req = MoveRequest {
            anchor: &folder,
            anchor_identity: None,
            relative: Path::new(&entry_name),
            expected: e.entry.entry_identity,
            expected_kind: Some(kind),
            dest_dir: &parent,
            dest_dir_identity: Some(e.entry.parent_identity),
            dest_name: &dest_name,
        };
        match move_no_replace(&req) {
            Ok(_) => return Ok((dest, n > 0)),
            Err(MoveError::DestinationExists) => continue,
            Err(MoveError::DestinationChanged { .. }) => {
                return Err(refused(
                    "parent_changed",
                    ErrorKind::SafetyBlocked,
                    "the original folder was replaced during the restore",
                ));
            }
            Err(err) => {
                return Err(refused(
                    err.code(),
                    if err.is_impossible_here() {
                        ErrorKind::SafetyBlocked
                    } else {
                        ErrorKind::PlanInvalidated
                    },
                    format!("restore move refused: {err}"),
                ));
            }
        }
    }
    Err(refused(
        "no_free_name",
        ErrorKind::SafetyBlocked,
        format!("no free restore name next to {}", original.display()),
    ))
}

/// The entry, by identity, at its original path or a restore sibling (in the recorded parent).
fn find_restored(
    e: &QuarantineEntry,
    original: &Path,
    parent: &Path,
    name: &OsStr,
) -> Option<(PathBuf, bool)> {
    let parent_ok = Native
        .metadata(parent)
        .is_ok_and(|p| p.kind == EntryKind::Dir && p.identity == Some(e.entry.parent_identity));
    if !parent_ok {
        return None;
    }
    std::iter::once((original.to_path_buf(), false))
        .chain((1..=MAX_SIBLINGS).map(|n| (parent.join(sibling_name(name, n)), true)))
        .take_while(|(p, sibling)| !*sibling || Native.metadata(p).is_ok())
        .find(|(p, _)| {
            Native
                .metadata(p)
                .is_ok_and(|m| m.identity == Some(e.entry.entry_identity))
        })
}
