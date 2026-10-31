//! The quarantine manifest of record (SAFE-16, D-3) and per-operation targets (SAFE-09).
//!
//! The database, not the on-disk manifest copy, is authoritative: restore reads only these
//! rows. Every recorded fact (paths, identities, kind, sizes, times) is immutable; an entry is
//! marked restored or purged exactly once (enforced by triggers, schema v4).

use crate::error::{Result, StoreError};
use crate::store::{Store, int};
use rusqlite::{OptionalExtension, Row, params};
use scull_core::{ErrorKind, FileIdentity, FsPath, ItemId, OperationId};

/// `<volume hex>:<file id hex>`, the stored form of a [`FileIdentity`].
pub fn identity_key(id: FileIdentity) -> String {
    format!("{:x}:{:x}", id.volume, id.file)
}

/// Parses [`identity_key`].
pub fn parse_identity_key(s: &str) -> Option<FileIdentity> {
    let (v, f) = s.split_once(':')?;
    Some(FileIdentity::new(
        u64::from_str_radix(v, 16).ok()?,
        u128::from_str_radix(f, 16).ok()?,
    ))
}

/// What a [`QuarantineLogEntry`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum QuarantineEvent {
    /// Moved back out of quarantine (to its original path or a sibling).
    Restored,
    /// A restore of an entry that was already restored (idempotent no-op).
    RestoreNoop,
    /// A restore was refused (integrity check, recorded paths, parent directory).
    RestoreRefused,
    /// Removed by a purge; `bytes` were freed.
    Purged,
    /// A purge kept the entry (it changed, or could not be removed completely).
    PurgeKept,
}

impl QuarantineEvent {
    /// Stable string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            QuarantineEvent::Restored => "restored",
            QuarantineEvent::RestoreNoop => "restore_noop",
            QuarantineEvent::RestoreRefused => "restore_refused",
            QuarantineEvent::Purged => "purged",
            QuarantineEvent::PurgeKept => "purge_kept",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            QuarantineEvent::Restored,
            QuarantineEvent::RestoreNoop,
            QuarantineEvent::RestoreRefused,
            QuarantineEvent::Purged,
            QuarantineEvent::PurgeKept,
        ]
        .into_iter()
        .find(|e| e.as_str() == s)
    }
}

/// One row of the append-only quarantine log (schema v5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineLogEntry {
    /// The manifest entry.
    pub entry_id: i64,
    /// When (Unix seconds).
    pub recorded_at: i64,
    /// What happened.
    pub event: QuarantineEvent,
    /// The path involved (restore target), if any.
    pub path: Option<FsPath>,
    /// Stable reason code for refusals and kept entries.
    pub reason: Option<String>,
    /// Bytes involved (freed by a purge), if any.
    pub bytes: Option<u64>,
}

/// A quarantined entry to record (right after the same-volume move).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewQuarantineEntry {
    /// The operation that quarantined it.
    pub operation_id: OperationId,
    /// The plan item.
    pub item_id: ItemId,
    /// Where it was (restore writes only here, or to a sibling of it).
    pub original_path: FsPath,
    /// Where it is now.
    pub quarantine_path: FsPath,
    /// Its identity (unchanged by a same-volume rename).
    pub entry_identity: FileIdentity,
    /// Its kind (`dir`, `file`, `symlink`, ...).
    pub entry_kind: String,
    /// Identity of the directory it was moved out of.
    pub parent_identity: FileIdentity,
    /// Allocated bytes held in quarantine.
    pub bytes: u64,
    /// Logical bytes.
    pub logical_bytes: u64,
    /// When it was moved (Unix seconds).
    pub quarantined_at: i64,
    /// When it may be purged, if it expires.
    pub expires_at: Option<i64>,
}

/// A recorded quarantine entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineEntry {
    /// Row ID.
    pub entry_id: i64,
    /// The recorded facts.
    pub entry: NewQuarantineEntry,
    /// When it was restored, and to where.
    pub restored: Option<(i64, FsPath)>,
    /// When it was purged, and the bytes that freed.
    pub purged: Option<(i64, u64)>,
}

impl QuarantineEntry {
    /// Still in quarantine (neither restored nor purged).
    pub fn is_held(&self) -> bool {
        self.restored.is_none() && self.purged.is_none()
    }
}

const ENTRY_COLUMNS: &str = "entry_id, operation_id, item_id, original_path, quarantine_path, \
     entry_identity, entry_kind, parent_identity, bytes, logical_bytes, quarantined_at, \
     expires_at, restored_at, restored_to, purged_at, purged_bytes";

fn bad(what: &str) -> StoreError {
    StoreError::rejected(
        ErrorKind::Internal,
        format!("quarantine manifest row has an invalid {what}"),
    )
}

fn entry_from_row(r: &Row<'_>) -> rusqlite::Result<Result<QuarantineEntry>> {
    let get_u64 = |i: usize| -> rusqlite::Result<Option<u64>> {
        Ok(r.get::<_, Option<i64>>(i)?
            .map(|v| u64::try_from(v).unwrap_or(0)))
    };
    let op: String = r.get(1)?;
    let item: String = r.get(2)?;
    let entry_identity: Option<String> = r.get(5)?;
    let parent_identity: Option<String> = r.get(7)?;
    let restored_at: Option<i64> = r.get(12)?;
    let restored_to: Option<String> = r.get(13)?;
    let purged_at: Option<i64> = r.get(14)?;
    let purged_bytes = get_u64(15)?;
    let original_path: String = r.get(3)?;
    let quarantine_path: String = r.get(4)?;
    let entry_kind: Option<String> = r.get(6)?;
    let bytes = get_u64(8)?.unwrap_or(0);
    let logical_bytes = get_u64(9)?.unwrap_or(0);
    let quarantined_at: i64 = r.get(10)?;
    let expires_at: Option<i64> = r.get(11)?;
    let entry_id: i64 = r.get(0)?;
    Ok((|| {
        Ok(QuarantineEntry {
            entry_id,
            entry: NewQuarantineEntry {
                operation_id: OperationId::try_from(op).map_err(|_| bad("operation ID"))?,
                item_id: ItemId::try_from(item).map_err(|_| bad("item ID"))?,
                original_path: FsPath::from(original_path.as_str()),
                quarantine_path: FsPath::from(quarantine_path.as_str()),
                // Rows from before v4 carry no identity: they can never be verified, so they
                // are never restored or purged automatically.
                entry_identity: entry_identity
                    .as_deref()
                    .and_then(parse_identity_key)
                    .ok_or_else(|| bad("entry identity"))?,
                entry_kind: entry_kind.ok_or_else(|| bad("entry kind"))?,
                parent_identity: parent_identity
                    .as_deref()
                    .and_then(parse_identity_key)
                    .ok_or_else(|| bad("parent identity"))?,
                bytes,
                logical_bytes,
                quarantined_at,
                expires_at,
            },
            restored: restored_at.zip(restored_to.map(|p| FsPath::from(p.as_str()))),
            purged: purged_at.map(|at| (at, purged_bytes.unwrap_or(0))),
        })
    })())
}

impl Store {
    /// Record a quarantined entry (the manifest of record, SAFE-16). Returns its entry ID.
    pub fn insert_quarantine_entry(&mut self, e: &NewQuarantineEntry) -> Result<i64> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO quarantine_manifest (operation_id, item_id, original_path, \
                 quarantine_path, entry_identity, entry_kind, parent_identity, bytes, \
                 logical_bytes, quarantined_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    e.operation_id.as_str(),
                    e.item_id.as_str(),
                    e.original_path.as_str(),
                    e.quarantine_path.as_str(),
                    identity_key(e.entry_identity),
                    e.entry_kind,
                    identity_key(e.parent_identity),
                    int(e.bytes)?,
                    int(e.logical_bytes)?,
                    e.quarantined_at,
                    e.expires_at,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    /// One manifest entry.
    pub fn quarantine_entry(&self, entry_id: i64) -> Result<Option<QuarantineEntry>> {
        self.read(|c| {
            c.query_row(
                &format!("SELECT {ENTRY_COLUMNS} FROM quarantine_manifest WHERE entry_id = ?1"),
                [entry_id],
                entry_from_row,
            )
            .optional()?
            .transpose()
        })
    }

    /// Every manifest entry of `operation` (or of all operations), oldest first. Rows written
    /// before schema v4 (no identity) are left out: they can't be verified.
    pub fn quarantine_entries(
        &self,
        operation: Option<&OperationId>,
    ) -> Result<Vec<QuarantineEntry>> {
        self.read(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM quarantine_manifest \
                 WHERE (?1 IS NULL OR operation_id = ?1) AND entry_identity IS NOT NULL \
                 ORDER BY quarantined_at, entry_id"
            ))?;
            let rows = stmt.query_map([operation.map(OperationId::as_str)], entry_from_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r??);
            }
            Ok(out)
        })
    }

    /// Entries still in quarantine, oldest first.
    pub fn quarantine_held(&self) -> Result<Vec<QuarantineEntry>> {
        Ok(self
            .quarantine_entries(None)?
            .into_iter()
            .filter(QuarantineEntry::is_held)
            .collect())
    }

    /// Mark an entry purged with the bytes that freed. Only a held entry can be purged.
    pub fn mark_quarantine_purged(&mut self, entry_id: i64, at: i64, freed: u64) -> Result<()> {
        self.write(|tx| {
            let n = tx.execute(
                "UPDATE quarantine_manifest SET purged_at = ?2, purged_bytes = ?3 \
                 WHERE entry_id = ?1 AND purged_at IS NULL AND restored_at IS NULL",
                params![entry_id, at, int(freed)?],
            )?;
            if n == 1 {
                Ok(())
            } else {
                Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!("quarantine entry {entry_id} is not held (restored or purged)"),
                ))
            }
        })
    }

    /// Mark an entry restored to `to`. Only a held entry can be restored.
    pub fn mark_quarantine_restored(&mut self, entry_id: i64, at: i64, to: &FsPath) -> Result<()> {
        self.write(|tx| {
            let n = tx.execute(
                "UPDATE quarantine_manifest SET restored_at = ?2, restored_to = ?3 \
                 WHERE entry_id = ?1 AND purged_at IS NULL AND restored_at IS NULL",
                params![entry_id, at, to.as_str()],
            )?;
            if n == 1 {
                Ok(())
            } else {
                Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!("quarantine entry {entry_id} is not held (restored or purged)"),
                ))
            }
        })
    }

    /// Append a row to the quarantine log (restores and purges, including refusals).
    pub fn log_quarantine_event(&mut self, e: &QuarantineLogEntry) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO quarantine_log (entry_id, recorded_at, event, path, reason, bytes) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    e.entry_id,
                    e.recorded_at,
                    e.event.as_str(),
                    e.path.as_ref().map(FsPath::as_str),
                    e.reason,
                    e.bytes.map(int).transpose()?,
                ],
            )?;
            Ok(())
        })
    }

    /// The log rows of one manifest entry, oldest first.
    pub fn quarantine_log(&self, entry_id: i64) -> Result<Vec<QuarantineLogEntry>> {
        self.read(|c| {
            let mut stmt = c.prepare(
                "SELECT recorded_at, event, path, reason, bytes FROM quarantine_log \
                 WHERE entry_id = ?1 ORDER BY seq",
            )?;
            let rows = stmt.query_map([entry_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (recorded_at, event, path, reason, bytes) = row?;
                out.push(QuarantineLogEntry {
                    entry_id,
                    recorded_at,
                    event: QuarantineEvent::parse(&event).ok_or_else(|| bad("log event"))?,
                    path: path.map(|p| FsPath::from(p.as_str())),
                    reason,
                    bytes: bytes.map(|b| u64::try_from(b).unwrap_or(0)),
                });
            }
            Ok(out)
        })
    }

    /// Record the identities an operation acts on (SAFE-09). Append-only.
    pub fn record_operation_targets(
        &mut self,
        operation_id: &OperationId,
        targets: &[(ItemId, FileIdentity)],
        recorded_at: i64,
    ) -> Result<()> {
        self.write(|tx| {
            let mut stmt = tx.prepare(
                "INSERT INTO operation_targets (operation_id, item_id, identity, recorded_at) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (item, id) in targets {
                stmt.execute(params![
                    operation_id.as_str(),
                    item.as_str(),
                    identity_key(*id),
                    recorded_at
                ])?;
            }
            Ok(())
        })
    }

    /// Operations other than `excluding` that started at or after `since` and targeted any of
    /// `identities` (a plan created at `since` is invalidated by them, SAFE-09).
    pub fn operations_touching(
        &self,
        identities: &[FileIdentity],
        since: i64,
        excluding: Option<&OperationId>,
    ) -> Result<Vec<OperationId>> {
        self.read(|c| {
            let mut stmt = c.prepare(
                "SELECT DISTINCT t.operation_id FROM operation_targets t \
                 JOIN operations o ON o.operation_id = t.operation_id \
                 WHERE t.identity = ?1 AND o.started_at >= ?2 \
                   AND (?3 IS NULL OR t.operation_id <> ?3) \
                 ORDER BY t.operation_id",
            )?;
            let mut out: Vec<OperationId> = Vec::new();
            for id in identities {
                let rows = stmt.query_map(
                    params![identity_key(*id), since, excluding.map(OperationId::as_str)],
                    |r| r.get::<_, String>(0),
                )?;
                for r in rows {
                    let op = OperationId::try_from(r?).map_err(|_| bad("operation ID"))?;
                    if !out.contains(&op) {
                        out.push(op);
                    }
                }
            }
            Ok(out)
        })
    }
}
