//! Same-volume quarantine with retention and purge (D-3, PRD §10.6, SAFE-16; P2-EXEC-05).
//!
//! # Where the quarantine lives
//!
//! Per volume and per user: `<base>/.scull-quarantine/<operation-id>/`, where `<base>` is the
//! **configured base** for the item ([`QuarantineSettings::base_for`]: `[quarantine] base` /
//! `bases` in the user config, `SCULL_QUARANTINE_BASE`; P2-EXEC-13) or, when none is
//! configured, the **shallowest directory on the item's volume, among the item's ancestors, that
//! the current user owns** (`scull_platform::quarantine::quarantine_base`): typically the home
//! directory, or the user's own folder on a shared data volume.
//!
//! A configured base is checked before anything is journaled or created: it must be absolute,
//! on the **same volume** as the item (checked on its nearest existing ancestor, so nothing is
//! created on another volume), not inside an item of the plan, and a real directory (not a
//! link); a missing base is created owner-only (`0700` / protected DACL). If any check fails the
//! item's quarantine is impossible: Trash fallback, else blocked; never moved across volumes,
//! never deleted.
//!
//! Under the test harness ([`scull_core::config::TEST_SANDBOX_ENV`], set by `scull-fixtures` on
//! every scull process a test starts) nothing outside the sandbox directory is used: an item
//! with no configured base is **blocked** (`quarantine_base_required`) instead of using the
//! automatic location, and a base outside the sandbox is blocked (`outside_test_sandbox`).
//!
//! Both directories are created owner-only
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
//! mount, or a filesystem without no-replace renames: the item moves to the Trash instead when
//! the operation enables the fallback ([`Operation::with_trash`], P2-EXEC-07); otherwise, or
//! when that is impossible too, it is **blocked** and left in place. It is never deleted
//! permanently instead.
//!
//! # Retention and purge
//!
//! Entries expire after [`DEFAULT_RETENTION_SECS`] (7 days) by default. [`Executor::purge_quarantine`]
//! removes expired entries (and, with a size cap, the oldest ones over the cap) through the
//! fd-relative removal walker, each checked against its recorded identity, and records the
//! freed bytes.

use crate::executor::Executor;
use crate::journal::{StepHook, StepPoint, at};
use crate::operation::Operation;
use crate::revalidate::Revalidated;
use scull_core::config::{QuarantineConfig, TEST_SANDBOX_ENV, quarantine_base_for};
use scull_core::{ErrorKind, FileIdentity, FsPath, OperationId, ScullError};
use scull_platform::quarantine::{
    MoveError, MoveRequest, QUARANTINE_DIR_NAME, move_no_replace, quarantine_base,
};
use scull_platform::remove::{self, RemoveError, RemoveOutcome, RemoveRequest};
use scull_platform::{
    EntryKind, Native, Platform, create_dir_owner_only, create_file_owner_only, ensure_owner_only,
};
use scull_storage::{
    JournalAction, JournalIntent, NewQuarantineEntry, QuarantineEntry, QuarantineEvent,
    QuarantineLogEntry, Store,
};
use std::collections::BTreeMap;
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
    /// automatic choice, for items on its volume. Items on another volume are not
    /// quarantined there (Trash fallback, else blocked). `[quarantine] base` in the config.
    pub base: Option<PathBuf>,
    /// Per volume (or any directory): items whose path starts with the key use the value as
    /// their base; the longest key wins, before [`QuarantineSettings::base`].
    /// `[quarantine.bases]` in the config.
    pub bases: BTreeMap<PathBuf, PathBuf>,
    /// How long entries are kept before they may be purged.
    pub retention_secs: i64,
}

impl Default for QuarantineSettings {
    fn default() -> Self {
        Self {
            base: None,
            bases: BTreeMap::new(),
            retention_secs: DEFAULT_RETENTION_SECS,
        }
    }
}

impl QuarantineSettings {
    /// The settings of a resolved `[quarantine]` config section (default retention).
    pub fn from_config(config: &QuarantineConfig) -> Self {
        Self {
            base: config.base.clone(),
            bases: config.bases.clone(),
            ..Self::default()
        }
    }

    /// The configured base for an item at `item`: the [`QuarantineSettings::bases`] entry with
    /// the longest key that is a path prefix of `item`, else [`QuarantineSettings::base`].
    /// `None`: the automatic choice.
    pub fn base_for(&self, item: &Path) -> Option<&Path> {
        quarantine_base_for(&self.bases, self.base.as_deref(), item)
    }
}

/// The test sandbox ([`TEST_SANDBOX_ENV`]): set (absolute, non-empty) only by the test
/// fixtures on the scull processes they start.
pub(crate) fn test_sandbox() -> Option<PathBuf> {
    std::env::var_os(TEST_SANDBOX_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Under the test sandbox, why `path` may not be used (it is outside the sandbox, or the
/// sandbox is not an absolute path). `None` outside the harness or when `path` is inside.
pub(crate) fn outside_test_sandbox(path: &Path) -> Option<String> {
    let sandbox = test_sandbox()?;
    if sandbox.is_absolute() && path.is_absolute() && path.starts_with(&sandbox) {
        return None;
    }
    Some(format!(
        "{} is outside the test sandbox {} ({TEST_SANDBOX_ENV}): scull never uses a location \
         outside a test's temp dir",
        path.display(),
        sandbox.display()
    ))
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
    /// Quarantine is impossible here: Trash fallback if the operation has one, else blocked
    /// (D-3).
    Impossible(String),
    /// Blocked outright, without the Trash fallback (the test-harness guard, P2-EXEC-13).
    Blocked {
        /// Stable reason code.
        code: &'static str,
        /// Why.
        message: String,
    },
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
    let (_, logical, allocated) = crate::journal::tree_stats(path);
    (logical, allocated)
}

fn failed(kind: ErrorKind, msg: impl Into<String>) -> QuarantineRefusal {
    QuarantineRefusal::Failed(ScullError::new(kind, msg))
}

/// The volume of `path` or, if it doesn't exist yet, of its nearest existing ancestor (no-follow
/// metadata): where `path` would be created.
fn existing_volume(path: &Path) -> Option<u64> {
    path.ancestors()
        .find_map(|a| Native.metadata(a).ok())
        .and_then(|m| m.identity)
        .map(|id| id.volume)
}

/// Checks a configured base for an item whose parent is `parent` and creates it owner-only
/// when missing (P2-EXEC-13). Every refusal is "impossible here" (Trash, else blocked).
fn prepare_configured_base(
    base: &Path,
    parent: &Path,
    plan_paths: &[PathBuf],
) -> Result<(), QuarantineRefusal> {
    let impossible = |why: String| {
        Err(QuarantineRefusal::Impossible(format!(
            "the configured quarantine location {} {why}",
            base.display()
        )))
    };
    if !base.is_absolute() {
        return impossible("is not an absolute path".into());
    }
    if let Some(item) = plan_paths.iter().find(|p| base.starts_with(p)) {
        return impossible(format!(
            "is inside {}, an item being cleaned",
            item.display()
        ));
    }
    let item_volume = Native
        .metadata(parent)
        .ok()
        .and_then(|m| m.identity)
        .map(|id| id.volume);
    let (Some(item_volume), Some(base_volume)) = (item_volume, existing_volume(base)) else {
        return impossible("or the item's volume can't be identified".into());
    };
    if item_volume != base_volume {
        return impossible(
            "is on another volume than the item; quarantine never moves anything across \
             volumes"
                .into(),
        );
    }
    if Native.metadata(base).is_err()
        && let Err(e) = create_dir_owner_only(base)
    {
        return impossible(format!("can't be created: {e}"));
    }
    match Native.metadata(base) {
        Ok(m) if m.kind == EntryKind::Dir => {}
        Ok(m) if m.kind.is_link() => return impossible("is a link, never followed".into()),
        Ok(m) => return impossible(format!("is a {}, not a directory", m.kind)),
        Err(e) => return impossible(format!("can't be read: {e}")),
    }
    // Created now or earlier: still the item's volume (a mount may sit at the base itself).
    match Native.metadata(base).ok().and_then(|m| m.identity) {
        Some(id) if id.volume == item_volume => Ok(()),
        _ => impossible("is on another volume than the item".into()),
    }
}

impl Operation<'_> {
    /// This operation's folder under `base` (its index), created owner-only on first use.
    fn folder(&mut self, base: &Path) -> Result<usize, QuarantineRefusal> {
        if let Some(i) = self.folders.iter().position(|f| f.base == base) {
            return Ok(i);
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
        Ok(self.folders.len() - 1)
    }

    /// Moves a revalidated item into this operation's quarantine and records it.
    ///
    /// Each move attempt is journaled first (SAFE-19): the item's path, identity and kind, its
    /// parent directory's identity and the exact destination entry, so crash recovery can tell
    /// a done move from one that never happened by looking for the identity on both sides. The
    /// open step is closed by the caller with the audited outcome; an attempt whose entry name
    /// was already taken is closed here as "changed nothing".
    pub(crate) fn quarantine(
        &mut self,
        item: &Revalidated,
        now: i64,
        hook: Option<&StepHook>,
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
        let base = match self.quarantine.base_for(&item.path) {
            Some(b) => {
                let b = b.to_path_buf();
                if let Some(why) = outside_test_sandbox(&b) {
                    return Err(QuarantineRefusal::Blocked {
                        code: "outside_test_sandbox",
                        message: why,
                    });
                }
                let plan_paths: Vec<PathBuf> = self
                    .plan_items
                    .iter()
                    .map(|i| i.path.to_path_buf())
                    .collect();
                prepare_configured_base(&b, parent, &plan_paths)?;
                b
            }
            None if test_sandbox().is_some() => {
                return Err(QuarantineRefusal::Blocked {
                    code: "quarantine_base_required",
                    message: format!(
                        "running under the test harness ({TEST_SANDBOX_ENV}) with no configured \
                         quarantine location: the automatic location is never used there, so \
                         the item was left in place"
                    ),
                });
            }
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
        let (folder_dir, folder_identity) = (
            self.folders[folder].dir.clone(),
            self.folders[folder].identity,
        );
        let source_parent = Native.metadata(parent).ok().and_then(|m| m.identity);
        let item_id = &item.item.item_id;
        let (moved, dest) = loop {
            let entry_name = format!("{:04}", self.folders[folder].next);
            self.folders[folder].next += 1;
            let dest = folder_dir.join(&entry_name);
            at(hook, StepPoint::BeforeIntent(item_id));
            self.journal_intent(&JournalIntent {
                operation_id: self.id().clone(),
                item_id: item_id.clone(),
                action: JournalAction::Quarantine,
                source_path: FsPath::from_path(&item.path),
                source_identity: item.item.identity,
                source_kind: kind_code(item.kind).to_owned(),
                source_parent,
                destination: Some(FsPath::from_path(&dest)),
                destination_dir: Some(folder_identity),
                tree: None,
                recorded_at: now,
            })
            .map_err(QuarantineRefusal::Failed)?;
            at(hook, StepPoint::AfterIntent(item_id));
            let req = MoveRequest {
                anchor: &item.anchor,
                anchor_identity: item.anchor_identity,
                relative: &relative,
                expected: item.item.identity,
                expected_kind: Some(item.kind),
                dest_dir: &folder_dir,
                dest_dir_identity: Some(folder_identity),
                dest_name: OsStr::new(&entry_name),
            };
            match move_no_replace(&req) {
                Ok(m) => break (m, dest),
                Err(MoveError::DestinationExists) if self.folders[folder].next < 10_000 => {
                    // This attempt changed nothing; the next one is journaled anew.
                    self.journal_close(None, now)
                        .map_err(QuarantineRefusal::Failed)?;
                    continue;
                }
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
        at(hook, StepPoint::AfterMove(item_id));
        let (logical_bytes, bytes) = measure(&dest);
        let rec = NewQuarantineEntry {
            operation_id: self.id().clone(),
            item_id: item_id.clone(),
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
            Ok(entry_id) => {
                at(hook, StepPoint::AfterManifest(item_id));
                Ok(Quarantined {
                    entry_id,
                    quarantine_path: dest,
                    bytes,
                    logical_bytes,
                })
            }
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
    /// An existing operation folder (crash recovery rewrites its manifest copy).
    pub(crate) fn existing(dir: PathBuf, identity: FileIdentity) -> Self {
        Self {
            base: dir
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            dir,
            identity,
            next: 1,
        }
    }

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
    if let Some(why) = outside_test_sandbox(&path) {
        return kept(why);
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
