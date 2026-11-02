//! Hash-chained, append-only audit log of executed operations (SAFE-08, PRD §13.3, DATA-02).
//!
//! # Lifecycle of one operation
//!
//! 1. [`Store::begin_operation`] records the `operations` row and the chain's first row
//!    (`started`: plan hash, approver, method, start time) in one transaction, and creates the
//!    operation's NDJSON mirror. It returns an [`OperationAudit`] handle holding the plan's items.
//! 2. [`Store::record_item`] appends exactly one outcome per plan item ([`ItemOutcome`]): removed,
//!    quarantined, moved to Trash, tool run, partial, failed, refused or skipped, each with a
//!    stable reason code where it matters. Path, action and predicted bytes come from the
//!    recorded plan, never from the caller. Partial failures are therefore recorded item by item.
//! 3. [`Store::finish_operation`] appends the `finished` row with the status and totals the
//!    store computes from its own item rows: predicted vs moved vs freed bytes, plus the measured
//!    free-space delta when the caller has one (#76). Nothing can be appended afterwards.
//!
//! [`Store::resume_operation`] reopens an unfinished operation (crash recovery, #74; the
//! write-ahead journal it reconciles is in [`crate::journal`]).
//!
//! # Tamper evidence
//!
//! Rows are append-only through SQLite triggers. Independently of the triggers, every row
//! stores `prev_hash` (the previous row's hash; the first row uses a genesis hash derived from
//! the operation ID) and `row_hash = sha256(prev_hash || canonical_json(row))`, both lowercase
//! hex; the hashed bytes are the ASCII hex of `prev_hash` followed by the canonical JSON of
//! [`AuditRowBody`] (fields in declaration order, `null` for absent values). The canonical form
//! is recomputed from the columns, so [`Store::verify_chain`] detects any edited column, any
//! deleted or reordered row, an edited `operations` row and finished totals that disagree with
//! the item rows, even when the triggers were dropped through a raw connection.
//!
//! The chain is not keyed: someone who rewrites every row after the tampered one, recomputing
//! all hashes, is not detected by the database alone, and neither is the removal of the
//! trailing rows of an unfinished operation. The NDJSON mirror is the second witness for those
//! cases ([`Store::verify_mirror`]).
//!
//! # NDJSON mirror
//!
//! Each operation also has `<data dir>/`[`AUDIT_DIR_NAME`]`/<operation-id>.ndjson`
//! ([`audit_mirror_path`]): one line per chain row ([`AuditRow`], the canonical body plus both
//! hashes), so the log can be checked with standard tools (`jq`, `sha256sum`). The directory
//! and files are created owner-only through the injected [`OwnerOnly`]; lines are only ever
//! appended (`O_APPEND`) and each write is fsynced. The database commit comes first and is the
//! record of truth; a mirror that fell behind (crash between the commit and the write) is
//! caught up on the next append or resume.
//!
//! # Invariant I-4 (fixtures lane)
//!
//! [`Store::disk_expectations`] lists, for every plan item, what its recorded outcome implies
//! for the filesystem ([`DiskExpectation`]) plus the plan items that have no outcome.
//! [`DiskExpectations::mismatches`] compares that against what a before/after snapshot saw
//! ([`ObservedChange`]); `scull-fixtures` only needs to map its snapshot diff onto it.
//!
//! # Privacy
//!
//! Reasons are stable codes (`^[a-z0-9][a-z0-9_.:-]{0,63}$`, e.g. an `ErrorKind`, a
//! `DropReason::code()` or a walker `SkipReason`), never free text, file contents or
//! environment values (INS-07, FX-DATA-07).

use crate::error::{Result, StoreError};
use crate::perms::OwnerOnly;
use crate::records::{ApprovalMethod, OperationStart};
use crate::store::{Store, int, internal};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use scull_core::{ContentHash, ErrorKind, FsPath, ItemId, OperationId, PlanId, ScullError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Name of the directory, inside scull's local data directory, holding the NDJSON audit
/// mirrors.
pub const AUDIT_DIR_NAME: &str = "audit";

/// Version of the canonical row format ([`AuditRowBody::v`]).
pub const AUDIT_FORMAT_VERSION: u32 = 1;

/// The NDJSON mirror of `operation_id`: `<data_dir>/audit/<operation-id>.ndjson`. `data_dir` is
/// the directory holding the database ([`crate::DB_FILE_NAME`]). Operation IDs are
/// `op_<alphanumerics>`, so the name is always a plain file name.
pub fn audit_mirror_path(data_dir: &Path, operation_id: &OperationId) -> PathBuf {
    data_dir
        .join(AUDIT_DIR_NAME)
        .join(format!("{operation_id}.ndjson"))
}

// ---- outcomes and statuses -------------------------------------------------------------------

/// What happened to one plan item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ItemOutcome {
    /// Deleted completely.
    Removed,
    /// Moved into same-volume quarantine (restorable).
    Quarantined,
    /// Moved to the platform Trash / Recycle Bin.
    MovedToTrash,
    /// The tool's own cleanup command ran successfully.
    ToolRun,
    /// Part of the item was removed before the action stopped (entries kept, or an anomaly
    /// aborted the walk). Needs a reason.
    Partial,
    /// The action was attempted and nothing was removed. Needs a reason.
    Failed,
    /// The Executor refused the item before touching the disk (safety check, path binding).
    /// Needs a reason.
    Refused,
    /// Dropped before acting (revalidation found it changed, SAFE-05). Needs a reason.
    Skipped,
}

impl ItemOutcome {
    const ALL: [ItemOutcome; 8] = [
        ItemOutcome::Removed,
        ItemOutcome::Quarantined,
        ItemOutcome::MovedToTrash,
        ItemOutcome::ToolRun,
        ItemOutcome::Partial,
        ItemOutcome::Failed,
        ItemOutcome::Refused,
        ItemOutcome::Skipped,
    ];

    /// Stable string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            ItemOutcome::Removed => "removed",
            ItemOutcome::Quarantined => "quarantined",
            ItemOutcome::MovedToTrash => "moved_to_trash",
            ItemOutcome::ToolRun => "tool_run",
            ItemOutcome::Partial => "partial",
            ItemOutcome::Failed => "failed",
            ItemOutcome::Refused => "refused",
            ItemOutcome::Skipped => "skipped",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == s)
    }

    /// The item's action completed.
    pub fn is_done(self) -> bool {
        matches!(
            self,
            ItemOutcome::Removed
                | ItemOutcome::Quarantined
                | ItemOutcome::MovedToTrash
                | ItemOutcome::ToolRun
        )
    }

    /// Nothing on disk was changed for this item.
    pub fn touched_nothing(self) -> bool {
        matches!(
            self,
            ItemOutcome::Failed | ItemOutcome::Refused | ItemOutcome::Skipped
        )
    }

    fn needs_reason(self) -> bool {
        matches!(self, ItemOutcome::Partial) || self.touched_nothing()
    }

    /// What this outcome implies for the item's original path (invariant I-4).
    pub fn disk_expectation(self) -> DiskExpectation {
        match self {
            ItemOutcome::Removed | ItemOutcome::Quarantined | ItemOutcome::MovedToTrash => {
                DiskExpectation::Gone
            }
            ItemOutcome::Partial => DiskExpectation::PartlyRemoved,
            ItemOutcome::Failed | ItemOutcome::Refused | ItemOutcome::Skipped => {
                DiskExpectation::Unchanged
            }
            ItemOutcome::ToolRun => DiskExpectation::ToolDefined,
        }
    }
}

/// How the caller ended an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OperationEnd {
    /// Every plan item was handled; each must have an outcome.
    Normal,
    /// The user cancelled; items without an outcome were not touched.
    Cancelled,
    /// Closed by crash recovery (#74) after an interruption.
    Interrupted,
}

/// Final status of an operation, derived by the store from the item outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OperationStatus {
    /// No item failed, was refused or was only partly processed (skips are not errors).
    Completed,
    /// Some items completed (or partly), others failed or were refused (exit code 3).
    CompletedWithErrors,
    /// Items failed or were refused and none completed.
    Failed,
    /// [`OperationEnd::Cancelled`].
    Cancelled,
    /// [`OperationEnd::Interrupted`].
    Interrupted,
}

impl OperationStatus {
    const ALL: [OperationStatus; 5] = [
        OperationStatus::Completed,
        OperationStatus::CompletedWithErrors,
        OperationStatus::Failed,
        OperationStatus::Cancelled,
        OperationStatus::Interrupted,
    ];

    /// Stable string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            OperationStatus::Completed => "completed",
            OperationStatus::CompletedWithErrors => "completed_with_errors",
            OperationStatus::Failed => "failed",
            OperationStatus::Cancelled => "cancelled",
            OperationStatus::Interrupted => "interrupted",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == s)
    }

    fn derive(end: OperationEnd, t: &OperationTotals) -> Self {
        match end {
            OperationEnd::Cancelled => OperationStatus::Cancelled,
            OperationEnd::Interrupted => OperationStatus::Interrupted,
            OperationEnd::Normal => {
                let errors = t.items_partial + t.items_failed + t.items_refused;
                if errors == 0 {
                    OperationStatus::Completed
                } else if t.items_done + t.items_partial > 0 {
                    OperationStatus::CompletedWithErrors
                } else {
                    OperationStatus::Failed
                }
            }
        }
    }
}

// ---- inputs ----------------------------------------------------------------------------------

/// One item outcome, as reported by the Executor. Path, action and predicted bytes are taken
/// from the recorded plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRecord {
    /// Plan item.
    pub item_id: ItemId,
    /// When the outcome was recorded (Unix seconds).
    pub recorded_at: i64,
    /// What happened.
    pub outcome: ItemOutcome,
    /// Stable reason code; required for partial, failed, refused and skipped.
    pub reason: Option<String>,
    /// Bytes moved aside (quarantine, Trash): not freed yet.
    pub moved_bytes: Option<u64>,
    /// Bytes freed (allocated bytes of what was removed).
    pub freed_bytes: Option<u64>,
    /// Filesystem entries removed.
    pub entries_removed: Option<u64>,
    /// Filesystem entries kept (each with a reason in the Executor's report).
    pub entries_kept: Option<u64>,
}

impl ItemRecord {
    /// An outcome with no reason and no byte counts.
    pub fn new(item_id: ItemId, recorded_at: i64, outcome: ItemOutcome) -> Self {
        Self {
            item_id,
            recorded_at,
            outcome,
            reason: None,
            moved_bytes: None,
            freed_bytes: None,
            entries_removed: None,
            entries_kept: None,
        }
    }

    /// Set the reason code.
    pub fn with_reason(mut self, code: impl Into<String>) -> Self {
        self.reason = Some(code.into());
        self
    }

    /// Dropped before acting (revalidation), with a stable code such as `DropReason::code()`.
    pub fn skipped(item_id: ItemId, recorded_at: i64, reason: impl Into<String>) -> Self {
        Self::new(item_id, recorded_at, ItemOutcome::Skipped).with_reason(reason)
    }

    /// The Executor returned an error, so nothing was removed: [`ItemOutcome::Refused`] for
    /// safety and plan-binding errors, [`ItemOutcome::Failed`] otherwise. The reason is the
    /// error's kind code (its message is free text and is not recorded).
    pub fn from_error(item_id: ItemId, recorded_at: i64, error: &ScullError) -> Self {
        let outcome = match error.kind {
            ErrorKind::SafetyBlocked
            | ErrorKind::PlanInvalidated
            | ErrorKind::PlanTampered
            | ErrorKind::PlanExpired
            | ErrorKind::PlanForeign
            | ErrorKind::UnsupportedPlatform => ItemOutcome::Refused,
            _ => ItemOutcome::Failed,
        };
        Self::new(item_id, recorded_at, outcome).with_reason(error.kind.as_str())
    }

    /// A finished removal walk ([`WalkSummary`], mapped from the Executor's `DeleteReport`):
    /// complete → [`ItemOutcome::Removed`]; otherwise [`ItemOutcome::Partial`] if anything was
    /// removed, else [`ItemOutcome::Failed`], with the walk's reason (default `incomplete` or
    /// `aborted`).
    pub fn from_walk(item_id: ItemId, recorded_at: i64, walk: &WalkSummary) -> Self {
        let (outcome, default_reason) = match walk.end {
            WalkEnd::Complete => (ItemOutcome::Removed, None),
            WalkEnd::Incomplete | WalkEnd::Aborted if walk.entries_removed > 0 => {
                (ItemOutcome::Partial, Some(walk.end.default_reason()))
            }
            WalkEnd::Incomplete | WalkEnd::Aborted => {
                (ItemOutcome::Failed, Some(walk.end.default_reason()))
            }
        };
        let reason = match outcome {
            ItemOutcome::Removed => walk.reason.clone(),
            _ => walk
                .reason
                .clone()
                .or_else(|| default_reason.map(str::to_owned)),
        };
        Self {
            item_id,
            recorded_at,
            outcome,
            reason,
            moved_bytes: None,
            freed_bytes: Some(walk.freed_bytes),
            entries_removed: Some(walk.entries_removed),
            entries_kept: Some(walk.entries_kept),
        }
    }
}

/// How a removal walk ended (mirrors `scull_platform::remove::RemoveOutcome`, which this crate
/// cannot depend on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WalkEnd {
    /// Everything removed.
    Complete,
    /// Some entries kept.
    Incomplete,
    /// Stopped at an anomaly.
    Aborted,
}

impl WalkEnd {
    fn default_reason(self) -> &'static str {
        match self {
            WalkEnd::Complete => "complete",
            WalkEnd::Incomplete => "incomplete",
            WalkEnd::Aborted => "aborted",
        }
    }
}

/// The facts of one removal walk that the audit records. The Executor fills it from its
/// `DeleteReport` (`outcome`, `removed` counts and `allocated_bytes`, `skipped_total`, and a
/// code such as the first `SkipReason::as_str()` or the abort's anomaly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkSummary {
    /// How the walk ended.
    pub end: WalkEnd,
    /// Entries removed (files, directories, links, special files).
    pub entries_removed: u64,
    /// Allocated bytes of what was removed.
    pub freed_bytes: u64,
    /// Entries kept.
    pub entries_kept: u64,
    /// Stable reason code for an incomplete or aborted walk.
    pub reason: Option<String>,
}

/// End of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationFinish {
    /// When execution finished.
    pub finished_at: i64,
    /// How it ended.
    pub end: OperationEnd,
    /// Measured free-space delta in bytes (positive = space gained), when measured (#76).
    pub measured_delta_bytes: Option<i64>,
}

// ---- outputs ---------------------------------------------------------------------------------

/// Totals of an operation, computed by the store from its item rows and the recorded plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationTotals {
    /// Items in the plan.
    pub items_planned: u64,
    /// Items removed, quarantined, moved to Trash or cleaned by their tool.
    pub items_done: u64,
    /// Items partly processed.
    pub items_partial: u64,
    /// Items that failed (nothing removed).
    pub items_failed: u64,
    /// Items refused by the Executor.
    pub items_refused: u64,
    /// Items skipped before acting.
    pub items_skipped: u64,
    /// Bytes the plan predicted for all its items.
    pub predicted_bytes: u64,
    /// Bytes moved aside (quarantine, Trash).
    pub moved_bytes: u64,
    /// Bytes freed by removal.
    pub freed_bytes: u64,
    /// Measured free-space delta, if measured.
    pub measured_delta_bytes: Option<i64>,
}

impl OperationTotals {
    /// Items with an outcome.
    pub fn items_recorded(&self) -> u64 {
        self.items_done
            + self.items_partial
            + self.items_failed
            + self.items_refused
            + self.items_skipped
    }

    /// Plan items without an outcome (only for cancelled or interrupted operations).
    pub fn items_missing(&self) -> u64 {
        self.items_planned.saturating_sub(self.items_recorded())
    }
}

/// How a finished operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSummary {
    /// When it finished.
    pub finished_at: i64,
    /// Final status.
    pub status: OperationStatus,
    /// Totals.
    pub totals: OperationTotals,
}

/// An operation as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRecord {
    /// Operation identifier.
    pub operation_id: OperationId,
    /// Executed plan.
    pub plan_id: PlanId,
    /// Hash of the executed plan.
    pub content_hash: ContentHash,
    /// Who approved it.
    pub approver: String,
    /// How it was approved.
    pub method: ApprovalMethod,
    /// When execution started.
    pub started_at: i64,
    /// Item outcomes recorded so far.
    pub items_recorded: u64,
    /// Set once the operation finished.
    pub finished: Option<OperationSummary>,
}

/// One recorded item outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemAudit {
    /// Position in the operation's chain.
    pub op_seq: u64,
    /// When it was recorded.
    pub recorded_at: i64,
    /// Plan item.
    pub item_id: ItemId,
    /// The item's path in the plan.
    pub path: FsPath,
    /// The plan's action (`delete`, `quarantine`, `trash`, `tool_command`, ...).
    pub action: String,
    /// What happened.
    pub outcome: ItemOutcome,
    /// Stable reason code.
    pub reason: Option<String>,
    /// Bytes the plan predicted for this item.
    pub predicted_bytes: u64,
    /// Bytes moved aside.
    pub moved_bytes: Option<u64>,
    /// Bytes freed.
    pub freed_bytes: Option<u64>,
    /// Entries removed.
    pub entries_removed: Option<u64>,
    /// Entries kept.
    pub entries_kept: Option<u64>,
}

/// The hashed content of one audit row, exactly as canonicalized: serialized with
/// `serde_json` in field order, absent values as `null`. Strings are the stored values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRowBody {
    /// Format version ([`AUDIT_FORMAT_VERSION`]).
    pub v: u32,
    /// Operation.
    pub operation_id: String,
    /// Position in the operation's chain (0 = `started`).
    pub op_seq: u64,
    /// `started`, `item`, `finished` or `legacy` (a pre-v3 row).
    pub kind: String,
    /// When it was recorded.
    pub recorded_at: i64,
    /// `started`: plan.
    pub plan_id: Option<String>,
    /// `started`: plan hash.
    pub content_hash: Option<String>,
    /// `started`: approver.
    pub approver: Option<String>,
    /// `started`: approval method.
    pub method: Option<String>,
    /// `item`: plan item.
    pub item_id: Option<String>,
    /// `item`: plan path.
    pub path: Option<String>,
    /// `item`: plan action.
    pub action: Option<String>,
    /// `item`: [`ItemOutcome`] (`legacy`: the v1 outcome string).
    pub outcome: Option<String>,
    /// Reason code.
    pub reason: Option<String>,
    /// `item`: predicted bytes; `finished`: plan total.
    pub predicted_bytes: Option<u64>,
    /// `item`/`finished`: bytes moved aside.
    pub moved_bytes: Option<u64>,
    /// `item`/`finished`: bytes freed (`legacy`: v1 `measured_bytes`).
    pub freed_bytes: Option<u64>,
    /// `item`: entries removed.
    pub entries_removed: Option<u64>,
    /// `item`: entries kept.
    pub entries_kept: Option<u64>,
    /// `finished`: [`OperationStatus`].
    pub status: Option<String>,
    /// `finished`: totals.
    pub items_planned: Option<u64>,
    /// `finished`: totals.
    pub items_done: Option<u64>,
    /// `finished`: totals.
    pub items_partial: Option<u64>,
    /// `finished`: totals.
    pub items_failed: Option<u64>,
    /// `finished`: totals.
    pub items_refused: Option<u64>,
    /// `finished`: totals.
    pub items_skipped: Option<u64>,
    /// `finished`: measured free-space delta.
    pub measured_delta_bytes: Option<i64>,
}

impl AuditRowBody {
    fn new(operation_id: &OperationId, kind: &str, recorded_at: i64) -> Self {
        Self {
            v: AUDIT_FORMAT_VERSION,
            operation_id: operation_id.as_str().to_owned(),
            op_seq: 0,
            kind: kind.to_owned(),
            recorded_at,
            plan_id: None,
            content_hash: None,
            approver: None,
            method: None,
            item_id: None,
            path: None,
            action: None,
            outcome: None,
            reason: None,
            predicted_bytes: None,
            moved_bytes: None,
            freed_bytes: None,
            entries_removed: None,
            entries_kept: None,
            status: None,
            items_planned: None,
            items_done: None,
            items_partial: None,
            items_failed: None,
            items_refused: None,
            items_skipped: None,
            measured_delta_bytes: None,
        }
    }

    /// The exact bytes covered by `row_hash` (after the previous hash).
    pub fn canonical_json(&self) -> Vec<u8> {
        // Serializing a struct of strings and integers cannot fail.
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// One chain row with its hashes: a line of the NDJSON mirror.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRow {
    /// Hashed content.
    #[serde(flatten)]
    pub body: AuditRowBody,
    /// Previous row's `row_hash`, or the genesis hash for `op_seq` 0.
    pub prev_hash: String,
    /// `sha256(prev_hash || canonical_json(body))`, lowercase hex.
    pub row_hash: String,
}

/// The `prev_hash` of an operation's first row: `sha256("scull-audit-v1\0" || operation_id)`.
pub fn genesis_hash(operation_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"scull-audit-v1\0");
    h.update(operation_id.as_bytes());
    hex(&h.finalize())
}

/// `sha256(prev_hash || canonical_json(body))`, lowercase hex.
pub fn row_hash(prev_hash: &str, body: &AuditRowBody) -> String {
    let mut h = Sha256::new();
    h.update(prev_hash.as_bytes());
    h.update(body.canonical_json());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(D[usize::from(b >> 4)] as char);
        s.push(D[usize::from(b & 0xf)] as char);
    }
    s
}

/// Result of [`Store::verify_chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainReport {
    /// Operation checked.
    pub operation_id: OperationId,
    /// Rows found.
    pub rows: u64,
    /// Whether a `finished` row exists.
    pub finished: bool,
    /// Everything wrong, in chain order. Empty means intact.
    pub problems: Vec<ChainProblem>,
}

impl ChainReport {
    /// No problem found.
    pub fn is_intact(&self) -> bool {
        self.problems.is_empty()
    }
}

/// One integrity problem found by [`Store::verify_chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChainProblem {
    /// The `operations` row is missing but audit rows exist.
    UnknownOperation,
    /// No `started` row at position 0 (rows deleted).
    MissingStart,
    /// The `started` row disagrees with the `operations` row (one of them was edited).
    StartMismatch,
    /// A position is missing (rows deleted) or out of order.
    SeqGap {
        /// Position expected.
        expected: u64,
        /// Position found.
        found: u64,
    },
    /// `prev_hash` is not the previous row's hash (a row before it was deleted, replaced or
    /// reordered).
    BrokenLink {
        /// Row whose link is broken.
        op_seq: u64,
    },
    /// The row's content does not match its hash (a column was edited).
    HashMismatch {
        /// Edited row.
        op_seq: u64,
    },
    /// A row of the wrong kind where it does not belong (a second `started`, rows after
    /// `finished`).
    Misplaced {
        /// The row.
        op_seq: u64,
    },
    /// More than one outcome for one item.
    DuplicateItem {
        /// The item.
        item_id: String,
    },
    /// The `finished` totals disagree with the item rows or the recorded plan.
    TotalsMismatch,
}

/// Result of [`Store::verify_mirror`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorReport {
    /// The mirror file.
    pub path: PathBuf,
    /// Rows in the database.
    pub db_rows: u64,
    /// Lines in the mirror that parse as rows.
    pub mirror_rows: u64,
    /// 1-based numbers of lines that do not parse (a write torn by a crash, or edits).
    pub bad_lines: Vec<u64>,
    /// Verdict.
    pub status: MirrorStatus,
}

/// How the NDJSON mirror compares with the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MirrorStatus {
    /// Same rows, same order, same hashes, every line hash-consistent.
    Matches,
    /// No mirror file.
    Missing,
    /// The mirror is a strict prefix of the database (writes pending).
    Behind,
    /// The mirror has rows the database lacks (database rows were deleted).
    Ahead,
    /// The row at this position differs (or its line is not hash-consistent).
    Diverged {
        /// Position of the first difference.
        op_seq: u64,
    },
}

/// What a recorded outcome implies for the item's original path (invariant I-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DiskExpectation {
    /// The path no longer exists (removed, quarantined, moved to Trash).
    Gone,
    /// The path and everything below it are unchanged (failed, refused, skipped).
    Unchanged,
    /// Some entries below the path are gone, the path itself remains.
    PartlyRemoved,
    /// Whatever the tool did (tool-native commands); not checked.
    ToolDefined,
}

/// What a before/after snapshot saw at an item's path, for [`DiskExpectations::mismatches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ObservedChange {
    /// Existed before, gone after.
    Gone,
    /// Identical before and after (paths, identities, contents).
    Unchanged,
    /// Exists after, but something below it changed or disappeared.
    Changed,
    /// Did not exist before the operation.
    Absent,
}

/// One plan item with its recorded outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedItem {
    /// Plan item.
    pub item_id: ItemId,
    /// Its path in the plan.
    pub path: FsPath,
    /// Recorded outcome.
    pub outcome: ItemOutcome,
    /// What it implies on disk.
    pub expect: DiskExpectation,
}

/// Data for invariant I-4 ("the audit log matches what happened on disk").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskExpectations {
    /// Operation.
    pub operation_id: OperationId,
    /// Whether the operation finished.
    pub finished: bool,
    /// Plan items with an outcome, in plan order.
    pub items: Vec<ExpectedItem>,
    /// Plan items with no outcome (path in the plan).
    pub without_outcome: Vec<(ItemId, FsPath)>,
}

impl DiskExpectations {
    /// Compare with what was observed at each item path; returns one message per mismatch,
    /// including every plan item without an outcome. Empty means the audit matches the disk.
    pub fn mismatches(&self, mut observe: impl FnMut(&FsPath) -> ObservedChange) -> Vec<String> {
        let mut out: Vec<String> = self
            .without_outcome
            .iter()
            .map(|(id, path)| format!("{id} ({path}): no audit outcome"))
            .collect();
        for item in &self.items {
            let seen = observe(&item.path);
            let ok = match item.expect {
                DiskExpectation::Gone => seen == ObservedChange::Gone,
                DiskExpectation::Unchanged => seen == ObservedChange::Unchanged,
                DiskExpectation::PartlyRemoved => seen == ObservedChange::Changed,
                DiskExpectation::ToolDefined => true,
            };
            if !ok {
                out.push(format!(
                    "{} ({}): audit says {} (expect {:?}) but disk shows {:?}",
                    item.item_id,
                    item.path,
                    item.outcome.as_str(),
                    item.expect,
                    seen
                ));
            }
        }
        out
    }
}

// ---- the handle ------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PlannedItem {
    path: FsPath,
    action: String,
    predicted: u64,
}

/// An operation being executed: returned by [`Store::begin_operation`] and
/// [`Store::resume_operation`], passed to [`Store::record_item`] and
/// [`Store::finish_operation`]. Holds the plan's items (outcomes are checked against them) and
/// the mirror's progress.
#[derive(Debug)]
pub struct OperationAudit {
    operation_id: OperationId,
    items: HashMap<ItemId, PlannedItem>,
    predicted_total: u64,
    mirror: PathBuf,
    mirror_rows: u64,
    finished: bool,
}

impl OperationAudit {
    /// The operation.
    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    /// The NDJSON mirror file.
    pub fn mirror_path(&self) -> &Path {
        &self.mirror
    }
}

// ---- store API -------------------------------------------------------------------------------

impl Store {
    /// Start an operation (SAFE-08): records the `operations` row and the `started` audit row
    /// atomically, after checking that the plan is recorded, intact, and has this hash. Creates
    /// the owner-only NDJSON mirror (and its directory) through `perms` first.
    pub fn begin_operation(
        &mut self,
        start: &OperationStart,
        perms: &dyn OwnerOnly,
    ) -> Result<OperationAudit> {
        let (items, _, predicted_total) = self.planned_items(&start.plan_id)?;
        let mirror = self.mirror_path(&start.operation_id)?;
        ensure_audit_dir(&mirror, perms)?;
        let exists: bool = self.read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM operations WHERE operation_id = ?1)",
                [start.operation_id.as_str()],
                |r| r.get(0),
            )?)
        })?;
        if exists {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                format!("operation {} is already recorded", start.operation_id),
            ));
        }
        create_mirror(&mirror, perms)?;
        let plan_hash = self.read(|c| {
            Ok(c.query_row(
                "SELECT content_hash FROM plans WHERE plan_id = ?1",
                [start.plan_id.as_str()],
                |r| r.get::<_, String>(0),
            )?)
        })?;
        if plan_hash != start.content_hash.as_str() {
            return Err(StoreError::rejected(
                ErrorKind::PlanTampered,
                "operation hash does not match the recorded plan",
            ));
        }
        self.write(|tx| {
            tx.execute(
                "INSERT INTO operations (operation_id, plan_id, content_hash, approver, method, \
                 started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    start.operation_id.as_str(),
                    start.plan_id.as_str(),
                    start.content_hash.as_str(),
                    start.approver,
                    start.method.as_str(),
                    start.started_at,
                ],
            )?;
            append_row(tx, started_body(start))?;
            Ok(())
        })?;
        let mut audit = OperationAudit {
            operation_id: start.operation_id.clone(),
            items,
            predicted_total,
            mirror,
            mirror_rows: 0,
            finished: false,
        };
        self.sync_mirror(&mut audit)?;
        Ok(audit)
    }

    /// Reopen an unfinished operation (crash recovery, #74), catching its mirror up with the
    /// database (a missing mirror is recreated from the database rows; a write torn by a crash
    /// is terminated and left in place, reported by [`Store::verify_mirror`]).
    pub fn resume_operation(
        &mut self,
        operation_id: &OperationId,
        perms: &dyn OwnerOnly,
    ) -> Result<OperationAudit> {
        let Some(op) = self.operation(operation_id)? else {
            return Err(StoreError::rejected(
                ErrorKind::NotFound,
                format!("no recorded operation {operation_id}"),
            ));
        };
        if op.finished.is_some() {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                format!("operation {operation_id} is finished; its audit is closed"),
            ));
        }
        let (items, _, predicted_total) = self.planned_items(&op.plan_id)?;
        let mirror = self.mirror_path(operation_id)?;
        ensure_audit_dir(&mirror, perms)?;
        let mirror_rows = match std::fs::symlink_metadata(&mirror) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                create_mirror(&mirror, perms)?;
                0
            }
            Err(e) => return Err(StoreError::io(&mirror, e)),
            Ok(_) => {
                let bytes = read_mirror(&mirror)?;
                if !bytes.is_empty() && !bytes.ends_with(b"\n") {
                    append_bytes(&mirror, b"\n")?;
                }
                parse_mirror(&bytes).0.len() as u64
            }
        };
        let mut audit = OperationAudit {
            operation_id: operation_id.clone(),
            items,
            predicted_total,
            mirror,
            mirror_rows,
            finished: false,
        };
        self.sync_mirror(&mut audit)?;
        Ok(audit)
    }

    /// Record one item outcome (call once per plan item, right after acting on it, also for
    /// refusals and skips). Rejected: an item not in the plan ([`ErrorKind::PlanInvalidated`]),
    /// a second outcome for the same item, a missing or malformed reason code, byte counts on
    /// an outcome that touched nothing, or a finished operation ([`ErrorKind::Usage`]).
    ///
    /// The database row is committed before the mirror line is written. If the mirror write
    /// fails the error is returned, the row stays recorded, and the next call catches the
    /// mirror up.
    pub fn record_item(
        &mut self,
        audit: &mut OperationAudit,
        rec: &ItemRecord,
    ) -> Result<ItemAudit> {
        if audit.finished {
            return Err(closed(&audit.operation_id));
        }
        let Some(planned) = audit.items.get(&rec.item_id) else {
            return Err(StoreError::rejected(
                ErrorKind::PlanInvalidated,
                format!(
                    "item {} is not part of operation {}'s plan",
                    rec.item_id, audit.operation_id
                ),
            ));
        };
        check_record(rec)?;
        let mut body = AuditRowBody::new(&audit.operation_id, "item", rec.recorded_at);
        body.item_id = Some(rec.item_id.as_str().to_owned());
        body.path = Some(planned.path.as_str().to_owned());
        body.action = Some(planned.action.clone());
        body.outcome = Some(rec.outcome.as_str().to_owned());
        body.reason = rec.reason.clone();
        body.predicted_bytes = Some(planned.predicted);
        body.moved_bytes = rec.moved_bytes;
        body.freed_bytes = rec.freed_bytes;
        body.entries_removed = rec.entries_removed;
        body.entries_kept = rec.entries_kept;
        let op = audit.operation_id.clone();
        let row = self.write(|tx| {
            let dup: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM audit WHERE operation_id = ?1 AND kind = 'item' \
                 AND item_id = ?2)",
                params![op.as_str(), rec.item_id.as_str()],
                |r| r.get(0),
            )?;
            if dup {
                return Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!("item {} already has an outcome in {op}", rec.item_id),
                ));
            }
            if is_finished(tx, &op)? {
                return Err(closed(&op));
            }
            append_row(tx, body)
        })?;
        self.sync_mirror(audit)?;
        item_from_row(&row.body)
    }

    /// Finish an operation: appends the `finished` row with the status and totals computed from
    /// the recorded item outcomes and the plan. With [`OperationEnd::Normal`] every plan item
    /// must have an outcome (FX-EXEC-13); cancelled and interrupted operations may leave items
    /// without one (they were not touched). Nothing can be appended afterwards.
    pub fn finish_operation(
        &mut self,
        audit: &mut OperationAudit,
        fin: &OperationFinish,
    ) -> Result<OperationSummary> {
        if audit.finished {
            return Err(closed(&audit.operation_id));
        }
        let op = audit.operation_id.clone();
        let planned = audit.items.len() as u64;
        let predicted = audit.predicted_total;
        let summary = self.write(|tx| {
            if is_finished(tx, &op)? {
                return Err(closed(&op));
            }
            let mut totals = item_totals(tx, &op)?;
            totals.items_planned = planned;
            totals.predicted_bytes = predicted;
            totals.measured_delta_bytes = fin.measured_delta_bytes;
            if fin.end == OperationEnd::Normal && totals.items_missing() > 0 {
                return Err(StoreError::rejected(
                    ErrorKind::Usage,
                    format!(
                        "{} plan item(s) of {op} have no outcome; record each one (skipped \
                         with a reason if not acted on) or end the operation as cancelled",
                        totals.items_missing()
                    ),
                ));
            }
            let status = OperationStatus::derive(fin.end, &totals);
            let mut body = AuditRowBody::new(&op, "finished", fin.finished_at);
            body.status = Some(status.as_str().to_owned());
            set_totals(&mut body, &totals);
            append_row(tx, body)?;
            Ok(OperationSummary {
                finished_at: fin.finished_at,
                status,
                totals,
            })
        })?;
        audit.finished = true;
        self.sync_mirror(audit)?;
        Ok(summary)
    }

    /// One operation with its summary, if recorded.
    pub fn operation(&self, operation_id: &OperationId) -> Result<Option<OperationRecord>> {
        self.read(|c| operation_record(c, operation_id))
    }

    /// Operations started at or after `since` (Unix seconds), oldest first.
    pub fn operations(&self, since: i64) -> Result<Vec<OperationRecord>> {
        self.read(|c| {
            let mut stmt = c.prepare(
                "SELECT operation_id FROM operations WHERE started_at >= ?1 \
                 ORDER BY started_at, operation_id",
            )?;
            let ids: Vec<String> = stmt
                .query_map([since], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let id = OperationId::try_from(id).map_err(bad)?;
                if let Some(rec) = operation_record(c, &id)? {
                    out.push(rec);
                }
            }
            Ok(out)
        })
    }

    /// The item outcomes of an operation, in the order they were recorded.
    pub fn audit_items(&self, operation_id: &OperationId) -> Result<Vec<ItemAudit>> {
        self.audit_rows(operation_id)?
            .iter()
            .filter(|r| r.body.kind == "item")
            .map(|r| item_from_row(&r.body))
            .collect()
    }

    /// Every chain row of an operation (with hashes), in chain order: the database side of
    /// the NDJSON mirror.
    pub fn audit_rows(&self, operation_id: &OperationId) -> Result<Vec<AuditRow>> {
        self.read(|c| rows_from(c, operation_id, 0))
    }

    /// Verify an operation's chain (see the module docs). `Ok` with problems listed means
    /// tampering or damage was detected; an unknown operation with no rows is
    /// [`ErrorKind::NotFound`].
    pub fn verify_chain(&self, operation_id: &OperationId) -> Result<ChainReport> {
        self.read(|c| verify_chain_conn(c, operation_id))
    }

    /// Verify the chain of every recorded operation; returns the reports that found problems.
    pub fn verify_all_chains(&self) -> Result<Vec<ChainReport>> {
        self.read(|c| {
            let mut stmt = c.prepare(
                "SELECT operation_id FROM operations UNION SELECT operation_id FROM audit \
                 ORDER BY 1",
            )?;
            let ids: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut out = Vec::new();
            for id in ids {
                let id = OperationId::try_from(id).map_err(bad)?;
                let report = verify_chain_conn(c, &id)?;
                if !report.is_intact() {
                    out.push(report);
                }
            }
            Ok(out)
        })
    }

    /// Compare an operation's NDJSON mirror with its database rows; every mirror line is also
    /// re-hashed.
    pub fn verify_mirror(&self, operation_id: &OperationId) -> Result<MirrorReport> {
        let path = self.mirror_path(operation_id)?;
        let db = self.audit_rows(operation_id)?;
        let mut report = MirrorReport {
            path: path.clone(),
            db_rows: db.len() as u64,
            mirror_rows: 0,
            bad_lines: Vec::new(),
            status: MirrorStatus::Missing,
        };
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(StoreError::io(&path, e)),
            Ok(_) => {}
        }
        let (lines, bad_lines) = parse_mirror(&read_mirror(&path)?);
        report.mirror_rows = lines.len() as u64;
        report.bad_lines = bad_lines;
        let first_diff = lines
            .iter()
            .zip(&db)
            .position(|(m, d)| m != d || row_hash(&m.prev_hash, &m.body) != m.row_hash);
        report.status = match first_diff {
            Some(i) => MirrorStatus::Diverged { op_seq: i as u64 },
            None if lines.len() < db.len() => MirrorStatus::Behind,
            None if lines.len() > db.len() => MirrorStatus::Ahead,
            None => MirrorStatus::Matches,
        };
        Ok(report)
    }

    /// What the recorded outcomes imply on disk, for invariant I-4 (see
    /// [`DiskExpectations::mismatches`]).
    pub fn disk_expectations(&self, operation_id: &OperationId) -> Result<DiskExpectations> {
        let Some(op) = self.operation(operation_id)? else {
            return Err(StoreError::rejected(
                ErrorKind::NotFound,
                format!("no recorded operation {operation_id}"),
            ));
        };
        let (planned, order, _) = self.planned_items(&op.plan_id)?;
        let recorded: HashMap<ItemId, ItemAudit> = self
            .audit_items(operation_id)?
            .into_iter()
            .map(|i| (i.item_id.clone(), i))
            .collect();
        let mut out = DiskExpectations {
            operation_id: operation_id.clone(),
            finished: op.finished.is_some(),
            items: Vec::new(),
            without_outcome: Vec::new(),
        };
        for id in order {
            let path = planned[&id].path.clone();
            match recorded.get(&id) {
                Some(r) => out.items.push(ExpectedItem {
                    item_id: id,
                    path,
                    outcome: r.outcome,
                    expect: r.outcome.disk_expectation(),
                }),
                None => out.without_outcome.push((id, path)),
            }
        }
        Ok(out)
    }

    /// The plan's items keyed by ID, in plan order, and the predicted total.
    #[allow(clippy::type_complexity)]
    fn planned_items(
        &self,
        plan_id: &PlanId,
    ) -> Result<(HashMap<ItemId, PlannedItem>, Vec<ItemId>, u64)> {
        let Some(plan) = self.get_plan(plan_id)? else {
            return Err(StoreError::rejected(
                ErrorKind::NotFound,
                format!("no recorded plan {plan_id}"),
            ));
        };
        let mut items = HashMap::new();
        let mut order = Vec::new();
        let mut total = 0u64;
        for i in plan.items() {
            let action = serde_json::to_value(&i.action).map_err(internal)?;
            let action = action
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown")
                .to_owned();
            total = total.saturating_add(i.reclaimable_bytes);
            order.push(i.item_id.clone());
            items.insert(
                i.item_id.clone(),
                PlannedItem {
                    path: i.path.clone(),
                    action,
                    predicted: i.reclaimable_bytes,
                },
            );
        }
        Ok((items, order, total))
    }

    fn mirror_path(&self, operation_id: &OperationId) -> Result<PathBuf> {
        let dir = self
            .path()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(audit_mirror_path(dir, operation_id))
    }

    /// Append the database rows the mirror does not have yet, then fsync.
    fn sync_mirror(&self, audit: &mut OperationAudit) -> Result<()> {
        let rows = self.read(|c| rows_from(c, &audit.operation_id, audit.mirror_rows))?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut buf = Vec::new();
        for r in &rows {
            serde_json::to_writer(&mut buf, r).map_err(internal)?;
            buf.push(b'\n');
        }
        append_bytes(&audit.mirror, &buf)?;
        audit.mirror_rows += rows.len() as u64;
        Ok(())
    }
}

// ---- helpers ---------------------------------------------------------------------------------

fn bad(m: String) -> StoreError {
    StoreError::rejected(ErrorKind::Internal, m)
}

fn closed(op: &OperationId) -> StoreError {
    StoreError::rejected(
        ErrorKind::Usage,
        format!("operation {op} is finished; its audit is closed"),
    )
}

/// Stable reason codes only (privacy: no free text, contents or environment values).
fn check_code(code: &str) -> Result<()> {
    let b = code.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_.:-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(StoreError::rejected(
            ErrorKind::Usage,
            "audit reasons must be stable codes matching ^[a-z0-9][a-z0-9_.:-]{0,63}$",
        ))
    }
}

pub(crate) fn check_record(rec: &ItemRecord) -> Result<()> {
    match &rec.reason {
        Some(code) => check_code(code)?,
        None if rec.outcome.needs_reason() => {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                format!("a {} outcome needs a reason code", rec.outcome.as_str()),
            ));
        }
        None => {}
    }
    let touched = [rec.moved_bytes, rec.freed_bytes, rec.entries_removed]
        .iter()
        .any(|v| v.is_some_and(|n| n > 0));
    if rec.outcome.touched_nothing() && touched {
        return Err(StoreError::rejected(
            ErrorKind::Usage,
            format!(
                "a {} outcome means nothing was changed; record partial instead",
                rec.outcome.as_str()
            ),
        ));
    }
    Ok(())
}

fn started_body(start: &OperationStart) -> AuditRowBody {
    let mut body = AuditRowBody::new(&start.operation_id, "started", start.started_at);
    body.plan_id = Some(start.plan_id.as_str().to_owned());
    body.content_hash = Some(start.content_hash.as_str().to_owned());
    body.approver = Some(start.approver.clone());
    body.method = Some(start.method.as_str().to_owned());
    body
}

fn set_totals(body: &mut AuditRowBody, t: &OperationTotals) {
    body.items_planned = Some(t.items_planned);
    body.items_done = Some(t.items_done);
    body.items_partial = Some(t.items_partial);
    body.items_failed = Some(t.items_failed);
    body.items_refused = Some(t.items_refused);
    body.items_skipped = Some(t.items_skipped);
    body.predicted_bytes = Some(t.predicted_bytes);
    body.moved_bytes = Some(t.moved_bytes);
    body.freed_bytes = Some(t.freed_bytes);
    body.measured_delta_bytes = t.measured_delta_bytes;
}

fn totals_of(body: &AuditRowBody) -> OperationTotals {
    OperationTotals {
        items_planned: body.items_planned.unwrap_or(0),
        items_done: body.items_done.unwrap_or(0),
        items_partial: body.items_partial.unwrap_or(0),
        items_failed: body.items_failed.unwrap_or(0),
        items_refused: body.items_refused.unwrap_or(0),
        items_skipped: body.items_skipped.unwrap_or(0),
        predicted_bytes: body.predicted_bytes.unwrap_or(0),
        moved_bytes: body.moved_bytes.unwrap_or(0),
        freed_bytes: body.freed_bytes.unwrap_or(0),
        measured_delta_bytes: body.measured_delta_bytes,
    }
}

/// Totals over the `item` rows (counts per outcome class, moved and freed sums).
fn totals_from_items<'a>(items: impl Iterator<Item = &'a AuditRowBody>) -> OperationTotals {
    let mut t = OperationTotals::default();
    for b in items {
        match b.outcome.as_deref().and_then(ItemOutcome::parse) {
            Some(o) if o.is_done() => t.items_done += 1,
            Some(ItemOutcome::Partial) => t.items_partial += 1,
            Some(ItemOutcome::Failed) => t.items_failed += 1,
            Some(ItemOutcome::Refused) => t.items_refused += 1,
            Some(ItemOutcome::Skipped) => t.items_skipped += 1,
            _ => {}
        }
        t.moved_bytes = t.moved_bytes.saturating_add(b.moved_bytes.unwrap_or(0));
        t.freed_bytes = t.freed_bytes.saturating_add(b.freed_bytes.unwrap_or(0));
    }
    t
}

fn item_totals(c: &Connection, op: &OperationId) -> Result<OperationTotals> {
    let rows = rows_from(c, op, 0)?;
    Ok(totals_from_items(
        rows.iter().map(|r| &r.body).filter(|b| b.kind == "item"),
    ))
}

pub(crate) fn is_finished(c: &Connection, op: &OperationId) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS (SELECT 1 FROM audit WHERE operation_id = ?1 AND kind = 'finished')",
        [op.as_str()],
        |r| r.get(0),
    )?)
}

const COLS: &str = "operation_id, op_seq, kind, recorded_at, plan_id, content_hash, approver, \
     method, item_id, path, action, outcome, reason, predicted_bytes, moved_bytes, freed_bytes, \
     entries_removed, entries_kept, status, items_planned, items_done, items_partial, \
     items_failed, items_refused, items_skipped, measured_delta_bytes, prev_hash, row_hash";

fn opt_int(v: Option<u64>) -> Result<Option<i64>> {
    v.map(int).transpose()
}

/// Append `body` at the end of its operation's chain (position and `prev_hash` read inside the
/// caller's write transaction).
pub(crate) fn append_row(tx: &Transaction<'_>, mut body: AuditRowBody) -> Result<AuditRow> {
    let last: Option<(u64, String)> = tx
        .query_row(
            "SELECT op_seq, row_hash FROM audit WHERE operation_id = ?1 \
             ORDER BY op_seq DESC LIMIT 1",
            [body.operation_id.as_str()],
            |r| Ok((get_u64(r, 0)?, r.get(1)?)),
        )
        .optional()?;
    let (op_seq, prev_hash) = match last {
        Some((seq, hash)) => (seq + 1, hash),
        None => (0, genesis_hash(&body.operation_id)),
    };
    body.op_seq = op_seq;
    let hash = row_hash(&prev_hash, &body);
    tx.execute(
        &format!(
            "INSERT INTO audit ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, \
             ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28)"
        ),
        params![
            body.operation_id,
            int(body.op_seq)?,
            body.kind,
            body.recorded_at,
            body.plan_id,
            body.content_hash,
            body.approver,
            body.method,
            body.item_id,
            body.path,
            body.action,
            body.outcome,
            body.reason,
            opt_int(body.predicted_bytes)?,
            opt_int(body.moved_bytes)?,
            opt_int(body.freed_bytes)?,
            opt_int(body.entries_removed)?,
            opt_int(body.entries_kept)?,
            body.status,
            opt_int(body.items_planned)?,
            opt_int(body.items_done)?,
            opt_int(body.items_partial)?,
            opt_int(body.items_failed)?,
            opt_int(body.items_refused)?,
            opt_int(body.items_skipped)?,
            body.measured_delta_bytes,
            prev_hash,
            hash,
        ],
    )?;
    Ok(AuditRow {
        body,
        prev_hash,
        row_hash: hash,
    })
}

fn get_u64(r: &Row<'_>, i: usize) -> rusqlite::Result<u64> {
    let v: i64 = r.get(i)?;
    u64::try_from(v).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(i, v))
}

fn get_opt_u64(r: &Row<'_>, i: usize) -> rusqlite::Result<Option<u64>> {
    match r.get::<_, Option<i64>>(i)? {
        None => Ok(None),
        Some(v) => u64::try_from(v)
            .map(Some)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(i, v)),
    }
}

fn row_from_sql(r: &Row<'_>) -> rusqlite::Result<AuditRow> {
    Ok(AuditRow {
        body: AuditRowBody {
            v: AUDIT_FORMAT_VERSION,
            operation_id: r.get(0)?,
            op_seq: get_u64(r, 1)?,
            kind: r.get(2)?,
            recorded_at: r.get(3)?,
            plan_id: r.get(4)?,
            content_hash: r.get(5)?,
            approver: r.get(6)?,
            method: r.get(7)?,
            item_id: r.get(8)?,
            path: r.get(9)?,
            action: r.get(10)?,
            outcome: r.get(11)?,
            reason: r.get(12)?,
            predicted_bytes: get_opt_u64(r, 13)?,
            moved_bytes: get_opt_u64(r, 14)?,
            freed_bytes: get_opt_u64(r, 15)?,
            entries_removed: get_opt_u64(r, 16)?,
            entries_kept: get_opt_u64(r, 17)?,
            status: r.get(18)?,
            items_planned: get_opt_u64(r, 19)?,
            items_done: get_opt_u64(r, 20)?,
            items_partial: get_opt_u64(r, 21)?,
            items_failed: get_opt_u64(r, 22)?,
            items_refused: get_opt_u64(r, 23)?,
            items_skipped: get_opt_u64(r, 24)?,
            measured_delta_bytes: r.get(25)?,
        },
        prev_hash: r.get(26)?,
        row_hash: r.get(27)?,
    })
}

fn rows_from(c: &Connection, op: &OperationId, from_seq: u64) -> Result<Vec<AuditRow>> {
    let mut stmt = c.prepare_cached(&format!(
        "SELECT {COLS} FROM audit WHERE operation_id = ?1 AND op_seq >= ?2 ORDER BY op_seq, seq"
    ))?;
    let rows = stmt
        .query_map(params![op.as_str(), int(from_seq)?], row_from_sql)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn item_from_row(b: &AuditRowBody) -> Result<ItemAudit> {
    let need = |v: &Option<String>, what: &str| {
        v.clone()
            .ok_or_else(|| bad(format!("audit item row {} has no {what}", b.op_seq)))
    };
    let outcome = need(&b.outcome, "outcome")?;
    Ok(ItemAudit {
        op_seq: b.op_seq,
        recorded_at: b.recorded_at,
        item_id: ItemId::try_from(need(&b.item_id, "item")?).map_err(bad)?,
        path: FsPath::from(need(&b.path, "path")?.as_str()),
        action: need(&b.action, "action")?,
        outcome: ItemOutcome::parse(&outcome)
            .ok_or_else(|| bad(format!("unknown audit outcome {outcome:?}")))?,
        reason: b.reason.clone(),
        predicted_bytes: b.predicted_bytes.unwrap_or(0),
        moved_bytes: b.moved_bytes,
        freed_bytes: b.freed_bytes,
        entries_removed: b.entries_removed,
        entries_kept: b.entries_kept,
    })
}

type OperationRow = (String, String, String, String, i64);

fn operation_row(c: &Connection, op: &OperationId) -> Result<Option<OperationRow>> {
    Ok(c.query_row(
        "SELECT plan_id, content_hash, approver, method, started_at FROM operations \
         WHERE operation_id = ?1",
        [op.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .optional()?)
}

fn operation_record(c: &Connection, op: &OperationId) -> Result<Option<OperationRecord>> {
    let Some((plan_id, hash, approver, method, started_at)) = operation_row(c, op)? else {
        return Ok(None);
    };
    let items_recorded = c.query_row(
        "SELECT count(*) FROM audit WHERE operation_id = ?1 AND kind = 'item'",
        [op.as_str()],
        |r| get_u64(r, 0),
    )?;
    let finished: Option<AuditRow> = c
        .query_row(
            &format!(
                "SELECT {COLS} FROM audit WHERE operation_id = ?1 AND kind = 'finished' \
                 ORDER BY op_seq LIMIT 1"
            ),
            [op.as_str()],
            row_from_sql,
        )
        .optional()?;
    let finished = match finished {
        None => None,
        Some(row) => {
            let status = row.body.status.clone().unwrap_or_default();
            Some(OperationSummary {
                finished_at: row.body.recorded_at,
                status: OperationStatus::parse(&status)
                    .ok_or_else(|| bad(format!("unknown operation status {status:?}")))?,
                totals: totals_of(&row.body),
            })
        }
    };
    Ok(Some(OperationRecord {
        operation_id: op.clone(),
        plan_id: PlanId::try_from(plan_id).map_err(bad)?,
        content_hash: ContentHash::try_from(hash).map_err(bad)?,
        approver,
        method: ApprovalMethod::parse(&method)
            .ok_or_else(|| bad(format!("unknown approval method {method:?}")))?,
        started_at,
        items_recorded,
        finished,
    }))
}

fn verify_chain_conn(c: &Connection, op: &OperationId) -> Result<ChainReport> {
    let rows = rows_from(c, op, 0)?;
    let op_row = operation_row(c, op)?;
    if op_row.is_none() && rows.is_empty() {
        return Err(StoreError::rejected(
            ErrorKind::NotFound,
            format!("no recorded operation {op}"),
        ));
    }
    let mut problems = Vec::new();
    if op_row.is_none() {
        problems.push(ChainProblem::UnknownOperation);
    }
    match rows.first() {
        Some(first) if first.body.kind == "started" && first.body.op_seq == 0 => {
            let b = &first.body;
            let matches = op_row.as_ref().is_some_and(|(plan, hash, who, how, at)| {
                b.plan_id.as_ref() == Some(plan)
                    && b.content_hash.as_ref() == Some(hash)
                    && b.approver.as_ref() == Some(who)
                    && b.method.as_ref() == Some(how)
                    && b.recorded_at == *at
            });
            if op_row.is_some() && !matches {
                problems.push(ChainProblem::StartMismatch);
            }
        }
        _ => problems.push(ChainProblem::MissingStart),
    }
    let mut expected_seq = 0u64;
    let mut expected_prev = genesis_hash(op.as_str());
    let mut finished_at: Option<usize> = None;
    let mut seen_items = HashSet::new();
    for (i, row) in rows.iter().enumerate() {
        let b = &row.body;
        if b.op_seq != expected_seq {
            problems.push(ChainProblem::SeqGap {
                expected: expected_seq,
                found: b.op_seq,
            });
        }
        if row.prev_hash != expected_prev {
            problems.push(ChainProblem::BrokenLink { op_seq: b.op_seq });
        }
        if row_hash(&row.prev_hash, b) != row.row_hash {
            problems.push(ChainProblem::HashMismatch { op_seq: b.op_seq });
        }
        if finished_at.is_some() || (b.kind == "started" && i != 0) {
            problems.push(ChainProblem::Misplaced { op_seq: b.op_seq });
        }
        if b.kind == "finished" && finished_at.is_none() {
            finished_at = Some(i);
        }
        if b.kind == "item" {
            let id = b.item_id.clone().unwrap_or_default();
            if !seen_items.insert(id.clone()) {
                problems.push(ChainProblem::DuplicateItem { item_id: id });
            }
        }
        expected_seq = b.op_seq + 1;
        expected_prev = row.row_hash.clone();
    }
    if let Some(fi) = finished_at {
        let fin = &rows[fi].body;
        let mut want = totals_from_items(
            rows[..fi]
                .iter()
                .map(|r| &r.body)
                .filter(|b| b.kind == "item"),
        );
        let got = totals_of(fin);
        want.items_planned = got.items_planned;
        want.predicted_bytes = got.predicted_bytes;
        want.measured_delta_bytes = got.measured_delta_bytes;
        let plan_ok = match op_row.as_ref() {
            Some((plan, ..)) => {
                let recorded: Option<(u64, u64)> = c
                    .query_row(
                        "SELECT item_count, reclaimable_bytes FROM plans WHERE plan_id = ?1",
                        [plan],
                        |r| Ok((get_u64(r, 0)?, get_u64(r, 1)?)),
                    )
                    .optional()?;
                recorded == Some((got.items_planned, got.predicted_bytes))
            }
            None => false,
        };
        if want != got || !plan_ok {
            problems.push(ChainProblem::TotalsMismatch);
        }
    }
    Ok(ChainReport {
        operation_id: op.clone(),
        rows: rows.len() as u64,
        finished: finished_at.is_some(),
        problems,
    })
}

// ---- mirror files ----------------------------------------------------------------------------

/// Create the audit directory owner-only if missing; refuse anything but a real directory.
fn ensure_audit_dir(mirror: &Path, perms: &dyn OwnerOnly) -> Result<()> {
    let Some(dir) = mirror.parent() else {
        return Ok(());
    };
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => perms
            .restrict(dir)
            .map_err(|source| StoreError::Permissions {
                path: dir.to_owned(),
                source,
            }),
        Ok(_) => Err(StoreError::rejected(
            ErrorKind::Config,
            format!("{} is not a directory; refusing to use it", dir.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match perms.create_dir(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(source) => Err(StoreError::Permissions {
                path: dir.to_owned(),
                source,
            }),
        },
        Err(e) => Err(StoreError::io(dir, e)),
    }
}

/// Create a new, empty, owner-only mirror (exclusive: never adopt a planted file or link).
fn create_mirror(path: &Path, perms: &dyn OwnerOnly) -> Result<()> {
    perms
        .create_file(path)
        .map_err(|source| StoreError::Permissions {
            path: path.to_owned(),
            source,
        })
}

fn check_regular(path: &Path) -> Result<()> {
    let m = std::fs::symlink_metadata(path).map_err(|e| StoreError::io(path, e))?;
    if m.is_file() {
        Ok(())
    } else {
        Err(StoreError::rejected(
            ErrorKind::Config,
            format!(
                "{} is not a regular file; refusing to use it",
                path.display()
            ),
        ))
    }
}

/// Append `bytes` (`O_APPEND`, never truncating) and fsync.
fn append_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    check_regular(path)?;
    let io = |e| StoreError::io(path, e);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(io)?;
    f.write_all(bytes).map_err(io)?;
    f.sync_data().map_err(io)
}

fn read_mirror(path: &Path) -> Result<Vec<u8>> {
    check_regular(path)?;
    std::fs::read(path).map_err(|e| StoreError::io(path, e))
}

/// Parsed rows and the 1-based numbers of lines that are not rows.
fn parse_mirror(bytes: &[u8]) -> (Vec<AuditRow>, Vec<u64>) {
    let mut rows = Vec::new();
    let mut bad = Vec::new();
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<AuditRow>(line) {
            Ok(r) => rows.push(r),
            Err(_) => bad.push(i as u64 + 1),
        }
    }
    (rows, bad)
}

/// Files in the audit mirror directory next to `db_path` (for the footprint).
pub(crate) fn mirror_files(db_path: &Path) -> Vec<PathBuf> {
    let dir = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join(AUDIT_DIR_NAME);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

// ---- migration v3 --------------------------------------------------------------------------

/// Rust step of migration v3: chain every recorded operation (a `started` row from its
/// `operations` row) followed by its v1 audit rows as `legacy` rows, in their original order.
/// The chain vouches for these rows from the migration on; what happened to them before is
/// covered only by the pre-migration backup.
pub(crate) fn migrate_v3_legacy(tx: &Transaction<'_>) -> Result<()> {
    let ops: Vec<(String, String, String, String, String, i64)> = {
        let mut stmt = tx.prepare(
            "SELECT operation_id, plan_id, content_hash, approver, method, started_at \
             FROM operations ORDER BY started_at, operation_id",
        )?;
        stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    for (op, plan_id, hash, approver, method, started_at) in ops {
        let op_id = OperationId::try_from(op.clone()).map_err(bad)?;
        let mut started = AuditRowBody::new(&op_id, "started", started_at);
        started.plan_id = Some(plan_id);
        started.content_hash = Some(hash);
        started.approver = Some(approver);
        started.method = Some(method);
        append_row(tx, started)?;
        let legacy: Vec<AuditRowBody> = {
            let mut stmt = tx.prepare(
                "SELECT recorded_at, item_id, path, action, outcome, reason, predicted_bytes, \
                 measured_bytes FROM audit_v1 WHERE operation_id = ?1 ORDER BY seq",
            )?;
            stmt.query_map([&op], |r| {
                let mut b = AuditRowBody::new(&op_id, "legacy", r.get(0)?);
                b.item_id = r.get(1)?;
                b.path = r.get(2)?;
                b.action = r.get(3)?;
                b.outcome = r.get(4)?;
                b.reason = r.get(5)?;
                b.predicted_bytes = get_opt_u64(r, 6)?;
                b.freed_bytes = get_opt_u64(r, 7)?;
                Ok(b)
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        for b in legacy {
            append_row(tx, b)?;
        }
    }
    Ok(())
}
