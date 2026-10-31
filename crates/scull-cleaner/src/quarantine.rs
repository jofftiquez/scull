//! Same-volume quarantine with retention and purge (D-3, PRD §10.6, SAFE-16; P2-EXEC-05).
//!
//! # Where the quarantine lives
//!
//! Per volume and per user: `<base>/.scull-quarantine/<operation-id>/`, where `<base>` is the
//! **shallowest directory on the item's volume, among the item's ancestors, that the current
//! user owns** (`scull_platform::quarantine::quarantine_base`): typically the home directory,
//! or the user's own folder on a shared data volume. Both directories are created owner-only
//! (`0700` / protected DACL) and checked to be owner-only before anything is moved in, so a
//! quarantine is never readable by another account, even on a shared volume (FX-EXEC-16).
//! Volume roots are never used unless the user owns them, and nothing is ever moved to another
//! volume.
//!
//! # Moving in
//!
//! One atomic, same-volume, no-replace rename (`renameat2(RENAME_NOREPLACE)` /
//! `renameatx_np(RENAME_EXCL)`), from the item's verified parent directory handle, of the
//! verified item (identity, kind; never a mount point). Entries are named `0001`, `0002`, ...
//! inside the operation folder. The manifest of record is the database
//! (`quarantine_manifest`: original path, quarantine path, identity, kind, parent directory
//! identity, sizes, expiry); `manifest.json` in the operation folder is a convenience copy that
//! restore never trusts.
//!
//! # When quarantine is impossible
//!
//! No user-owned directory on the volume, a read-only or unwritable parent, another volume or
//! mount, or a filesystem without no-replace renames: the Trash fallback is next (P2-EXEC-07,
//! #73, not in this build), so the item is **blocked** and left in place. It is never deleted
//! permanently instead.
//!
//! # Retention and purge
//!
//! Entries expire after [`DEFAULT_RETENTION_SECS`] (7 days) by default. [`Executor::purge_quarantine`]
//! removes expired entries (and, with a size cap, the oldest ones over the cap) through the
//! fd-relative removal walker, each checked against its recorded identity, and records the
//! freed bytes.

use crate::executor::Executor;
use crate::operation::Operation;
use crate::revalidate::Revalidated;
use scull_core::{ErrorKind, FileIdentity, FsPath, OperationId, ScullError};
use scull_platform::quarantine::{
    MoveError, MoveRequest, QUARANTINE_DIR_NAME, move_no_replace, quarantine_base,
};
use scull_platform::remove::{self, RemoveError, RemoveOutcome, RemoveRequest};
use scull_platform::{
    EntryKind, Native, Platform, create_dir_owner_only, create_file_owner_only, ensure_owner_only,
};
use scull_storage::{
    NewQuarantineEntry, QuarantineEntry, QuarantineEvent, QuarantineLogEntry, Store,
};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Default retention: 7 days (PRD §10.6).
pub const DEFAULT_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// Name of the on-disk manifest copy inside an operation folder.
pub const DISK_MANIFEST: &str = "manifest.json";

/// Quarantine settings for an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineSettings {
    /// Use this directory as the base (it holds `.scull-quarantine`) instead of the
    /// automatic choice. It must be on the item's volume (otherwise the move is refused as
    /// cross-device). For tests and portable setups.
    pub base: Option<PathBuf>,
    /// How long entries are kept before they may be purged.
    pub retention_secs: i64,
}

impl Default for QuarantineSettings {
    fn default() -> Self {
        Self {
            base: None,
            retention_secs: DEFAULT_RETENTION_SECS,
        }
    }
}

/// One operation's quarantine folder on one volume.
#[derive(Debug)]
pub(crate) struct QuarantineFolder {
    base: PathBuf,
    dir: PathBuf,
    identity: FileIdentity,
    next: u32,
}

/// An item moved into quarantine.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Quarantined {
    /// Its manifest entry.
    pub entry_id: i64,
    /// Where it is now.
    pub quarantine_path: PathBuf,
    /// Allocated bytes held in quarantine (not freed until purged).
    pub bytes: u64,
    /// Logical bytes.
    pub logical_bytes: u64,
}

/// Why an item was not quarantined. Nothing was removed.
#[derive(Debug)]
pub(crate) enum QuarantineRefusal {
    /// Quarantine is impossible here and there is no Trash fallback yet: blocked (D-3).
    Impossible(String),
    /// The item changed or the move failed; the item stays where it was (or, if
    /// `ScullError` says so, was moved back).
    Failed(ScullError),
}

/// Stable code for an entry kind, as stored in the manifest.
pub(crate) fn kind_code(k: EntryKind) -> &'static str {
    k.as_str()
}

/// Parses [`kind_code`].
pub(crate) fn kind_from_code(s: &str) -> Option<EntryKind> {
    [
        EntryKind::File,
        EntryKind::Dir,
        EntryKind::Symlink,
        EntryKind::Junction,
        EntryKind::ReparsePoint,
        EntryKind::Fifo,
        EntryKind::Socket,
        EntryKind::BlockDevice,
        EntryKind::CharDevice,
        EntryKind::Other,
    ]
    .into_iter()
    .find(|k| k.as_str() == s)
}

/// Logical and allocated bytes of the tree at `path`, never following links.
pub(crate) fn measure(path: &Path) -> (u64, u64) {
    let (mut logical, mut allocated) = (0u64, 0u64);
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(md) = Native.metadata(&p) else {
            continue;
        };
        logical = logical.saturating_add(md.logical_size);
        allocated = allocated.saturating_add(md.allocated_size.unwrap_or(md.logical_size));
        if md.kind == EntryKind::Dir
            && let Ok(entries) = Native.read_dir(&p)
        {
            for e in entries.flatten() {
                stack.push(e.path);
            }
        }
    }
    (logical, allocated)
}

fn failed(kind: ErrorKind, msg: impl Into<String>) -> QuarantineRefusal {
    QuarantineRefusal::Failed(ScullError::new(kind, msg))
}

impl Operation<'_> {
    /// This operation's folder under `base`, created owner-only on first use.
    fn folder(&mut self, base: &Path) -> Result<&mut QuarantineFolder, QuarantineRefusal> {
        if let Some(i) = self.folders.iter().position(|f| f.base == base) {
            return Ok(&mut self.folders[i]);
        }
        let root = base.join(QUARANTINE_DIR_NAME);
        let dir = root.join(self.id().as_str());
        for d in [&root, &dir] {
            create_dir_owner_only(d).map_err(|e| {
                QuarantineRefusal::Impossible(format!(
                    "cannot create the quarantine folder {}: {e}",
                    d.display()
                ))
            })?;
            ensure_owner_only(d).map_err(|e| {
                QuarantineRefusal::Impossible(format!(
                    "the quarantine folder {} is not private to you: {e}",
                    d.display()
                ))
            })?;
        }
        let identity = Native
            .metadata(&dir)
            .ok()
            .and_then(|m| m.identity)
            .ok_or_else(|| {
                QuarantineRefusal::Impossible("cannot read the quarantine folder".into())
            })?;
        self.folders.push(QuarantineFolder {
            base: base.to_path_buf(),
            dir,
            identity,
            next: 1,
        });
        Ok(self.folders.last_mut().expect("just pushed"))
    }

    /// Moves a revalidated item into this operation's quarantine and records it.
    pub(crate) fn quarantine(
        &mut self,
        item: &Revalidated,
        now: i64,
    ) -> Result<Quarantined, QuarantineRefusal> {
        let parent = item
            .path
            .parent()
            .ok_or_else(|| failed(ErrorKind::SafetyBlocked, "the item has no parent"))?;
        let name = item
            .path
            .file_name()
            .ok_or_else(|| failed(ErrorKind::SafetyBlocked, "the item has no name"))?
            .to_owned();
        let relative = item
            .path
            .strip_prefix(&item.anchor)
            .map_err(|_| failed(ErrorKind::SafetyBlocked, "the item is not below its anchor"))?
            .to_path_buf();
        let base = match &self.quarantine.base {
            Some(b) => b.clone(),
            None => match quarantine_base(parent) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    return Err(QuarantineRefusal::Impossible(
                        "no directory you own on this volume can hold a quarantine".into(),
                    ));
                }
                Err(e) => {
                    return Err(QuarantineRefusal::Impossible(format!(
                        "cannot choose a quarantine location: {e}"
                    )));
                }
            },
        };
        if FsPath::from_path(&base).is_lossy() {
            return Err(QuarantineRefusal::Impossible(
                "the quarantine location is not valid Unicode and could not be recorded".into(),
            ));
        }
        let retention = self.quarantine.retention_secs;
        let folder = self.folder(&base)?;
        let (moved, dest) = loop {
            let entry_name = format!("{:04}", folder.next);
            folder.next += 1;
            let req = MoveRequest {
                anchor: &item.anchor,
                anchor_identity: item.anchor_identity,
                relative: &relative,
                expected: item.item.identity,
                expected_kind: Some(item.kind),
                dest_dir: &folder.dir,
                dest_dir_identity: Some(folder.identity),
                dest_name: OsStr::new(&entry_name),
            };
            match move_no_replace(&req) {
                Ok(m) => break (m, folder.dir.join(&entry_name)),
                Err(MoveError::DestinationExists) if folder.next < 10_000 => continue,
                Err(e) if e.is_impossible_here() => {
                    return Err(QuarantineRefusal::Impossible(e.to_string()));
                }
                Err(e) => {
                    let kind = match e {
                        MoveError::SourceMissing => ErrorKind::NotFound,
                        MoveError::IdentityMismatch { .. }
                        | MoveError::KindMismatch { .. }
                        | MoveError::AnchorChanged { .. }
                        | MoveError::MovedUnexpected { .. } => ErrorKind::PlanInvalidated,
                        MoveError::Io { .. } | MoveError::SourceUnavailable { .. } => ErrorKind::Io,
                        _ => ErrorKind::SafetyBlocked,
                    };
                    return Err(failed(kind, format!("quarantine move refused: {e}")));
                }
            }
        };
        let folder_dir = folder.dir.clone();
        let folder_identity = folder.identity;
        let (logical_bytes, bytes) = measure(&dest);
        let rec = NewQuarantineEntry {
            operation_id: self.id().clone(),
            item_id: item.item.item_id.clone(),
            original_path: FsPath::from_path(&item.path),
            quarantine_path: FsPath::from_path(&dest),
            entry_identity: moved.identity,
            entry_kind: kind_code(moved.kind).to_owned(),
            parent_identity: moved.source_parent,
            bytes,
            logical_bytes,
            quarantined_at: now,
            expires_at: now.checked_add(retention),
        };
        match self.store.insert_quarantine_entry(&rec) {
            Ok(entry_id) => Ok(Quarantined {
                entry_id,
                quarantine_path: dest,
                bytes,
                logical_bytes,
            }),
            Err(db) => {
                // Without a manifest entry the item could not be restored by scull: move it
                // back to where it was (no-replace, into the same directory object).
                let back = move_no_replace(&MoveRequest {
                    anchor: &folder_dir,
                    anchor_identity: Some(folder_identity),
                    relative: Path::new(dest.file_name().unwrap_or_default()),
                    expected: moved.identity,
                    expected_kind: Some(moved.kind),
                    dest_dir: parent,
                    dest_dir_identity: Some(moved.source_parent),
                    dest_name: &name,
                });
                Err(failed(
                    ErrorKind::Io,
                    format!(
                        "the quarantine manifest could not be recorded ({db}); the item {}",
                        match back {
                            Ok(_) => "was moved back".to_owned(),
                            Err(e) => format!("remains at {} ({e})", dest.display()),
                        }
                    ),
                ))
            }
        }
    }
}

impl QuarantineFolder {
    /// Writes `manifest.json`, a copy of this folder's database entries (restore never trusts
    /// it), replacing the previous copy atomically.
    pub(crate) fn write_disk_manifest(
        &self,
        store: &Store,
        operation: &OperationId,
    ) -> Result<(), ScullError> {
        let prefix = FsPath::from_path(&self.dir);
        let entries: Vec<serde_json::Value> = store
            .quarantine_entries(Some(operation))
            .map_err(ScullError::from)?
            .into_iter()
            .filter(|e| {
                e.entry
                    .quarantine_path
                    .as_str()
                    .starts_with(prefix.as_str())
            })
            .map(|e| {
                serde_json::json!({
                    "entry_id": e.entry_id,
                    "item_id": e.entry.item_id.as_str(),
                    "original_path": e.entry.original_path.as_str(),
                    "quarantine_path": e.entry.quarantine_path.as_str(),
                    "kind": e.entry.entry_kind,
                    "bytes": e.entry.bytes,
                    "quarantined_at": e.entry.quarantined_at,
                    "expires_at": e.entry.expires_at,
                })
            })
            .collect();
        let doc = serde_json::json!({
            "format": 1,
            "note": "copy for humans; scull's database is the manifest of record (SAFE-16)",
            "operation_id": operation.as_str(),
            "entries": entries,
        });
        let io = |e: std::io::Error| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot write the quarantine manifest copy: {e}"),
            )
        };
        let tmp = self.dir.join(format!("{DISK_MANIFEST}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        let mut f = create_file_owner_only(&tmp).map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot write the quarantine manifest copy: {e}"),
            )
        })?;
        f.write_all(
            serde_json::to_string_pretty(&doc)
                .unwrap_or_default()
                .as_bytes(),
        )
        .map_err(io)?;
        f.sync_all().map_err(io)?;
        std::fs::rename(&tmp, self.dir.join(DISK_MANIFEST)).map_err(io)
    }
}

/// What to purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgePolicy {
    /// Now, Unix seconds.
    pub now: i64,
    /// Purge entries whose retention has expired.
    pub expired: bool,
    /// Also purge entries quarantined before this time.
    pub older_than: Option<i64>,
    /// Then purge the oldest remaining entries until the quarantine holds at most this many
    /// bytes.
    pub size_cap: Option<u64>,
}

impl PurgePolicy {
    /// Purge what has expired at `now` (the default schedule).
    pub fn expired_at(now: i64) -> Self {
        Self {
            now,
            expired: true,
            older_than: None,
            size_cap: None,
        }
    }
}

/// What happened to one purged entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PurgeOutcome {
    /// Removed; these bytes were freed (recorded in the manifest).
    Purged {
        /// Allocated bytes freed.
        freed_bytes: u64,
    },
    /// The entry was no longer in quarantine (removed by someone else): recorded as purged
    /// with nothing freed.
    AlreadyGone,
    /// Kept: the entry changed, or could not be removed completely.
    Kept {
        /// Why.
        reason: String,
    },
}

/// The result of [`Executor::purge_quarantine`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PurgeReport {
    /// One outcome per selected entry.
    pub entries: Vec<(QuarantineEntry, PurgeOutcome)>,
    /// Total bytes freed.
    pub freed_bytes: u64,
}

/// The held entries `policy` selects, oldest first.
fn select(held: Vec<QuarantineEntry>, policy: &PurgePolicy) -> Vec<QuarantineEntry> {
    let (mut chosen, mut rest): (Vec<_>, Vec<_>) = held.into_iter().partition(|e| {
        (policy.expired && e.entry.expires_at.is_some_and(|x| x <= policy.now))
            || policy
                .older_than
                .is_some_and(|t| e.entry.quarantined_at < t)
    });
    if let Some(cap) = policy.size_cap {
        let mut total: u64 = rest.iter().map(|e| e.entry.bytes).sum();
        rest.sort_by_key(|e| (e.entry.quarantined_at, e.entry_id));
        let mut keep = Vec::new();
        for e in rest {
            if total > cap {
                total = total.saturating_sub(e.entry.bytes);
                chosen.push(e);
            } else {
                keep.push(e);
            }
        }
    }
    chosen.sort_by_key(|e| (e.entry.quarantined_at, e.entry_id));
    chosen
}

impl Executor {
    /// Purge quarantine entries selected by `policy` (expired, older than, over the size cap)
    /// under the execution lock: each is removed through the fd-relative walker starting at
    /// its operation folder and checked against its recorded identity and kind; the freed
    /// bytes are recorded in the manifest and reported (PRD §10.6).
    pub fn purge_quarantine(
        &self,
        store: &mut Store,
        policy: &PurgePolicy,
    ) -> Result<PurgeReport, ScullError> {
        if self.lock().is_none() {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "purging the quarantine needs the execution lock (SAFE-09)",
            ));
        }
        let held = store.quarantine_held().map_err(ScullError::from)?;
        let mut report = PurgeReport::default();
        for entry in select(held, policy) {
            let outcome = purge_one(&entry);
            let (event, reason, bytes) = match &outcome {
                PurgeOutcome::Purged { freed_bytes } => {
                    store
                        .mark_quarantine_purged(entry.entry_id, policy.now, *freed_bytes)
                        .map_err(ScullError::from)?;
                    report.freed_bytes = report.freed_bytes.saturating_add(*freed_bytes);
                    (QuarantineEvent::Purged, None, Some(*freed_bytes))
                }
                PurgeOutcome::AlreadyGone => {
                    store
                        .mark_quarantine_purged(entry.entry_id, policy.now, 0)
                        .map_err(ScullError::from)?;
                    (QuarantineEvent::Purged, Some("already_gone"), Some(0))
                }
                _ => (
                    QuarantineEvent::PurgeKept,
                    Some("changed_or_incomplete"),
                    None,
                ),
            };
            store
                .log_quarantine_event(&QuarantineLogEntry {
                    entry_id: entry.entry_id,
                    recorded_at: policy.now,
                    event,
                    path: None,
                    reason: reason.map(str::to_owned),
                    bytes,
                })
                .map_err(ScullError::from)?;
            report.entries.push((entry, outcome));
        }
        Ok(report)
    }
}

fn purge_one(entry: &QuarantineEntry) -> PurgeOutcome {
    let kept = |reason: String| PurgeOutcome::Kept { reason };
    let path = entry.entry.quarantine_path.to_path_buf();
    let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
        return kept("the recorded quarantine path is malformed".into());
    };
    if entry.entry.quarantine_path.is_lossy() || !path.is_absolute() {
        return kept("the recorded quarantine path can't be used".into());
    }
    let req = RemoveRequest {
        anchor: folder,
        anchor_identity: None,
        relative: Path::new(name),
        expected: entry.entry.entry_identity,
        expected_kind: kind_from_code(&entry.entry.entry_kind),
    };
    match remove::remove_tree(&req) {
        Ok(r) if r.outcome == RemoveOutcome::Complete => PurgeOutcome::Purged {
            freed_bytes: r.removed.allocated_bytes,
        },
        Ok(r) => kept(format!(
            "removal {:?}: {} entries kept",
            r.outcome, r.skipped_total
        )),
        Err(RemoveError::TargetMissing) => PurgeOutcome::AlreadyGone,
        Err(RemoveError::AnchorUnavailable { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            PurgeOutcome::AlreadyGone
        }
        Err(e) => kept(e.to_string()),
    }
}
