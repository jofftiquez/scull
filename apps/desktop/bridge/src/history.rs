//! `history`: the cleanup operations recorded in scull's database, each with its item outcomes,
//! the human-readable audit log and the result of verifying its hash chain (History screen,
//! design 09).
//!
//! # Read-only
//!
//! The database is never created, migrated or written: [`read_history`] first probes the file
//! header with a plain file read (no SQLite) and only then reads it. A missing database reads
//! `no_database`; anything other than a current scull database reads `unavailable`, with the
//! reason, and the file is left untouched. Same approach as `scull status`
//! (`scull_cli::db::open_current`), reimplemented here because the bridge must not depend on
//! the CLI crate (which links the executor once Phase 2 lands; SAFE-10).
//!
//! # What this build records
//!
//! Operations and their audit chain are written by the Phase 2 Executor (`scull-cleaner`,
//! P2-EXEC-09) through `scull-storage`'s audit API (`operations`, `audit_items`,
//! `verify_chain`). This build of the core has neither the Executor nor that API, so nothing
//! can have recorded an operation: a current database reads `ok` with no operations and
//! `audit_api: false`. When the audit API is available, [`read_history`] fills
//! [`HistoryView::operations`] from it, newest first, with the same read-only open.
//!
//! Restoring a quarantined item is a mutation, so it is not here: the History screen calls a
//! screen-local stub (`apps/desktop/src/components/history/bridge.ts`) until the restore
//! command goes through the core (never overwriting: a taken path restores to the sibling
//! `<name>.scull-restored-<n>`, P2-EXEC-06).

use schemars::JsonSchema;
use scull_core::Tier;
use scull_platform::{EntryKind, Native, Platform};
use scull_storage::{APPLICATION_ID, SCHEMA_VERSION};
use serde::Serialize;
use std::io::Read;
use std::path::Path;

/// Whether there is history to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryReadState {
    /// The database was read.
    Ok,
    /// No database yet (nothing was ever recorded).
    NoDatabase,
    /// The database can't be read by this build (newer or older schema, damaged, foreign);
    /// `detail` says why. It was not touched.
    Unavailable,
}

/// What an operation did, for the timeline's icon and wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// A reviewed cleanup plan (Cleanup Review, Projects).
    Cleanup,
    /// Quick Clean: package-manager tools run on their caches.
    QuickClean,
    /// `git worktree remove` of whole worktrees (branches kept).
    Worktrees,
    /// A plan an AI agent prepared through MCP and a human approved outside the chat.
    Agent,
}

/// How an operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    /// Every item has an outcome and none failed.
    Completed,
    /// Finished, but some items failed or were refused.
    CompletedWithErrors,
    /// Nothing was done.
    Failed,
    /// The user stopped it; the remaining items were skipped.
    Cancelled,
    /// scull stopped mid-way (crash, power loss); recovered from the journal.
    Interrupted,
    /// Still running (no `finished` row yet).
    Running,
}

/// The outcome of one plan item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemOutcome {
    /// Deleted by the Executor.
    Removed,
    /// Moved to scull's same-volume quarantine; restorable until `restorable_until`.
    Quarantined,
    /// Moved to the system Trash / Recycle Bin.
    MovedToTrash,
    /// Cleaned by the package manager's own command (Quick Clean).
    ToolRun,
    /// Partly removed (some entries were kept or failed).
    Partial,
    /// Failed; nothing or part of it was removed (`reason`).
    Failed,
    /// Refused by a safety re-check at execution time (`reason`).
    Refused,
    /// Skipped (changed since the plan, or the operation was cancelled).
    Skipped,
    /// Was quarantined, then restored (`restored_to`).
    Restored,
    /// Was quarantined, then purged when the retention ended.
    Purged,
}

/// One item of an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryItem {
    /// The plan item's identifier.
    pub item_id: String,
    /// The path the plan named (or the tool's cache for Quick Clean).
    pub path: String,
    /// Short label when the path alone doesn't say it (e.g. `pnpm store (pruned)`).
    pub label: Option<String>,
    /// Planned action (`delete`, `quarantine`, `trash`, `tool`, `worktree_remove`).
    pub action: String,
    /// Safety tier at planning time.
    pub tier: Option<Tier>,
    /// What happened.
    pub outcome: ItemOutcome,
    /// Reason code for failed, refused, skipped or partial outcomes.
    pub reason: Option<String>,
    /// Bytes the plan predicted.
    pub predicted_bytes: u64,
    /// Bytes freed (deleted or tool-cleaned).
    pub freed_bytes: Option<u64>,
    /// Bytes moved (quarantine or Trash).
    pub moved_bytes: Option<u64>,
    /// For quarantined items: when the quarantine retention ends (Unix seconds).
    pub restorable_until: Option<i64>,
    /// For restored items: where it went back to (the original path, or a
    /// `<name>.scull-restored-<n>` sibling when the original was taken; never overwritten).
    pub restored_to: Option<String>,
}

/// Kind of an audit log line, for its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditLineKind {
    /// Plan approval (who, how).
    Approval,
    /// Re-checks and tool resolution.
    Check,
    /// Something was done (deleted, quarantined, tool run, restored).
    Action,
    /// Measured free-space delta.
    Measured,
    /// A warning (e.g. a local snapshot held space).
    Warning,
}

/// One line of an operation's audit log, rendered from its chain rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditLine {
    /// When (Unix seconds).
    pub at: i64,
    /// Line kind.
    pub kind: AuditLineKind,
    /// Text, e.g. `plan 7f3a·c21e approved · desktop`.
    pub text: String,
}

/// Result of verifying an operation's hash chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChainCheck {
    /// No tampering or damage found.
    pub intact: bool,
    /// Chain rows checked.
    pub rows: u64,
    /// What was wrong, when something was.
    pub problems: Vec<String>,
}

/// One recorded operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryOperation {
    /// Operation identifier.
    pub operation_id: String,
    /// The approved plan it executed.
    pub plan_id: String,
    /// What it did.
    pub kind: OperationKind,
    /// Who approved it (`you`, or the agent that prepared it and `you`).
    pub approver: String,
    /// Where it was approved (`desktop`, `cli`, `desktop_dialog`).
    pub method: String,
    /// Where it was started from (`Cleanup Review`, `Overview`, ...), when known.
    pub origin: Option<String>,
    /// The agent that prepared the plan (agent operations).
    pub agent: Option<String>,
    /// The plan's one-line description, when it has one.
    pub summary: Option<String>,
    /// Start (Unix seconds).
    pub started_at: i64,
    /// Finish (Unix seconds), `None` while running.
    pub finished_at: Option<i64>,
    /// How it ended.
    pub status: OperationStatus,
    /// Bytes the plan predicted.
    pub predicted_bytes: u64,
    /// Measured change in free space (bytes; may be negative or missing).
    pub measured_delta_bytes: Option<i64>,
    /// Why the measured delta differs from the prediction (codes, e.g. `local_snapshot`).
    pub delta_explanations: Vec<String>,
    /// Items, in plan order.
    pub items: Vec<HistoryItem>,
    /// Audit log, in chain order.
    pub log: Vec<AuditLine>,
    /// Hash-chain verification.
    pub chain: ChainCheck,
}

/// `history`: every recorded operation, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryView {
    /// Whether the database was read.
    pub state: HistoryReadState,
    /// Why it couldn't be, when it couldn't.
    pub detail: Option<String>,
    /// When this report was produced (Unix seconds).
    pub now: i64,
    /// Whether this build of the core can read operations and their audit chain. `false`
    /// before the Phase 2 audit API: the list is then empty by construction.
    pub audit_api: bool,
    /// Operations, newest first.
    pub operations: Vec<HistoryOperation>,
}

impl HistoryView {
    fn empty(now: i64, state: HistoryReadState, detail: Option<String>) -> Self {
        Self {
            state,
            detail,
            now,
            audit_api: false,
            operations: Vec::new(),
        }
    }
}

/// What the database file looks like from its header (no SQLite involved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Nothing there (or an empty, never-initialised file).
    Missing,
    /// A current scull database.
    Current,
    /// Something this build can't read, and why.
    NotCurrent(String),
}

const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Classify the first 100 bytes of a database file (SQLite's documented header: the magic
/// string, `user_version` at offset 60, `application_id` at 68, both big-endian).
pub fn classify_header(header: &[u8]) -> Probe {
    if header.is_empty() {
        return Probe::Missing;
    }
    if header.len() < 100 || &header[..16] != MAGIC {
        return Probe::NotCurrent("not a SQLite database".into());
    }
    let word = |at: usize| [header[at], header[at + 1], header[at + 2], header[at + 3]];
    let version = u32::from_be_bytes(word(60));
    let app = i32::from_be_bytes(word(68));
    match (app, version) {
        (0, 0) => Probe::Missing,
        (id, _) if id != APPLICATION_ID => {
            Probe::NotCurrent("not a scull database (another application's)".into())
        }
        (_, v) if v == SCHEMA_VERSION => Probe::Current,
        (_, v) => Probe::NotCurrent(format!(
            "schema version {v} (this build reads {SCHEMA_VERSION}); run `scull status` in a \
             terminal"
        )),
    }
}

/// Probe the database at `path` with a plain read of its header. The entry's kind comes from
/// the platform's no-follow metadata, so a link at `path` is reported, never followed; nothing
/// is created.
pub fn probe(path: &Path) -> Probe {
    match Native.metadata(path) {
        Ok(md) if md.kind == EntryKind::File => {}
        Ok(_) => return Probe::NotCurrent("not a regular file".into()),
        Err(e) if e.is_not_found() => return Probe::Missing,
        Err(e) => return Probe::NotCurrent(format!("cannot read it: {e}")),
    }
    let mut header = Vec::with_capacity(100);
    match std::fs::File::open(path).and_then(|f| f.take(100).read_to_end(&mut header)) {
        Ok(_) => classify_header(&header),
        Err(e) => Probe::NotCurrent(format!("cannot read it: {e}")),
    }
}

/// Read the operation history from the database in `data_dir` (read-only; see the module
/// docs). `None`: no data directory, so nothing was ever recorded.
pub fn read_history(data_dir: Option<&Path>, now: i64) -> HistoryView {
    let Some(dir) = data_dir else {
        return HistoryView::empty(now, HistoryReadState::NoDatabase, None);
    };
    match probe(&dir.join(scull_storage::DB_FILE_NAME)) {
        Probe::Missing => HistoryView::empty(now, HistoryReadState::NoDatabase, None),
        Probe::NotCurrent(why) => HistoryView::empty(now, HistoryReadState::Unavailable, Some(why)),
        // This core has no audit API (Phase 2, P2-EXEC-09) and no Executor, so a current
        // database holds no operations. With the API: open it without creating or migrating
        // anything, then `operations(0)` newest first, `audit_items` and `verify_chain` for
        // each.
        Probe::Current => HistoryView::empty(now, HistoryReadState::Ok, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(version: u32, app: i32) -> Vec<u8> {
        let mut h = vec![0u8; 100];
        h[..16].copy_from_slice(MAGIC);
        h[60..64].copy_from_slice(&version.to_be_bytes());
        h[68..72].copy_from_slice(&app.to_be_bytes());
        h
    }

    #[test]
    fn headers_are_classified() {
        assert_eq!(classify_header(b""), Probe::Missing);
        assert!(matches!(classify_header(b"hello"), Probe::NotCurrent(_)));
        assert_eq!(classify_header(&header(0, 0)), Probe::Missing);
        assert_eq!(
            classify_header(&header(SCHEMA_VERSION, APPLICATION_ID)),
            Probe::Current
        );
        let Probe::NotCurrent(why) = classify_header(&header(SCHEMA_VERSION + 1, APPLICATION_ID))
        else {
            panic!("newer schema must not read as current");
        };
        assert!(why.contains("schema version"), "{why}");
        assert!(matches!(
            classify_header(&header(SCHEMA_VERSION, 7)),
            Probe::NotCurrent(_)
        ));
    }

    #[test]
    fn missing_database_is_never_created() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let v = read_history(Some(dir.path()), 10);
        assert_eq!(v.state, HistoryReadState::NoDatabase);
        assert!(v.operations.is_empty());
        assert!(!dir.path().join(scull_storage::DB_FILE_NAME).exists());
        assert_eq!(
            read_history(None, 10).state,
            HistoryReadState::NoDatabase,
            "no data directory"
        );
    }
}
