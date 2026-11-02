//! The write-ahead operation journal (SAFE-19, T-15, P2-EXEC-08; schema v6).
//!
//! Before each mutating step of an operation the Executor appends an **intent**
//! ([`Store::journal_intent`]): the item, the action, the identity, kind and path of the source
//! and, for moves, the exact destination. Every commit is fsynced (`synchronous=FULL`), so the
//! intent is durable before the disk is touched. After the step the Executor **closes** the
//! intent ([`Store::journal_close`], [`JournalCloseKind::Done`]) with the outcome it is about to
//! audit, then appends the audit row.
//!
//! On the next start, crash recovery lists the operations without a `finished` audit row
//! ([`Store::unfinished_operations`]), reads their journal ([`Store::journal_steps`]),
//! reconciles each open intent with the disk and closes it with
//! [`JournalCloseKind::Recovered`]. The journal is append-only (triggers): a step is closed
//! exactly once, and a close always refers to an intent of the same operation and item.

use crate::audit::{ItemOutcome, ItemRecord, check_record, is_finished};
use crate::error::{Result, StoreError};
use crate::quarantine::{identity_key, parse_identity_key};
use crate::store::{Store, int};
use rusqlite::{OptionalExtension, Row, params};
use scull_core::{ErrorKind, FileIdentity, FsPath, ItemId, OperationId};

/// The mutating step an intent announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JournalAction {
    /// A removal walk (S0).
    Delete,
    /// A same-volume, no-replace move into the operation's quarantine folder.
    Quarantine,
    /// A move to the platform Trash.
    Trash,
}

impl JournalAction {
    /// Stable string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            JournalAction::Delete => "delete",
            JournalAction::Quarantine => "quarantine",
            JournalAction::Trash => "trash",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            JournalAction::Delete,
            JournalAction::Quarantine,
            JournalAction::Trash,
        ]
        .into_iter()
        .find(|a| a.as_str() == s)
    }
}

/// Entry count and logical bytes of a tree, taken right before a removal walk. Removal only
/// ever takes entries away, so a tree that still has this fingerprint after a crash was not
/// touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeFingerprint {
    /// Filesystem entries (the root included).
    pub entries: u64,
    /// Logical bytes.
    pub bytes: u64,
}

/// An intent: what the Executor is about to do to one plan item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalIntent {
    /// The operation.
    pub operation_id: OperationId,
    /// The plan item.
    pub item_id: ItemId,
    /// The step.
    pub action: JournalAction,
    /// Native path of the source (valid Unicode: items whose path isn't are never executed).
    pub source_path: FsPath,
    /// Identity the source has right now.
    pub source_identity: FileIdentity,
    /// Its kind (`dir`, `file`, ...).
    pub source_kind: String,
    /// Identity of the directory holding the source, when known.
    pub source_parent: Option<FileIdentity>,
    /// Where a move puts it (quarantine: the exact entry path), when known in advance.
    pub destination: Option<FsPath>,
    /// Identity of the destination directory, when known.
    pub destination_dir: Option<FileIdentity>,
    /// The tree before a removal walk.
    pub tree: Option<TreeFingerprint>,
    /// When (Unix seconds).
    pub recorded_at: i64,
}

/// How an intent was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JournalCloseKind {
    /// By the Executor, right after the step.
    Done,
    /// By crash recovery, from what the disk showed.
    Recovered,
}

impl JournalCloseKind {
    /// Stable string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            JournalCloseKind::Done => "done",
            JournalCloseKind::Recovered => "recovered",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "done" => Some(JournalCloseKind::Done),
            "recovered" => Some(JournalCloseKind::Recovered),
            _ => None,
        }
    }
}

/// The close of an intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalClose {
    /// Who closed it.
    pub kind: JournalCloseKind,
    /// When (Unix seconds).
    pub recorded_at: i64,
    /// The item outcome, exactly as it is (or was) audited; `None` when this attempt changed
    /// nothing and another attempt for the same item follows.
    pub outcome: Option<ItemRecord>,
}

/// One journaled step: its intent and, once closed, the close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalStep {
    /// Position in the journal (the intent's row).
    pub seq: i64,
    /// The intent.
    pub intent: JournalIntent,
    /// The close, if any. An open step after a crash is what recovery reconciles.
    pub close: Option<JournalClose>,
}

fn bad(what: &str) -> StoreError {
    StoreError::rejected(
        ErrorKind::Internal,
        format!("journal row has an invalid {what}"),
    )
}

const COLUMNS: &str = "seq, operation_id, item_id, kind, intent_seq, recorded_at, action, \
     source_path, source_identity, source_kind, source_parent_identity, destination, \
     destination_dir_identity, tree_entries, tree_bytes, outcome, reason, moved_bytes, \
     freed_bytes, entries_removed, entries_kept";

/// One raw journal row.
struct RawRow {
    seq: i64,
    operation_id: String,
    item_id: String,
    kind: String,
    intent_seq: Option<i64>,
    recorded_at: i64,
    action: Option<String>,
    source_path: Option<String>,
    source_identity: Option<String>,
    source_kind: Option<String>,
    source_parent: Option<String>,
    destination: Option<String>,
    destination_dir: Option<String>,
    tree_entries: Option<i64>,
    tree_bytes: Option<i64>,
    outcome: Option<String>,
    reason: Option<String>,
    moved_bytes: Option<i64>,
    freed_bytes: Option<i64>,
    entries_removed: Option<i64>,
    entries_kept: Option<i64>,
}

fn raw(r: &Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        seq: r.get(0)?,
        operation_id: r.get(1)?,
        item_id: r.get(2)?,
        kind: r.get(3)?,
        intent_seq: r.get(4)?,
        recorded_at: r.get(5)?,
        action: r.get(6)?,
        source_path: r.get(7)?,
        source_identity: r.get(8)?,
        source_kind: r.get(9)?,
        source_parent: r.get(10)?,
        destination: r.get(11)?,
        destination_dir: r.get(12)?,
        tree_entries: r.get(13)?,
        tree_bytes: r.get(14)?,
        outcome: r.get(15)?,
        reason: r.get(16)?,
        moved_bytes: r.get(17)?,
        freed_bytes: r.get(18)?,
        entries_removed: r.get(19)?,
        entries_kept: r.get(20)?,
    })
}

fn u(v: Option<i64>) -> Option<u64> {
    v.map(|n| u64::try_from(n).unwrap_or(0))
}

fn identity(v: Option<&str>, what: &str) -> Result<Option<FileIdentity>> {
    v.map(|s| parse_identity_key(s).ok_or_else(|| bad(what)))
        .transpose()
}

impl RawRow {
    fn intent(&self) -> Result<JournalIntent> {
        Ok(JournalIntent {
            operation_id: OperationId::try_from(self.operation_id.clone())
                .map_err(|_| bad("operation ID"))?,
            item_id: ItemId::try_from(self.item_id.clone()).map_err(|_| bad("item ID"))?,
            action: self
                .action
                .as_deref()
                .and_then(JournalAction::parse)
                .ok_or_else(|| bad("action"))?,
            source_path: FsPath::from(self.source_path.as_deref().ok_or_else(|| bad("path"))?),
            source_identity: identity(self.source_identity.as_deref(), "source identity")?
                .ok_or_else(|| bad("source identity"))?,
            source_kind: self.source_kind.clone().ok_or_else(|| bad("source kind"))?,
            source_parent: identity(self.source_parent.as_deref(), "parent identity")?,
            destination: self.destination.as_deref().map(FsPath::from),
            destination_dir: identity(self.destination_dir.as_deref(), "destination identity")?,
            tree: match (u(self.tree_entries), u(self.tree_bytes)) {
                (Some(entries), Some(bytes)) => Some(TreeFingerprint { entries, bytes }),
                _ => None,
            },
            recorded_at: self.recorded_at,
        })
    }

    fn close(&self) -> Result<JournalClose> {
        let item_id = ItemId::try_from(self.item_id.clone()).map_err(|_| bad("item ID"))?;
        let outcome = match &self.outcome {
            None => None,
            Some(o) => Some(ItemRecord {
                item_id,
                recorded_at: self.recorded_at,
                outcome: ItemOutcome::parse(o).ok_or_else(|| bad("outcome"))?,
                reason: self.reason.clone(),
                moved_bytes: u(self.moved_bytes),
                freed_bytes: u(self.freed_bytes),
                entries_removed: u(self.entries_removed),
                entries_kept: u(self.entries_kept),
            }),
        };
        Ok(JournalClose {
            kind: JournalCloseKind::parse(&self.kind).ok_or_else(|| bad("kind"))?,
            recorded_at: self.recorded_at,
            outcome,
        })
    }
}

impl Store {
    /// Append an intent, durably (the commit is fsynced) before the step it announces.
    /// Refused for a finished operation. Returns the intent's sequence number, which closes it.
    pub fn journal_intent(&mut self, i: &JournalIntent) -> Result<i64> {
        self.write(|tx| {
            if is_finished(tx, &i.operation_id)? {
                return Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!(
                        "operation {} is finished; nothing more can be journaled",
                        i.operation_id
                    ),
                ));
            }
            tx.execute(
                "INSERT INTO journal (operation_id, item_id, kind, recorded_at, action, \
                 source_path, source_identity, source_kind, source_parent_identity, destination, \
                 destination_dir_identity, tree_entries, tree_bytes) \
                 VALUES (?1, ?2, 'intent', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    i.operation_id.as_str(),
                    i.item_id.as_str(),
                    i.recorded_at,
                    i.action.as_str(),
                    i.source_path.as_str(),
                    identity_key(i.source_identity),
                    i.source_kind,
                    i.source_parent.map(identity_key),
                    i.destination.as_ref().map(FsPath::as_str),
                    i.destination_dir.map(identity_key),
                    i.tree.map(|t| int(t.entries)).transpose()?,
                    i.tree.map(|t| int(t.bytes)).transpose()?,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })
    }

    /// Close intent `intent_seq` (once). The outcome, if any, must be for the intent's item
    /// and is validated like an audit record (stable reason code, no byte counts on an outcome
    /// that touched nothing).
    pub fn journal_close(&mut self, intent_seq: i64, close: &JournalClose) -> Result<()> {
        if let Some(rec) = &close.outcome {
            check_record(rec)?;
        }
        self.write(|tx| {
            let found: Option<(String, String)> = tx
                .query_row(
                    "SELECT operation_id, item_id FROM journal WHERE seq = ?1 AND kind = 'intent'",
                    [intent_seq],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((op, item)) = found else {
                return Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!("no journal intent {intent_seq}"),
                ));
            };
            if let Some(rec) = &close.outcome
                && rec.item_id.as_str() != item
            {
                return Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!("journal intent {intent_seq} is for another item"),
                ));
            }
            let closed: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM journal WHERE intent_seq = ?1)",
                [intent_seq],
                |r| r.get(0),
            )?;
            if closed {
                return Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!("journal intent {intent_seq} is already closed"),
                ));
            }
            let rec = close.outcome.as_ref();
            tx.execute(
                "INSERT INTO journal (operation_id, item_id, kind, intent_seq, recorded_at, \
                 outcome, reason, moved_bytes, freed_bytes, entries_removed, entries_kept) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    op,
                    item,
                    close.kind.as_str(),
                    intent_seq,
                    close.recorded_at,
                    rec.map(|r| r.outcome.as_str()),
                    rec.and_then(|r| r.reason.clone()),
                    rec.and_then(|r| r.moved_bytes).map(int).transpose()?,
                    rec.and_then(|r| r.freed_bytes).map(int).transpose()?,
                    rec.and_then(|r| r.entries_removed).map(int).transpose()?,
                    rec.and_then(|r| r.entries_kept).map(int).transpose()?,
                ],
            )?;
            Ok(())
        })
    }

    /// Every journaled step of `operation_id`, in journal order, each with its close if any.
    pub fn journal_steps(&self, operation_id: &OperationId) -> Result<Vec<JournalStep>> {
        let rows: Vec<RawRow> = self.read(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {COLUMNS} FROM journal WHERE operation_id = ?1 ORDER BY seq"
            ))?;
            let rows = stmt
                .query_map([operation_id.as_str()], raw)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        let mut steps: Vec<JournalStep> = Vec::new();
        for r in &rows {
            match r.intent_seq {
                None => steps.push(JournalStep {
                    seq: r.seq,
                    intent: r.intent()?,
                    close: None,
                }),
                Some(of) => {
                    let step = steps
                        .iter_mut()
                        .find(|s| s.seq == of)
                        .ok_or_else(|| bad("close reference"))?;
                    step.close = Some(r.close()?);
                }
            }
        }
        Ok(steps)
    }

    /// Operations with no `finished` audit row (interrupted by a crash, or still running in
    /// this process), oldest first: what crash recovery reconciles. Operations migrated from
    /// before schema v3 (`legacy` rows) predate the journal and are not listed.
    pub fn unfinished_operations(&self) -> Result<Vec<OperationId>> {
        self.read(|c| {
            let mut stmt = c.prepare(
                "SELECT o.operation_id FROM operations o WHERE NOT EXISTS (\
                 SELECT 1 FROM audit a WHERE a.operation_id = o.operation_id \
                 AND a.kind IN ('finished', 'legacy')) ORDER BY o.started_at, o.operation_id",
            )?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids.into_iter()
                .map(|s| OperationId::try_from(s).map_err(|_| bad("operation ID")))
                .collect()
        })
    }
}
