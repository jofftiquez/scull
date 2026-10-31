//! Crash recovery (SAFE-19, T-15, P2-EXEC-08): reconcile the write-ahead journal of every
//! unfinished operation with the disk, on the next start, before anything else runs.
//!
//! [`Executor::recover`] (under the execution lock) reopens each operation that has no
//! `finished` audit row ([`Store::resume_operation`]) and gives every plan item that has no
//! audit outcome exactly one, from the journal and the disk:
//!
//! * **no intent**: the item was never started → `skipped` / `interrupted`;
//! * **closed intent** (the crash came between the close and the audit row): the close is
//!   replayed as the audit outcome;
//! * **open intent**: the step may or may not have happened, so the disk decides (paths are
//!   looked up no-follow and compared by identity, never by name alone):
//!   * removal walk: the source is gone → `removed`; still there with the fingerprint the
//!     intent recorded (entry count, logical bytes) → `skipped` / `interrupted` (nothing was
//!     removed); anything else → `partial` / `interrupted`;
//!   * quarantine move: the identity is at the destination and not at the source → the move
//!     happened: its manifest entry is recorded if the crash came before it (never twice),
//!     → `quarantined`; at the source and not at the destination → `skipped` /
//!     `interrupted`; at both → `refused` / `recovery_ambiguous` (left alone);
//!   * Trash move: the source is gone → `moved_to_trash`; still there → `skipped`.
//!
//! Recovery **never resumes** a deletion or a move and never touches an item: it only records
//! what the disk shows, closes each open journal step (`recovered`), rewrites the quarantine
//! folders' `manifest.json` copies and ends the operation as `interrupted`. The user re-plans
//! whatever is left (a new plan goes through revalidation again).

use crate::executor::Executor;
use crate::journal::fingerprint;
use crate::quarantine::{DEFAULT_RETENTION_SECS, QuarantineFolder, measure};
use scull_core::{ErrorKind, FileIdentity, FsPath, ItemId, OperationId, ScullError};
use scull_engine::PlatformOwnerOnly;
use scull_platform::{Native, Platform};
use scull_storage::{
    ItemAudit, ItemOutcome, ItemRecord, JournalAction, JournalClose, JournalCloseKind, JournalStep,
    NewQuarantineEntry, OperationEnd, OperationFinish, OperationSummary, Store,
};
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

/// Reason code for an item an interrupted operation did not get to, or whose step had not
/// touched the disk yet.
pub const INTERRUPTED: &str = "interrupted";

/// How recovery settled one plan item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recovery {
    /// No step was started for the item: nothing was touched.
    NotStarted,
    /// The step had finished and was closed in the journal; only the audit row was missing.
    Replayed,
    /// The step was in progress: its outcome was read from the disk.
    Reconciled(JournalAction),
}

/// One plan item settled by recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveredItem {
    /// The item.
    pub item_id: ItemId,
    /// How it was settled.
    pub how: Recovery,
    /// The audit outcome recorded for it.
    pub outcome: ItemOutcome,
    /// Its reason code.
    pub reason: Option<String>,
    /// The quarantine manifest entry recovery recorded for a move the crash left unrecorded.
    pub manifest_entry: Option<i64>,
}

/// One recovered operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecoveredOperation {
    /// The operation.
    pub operation_id: OperationId,
    /// Plan items that had no audit outcome, in plan order, each with the one recovery gave.
    pub items: Vec<RecoveredItem>,
    /// The operation's close (status `interrupted`).
    pub summary: OperationSummary,
}

/// The result of [`Executor::recover`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct RecoveryReport {
    /// Every operation that was unfinished, oldest first.
    pub operations: Vec<RecoveredOperation>,
}

impl RecoveryReport {
    /// Whether there was nothing to recover.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }
}

/// What a no-follow lookup of a journaled path showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    /// Nothing at the path.
    Absent,
    /// The journaled object (same identity).
    Same,
    /// Another object.
    Other,
    /// The path could not be read.
    Unknown,
}

fn look(path: &FsPath, identity: FileIdentity) -> Seen {
    if path.is_lossy() {
        return Seen::Unknown;
    }
    match Native.metadata(&path.to_path_buf()) {
        Ok(md) if md.identity == Some(identity) => Seen::Same,
        Ok(_) => Seen::Other,
        Err(e) if e.is_not_found() => Seen::Absent,
        Err(_) => Seen::Unknown,
    }
}

/// The outcome recovery records, and a manifest entry to insert first, if any.
struct Settled {
    record: ItemRecord,
    manifest: Option<NewQuarantineEntry>,
}

fn settled(record: ItemRecord) -> Settled {
    Settled {
        record,
        manifest: None,
    }
}

/// Reads the outcome of an open step from the disk. Never changes the disk.
fn reconcile(store: &Store, step: &JournalStep, now: i64) -> Result<Settled, ScullError> {
    let i = &step.intent;
    let id = i.item_id.clone();
    let rec =
        |o: ItemOutcome, reason: &str| ItemRecord::new(id.clone(), now, o).with_reason(reason);
    let src = look(&i.source_path, i.source_identity);
    Ok(match i.action {
        JournalAction::Delete => match src {
            Seen::Absent => settled(rec(ItemOutcome::Removed, "recovered")),
            Seen::Same
                if i.tree
                    .is_some_and(|t| fingerprint(&i.source_path.to_path_buf()) == t) =>
            {
                settled(rec(ItemOutcome::Skipped, INTERRUPTED))
            }
            Seen::Same | Seen::Other => settled(rec(ItemOutcome::Partial, INTERRUPTED)),
            Seen::Unknown => settled(rec(ItemOutcome::Partial, "recovery_unverifiable")),
        },
        JournalAction::Trash => match src {
            Seen::Same => settled(rec(ItemOutcome::Skipped, INTERRUPTED)),
            Seen::Absent | Seen::Other => settled(rec(ItemOutcome::MovedToTrash, "recovered")),
            Seen::Unknown => settled(rec(ItemOutcome::Failed, "recovery_unverifiable")),
        },
        JournalAction::Quarantine => {
            let Some(dest) = &i.destination else {
                return Ok(settled(rec(ItemOutcome::Failed, "recovery_unverifiable")));
            };
            let dst = look(dest, i.source_identity);
            match (src, dst) {
                (Seen::Same, Seen::Same) => {
                    settled(rec(ItemOutcome::Refused, "recovery_ambiguous"))
                }
                (Seen::Same, _) => settled(rec(ItemOutcome::Skipped, INTERRUPTED)),
                (_, Seen::Same) => {
                    // The move happened. Record its manifest entry unless the crash came after
                    // that (never twice: one entry per operation and item).
                    let existing = store
                        .quarantine_entries(Some(&i.operation_id))
                        .map_err(ScullError::from)?
                        .into_iter()
                        .find(|e| e.entry.item_id == i.item_id);
                    let (bytes, manifest) = match existing {
                        Some(e) => (e.entry.bytes, None),
                        None => {
                            let (logical_bytes, bytes) = measure(&dest.to_path_buf());
                            let Some(parent_identity) = i.source_parent else {
                                return Ok(settled(rec(
                                    ItemOutcome::Failed,
                                    "recovery_unverifiable",
                                )));
                            };
                            (
                                bytes,
                                Some(NewQuarantineEntry {
                                    operation_id: i.operation_id.clone(),
                                    item_id: i.item_id.clone(),
                                    original_path: i.source_path.clone(),
                                    quarantine_path: dest.clone(),
                                    entry_identity: i.source_identity,
                                    entry_kind: i.source_kind.clone(),
                                    parent_identity,
                                    bytes,
                                    logical_bytes,
                                    quarantined_at: i.recorded_at,
                                    expires_at: i.recorded_at.checked_add(DEFAULT_RETENTION_SECS),
                                }),
                            )
                        }
                    };
                    let mut record = ItemRecord::new(id.clone(), now, ItemOutcome::Quarantined)
                        .with_reason("recovered");
                    record.moved_bytes = Some(bytes);
                    Settled { record, manifest }
                }
                (Seen::Absent, Seen::Absent | Seen::Other) => {
                    settled(rec(ItemOutcome::Failed, "recovery_not_found"))
                }
                (Seen::Other, Seen::Absent | Seen::Other) => {
                    settled(rec(ItemOutcome::Skipped, INTERRUPTED))
                }
                _ => settled(rec(ItemOutcome::Failed, "recovery_unverifiable")),
            }
        }
        _ => settled(rec(ItemOutcome::Failed, "recovery_unverifiable")),
    })
}

/// An audit row as the record that produced it.
fn record_of(a: &ItemAudit) -> ItemRecord {
    ItemRecord {
        item_id: a.item_id.clone(),
        recorded_at: a.recorded_at,
        outcome: a.outcome,
        reason: a.reason.clone(),
        moved_bytes: a.moved_bytes,
        freed_bytes: a.freed_bytes,
        entries_removed: a.entries_removed,
        entries_kept: a.entries_kept,
    }
}

impl Executor {
    /// Crash recovery (SAFE-19): reconcile every unfinished operation in `store` with the disk
    /// and close it as interrupted. Call it right after [`Executor::acquire`], before any plan
    /// runs ([`Executor::begin_operation`] refuses while an operation is unfinished). See the
    /// [module docs](crate::recover) for how each item is settled; recovery never resumes a
    /// deletion or a move and never touches an item. `now` is the current time (Unix
    /// seconds). Needs the execution lock.
    ///
    /// Operations of this process that are still running must not exist when this is called
    /// (the lock makes that true across processes).
    pub fn recover(&self, store: &mut Store, now: i64) -> Result<RecoveryReport, ScullError> {
        if self.lock().is_none() {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                "crash recovery needs the execution lock (SAFE-09)",
            ));
        }
        let mut report = RecoveryReport::default();
        for op in store.unfinished_operations().map_err(ScullError::from)? {
            report.operations.push(recover_one(store, &op, now)?);
        }
        Ok(report)
    }
}

fn recover_one(
    store: &mut Store,
    op: &OperationId,
    now: i64,
) -> Result<RecoveredOperation, ScullError> {
    let e = ScullError::from;
    let mut audit = store.resume_operation(op, &PlatformOwnerOnly).map_err(e)?;
    let record = store
        .operation(op)
        .map_err(e)?
        .ok_or_else(|| ScullError::new(ErrorKind::Internal, format!("operation {op} vanished")))?;
    let plan = store.get_plan(&record.plan_id).map_err(e)?.ok_or_else(|| {
        ScullError::new(
            ErrorKind::Internal,
            format!("the plan of operation {op} is not recorded"),
        )
    })?;
    let audited = store.audit_items(op).map_err(e)?;
    let steps = store.journal_steps(op).map_err(e)?;
    let done: HashSet<ItemId> = audited.iter().map(|a| a.item_id.clone()).collect();

    // Open steps of items that were audited anyway (not produced by this Executor's order of
    // writes, but closed consistently if found): close them with the audited outcome.
    for step in steps.iter().filter(|s| s.close.is_none()) {
        if let Some(a) = audited.iter().find(|a| a.item_id == step.intent.item_id) {
            store
                .journal_close(
                    step.seq,
                    &JournalClose {
                        kind: JournalCloseKind::Recovered,
                        recorded_at: now,
                        outcome: Some(record_of(a)),
                    },
                )
                .map_err(e)?;
        }
    }

    let mut items = Vec::new();
    for item in plan.items() {
        if done.contains(&item.item_id) {
            continue;
        }
        let last = steps
            .iter()
            .rev()
            .find(|s| s.intent.item_id == item.item_id);
        let (how, settled) = match last {
            None => (
                Recovery::NotStarted,
                settled(
                    ItemRecord::new(item.item_id.clone(), now, ItemOutcome::Skipped)
                        .with_reason(INTERRUPTED),
                ),
            ),
            Some(step) => match &step.close {
                Some(close) => match &close.outcome {
                    Some(rec) => (Recovery::Replayed, settled(rec.clone())),
                    // The last attempt changed nothing and its retry never started.
                    None => (
                        Recovery::NotStarted,
                        settled(
                            ItemRecord::new(item.item_id.clone(), now, ItemOutcome::Skipped)
                                .with_reason(INTERRUPTED),
                        ),
                    ),
                },
                None => (
                    Recovery::Reconciled(step.intent.action),
                    reconcile(store, step, now)?,
                ),
            },
        };
        let manifest_entry = match &settled.manifest {
            Some(m) => Some(store.insert_quarantine_entry(m).map_err(e)?),
            None => None,
        };
        if let (Recovery::Reconciled(_), Some(step)) = (how, last) {
            store
                .journal_close(
                    step.seq,
                    &JournalClose {
                        kind: JournalCloseKind::Recovered,
                        recorded_at: now,
                        outcome: Some(settled.record.clone()),
                    },
                )
                .map_err(e)?;
        }
        store.record_item(&mut audit, &settled.record).map_err(e)?;
        items.push(RecoveredItem {
            item_id: item.item_id.clone(),
            how,
            outcome: settled.record.outcome,
            reason: settled.record.reason.clone(),
            manifest_entry,
        });
    }

    let summary = store
        .finish_operation(
            &mut audit,
            &OperationFinish {
                finished_at: now,
                end: OperationEnd::Interrupted,
                measured_delta_bytes: None,
            },
        )
        .map_err(e)?;

    // The quarantine folders' human-readable manifest copies, which the crash may have left
    // stale or missing.
    let dirs: BTreeSet<PathBuf> = steps
        .iter()
        .filter(|s| s.intent.action == JournalAction::Quarantine)
        .filter_map(|s| s.intent.destination.as_ref())
        .filter(|d| !d.is_lossy())
        .filter_map(|d| d.to_path_buf().parent().map(PathBuf::from))
        .collect();
    for dir in dirs {
        if let Some(identity) = Native.metadata(&dir).ok().and_then(|m| m.identity) {
            QuarantineFolder::existing(dir, identity).write_disk_manifest(store, op)?;
        }
    }
    Ok(RecoveredOperation {
        operation_id: op.clone(),
        items,
        summary,
    })
}
