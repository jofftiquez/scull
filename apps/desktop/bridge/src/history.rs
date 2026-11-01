//! `history`: the cleanup operations recorded in scull's database, each with its item outcomes,
//! the human-readable audit log and the result of verifying its hash chain (History screen,
//! design 09).
//!
//! # Read-only
//!
//! The database is never created, migrated or written: [`read_history`] first probes the file
//! header with a plain file read (no SQLite) and only then opens it with
//! [`Store::open_read_only`] (`SQLITE_OPEN_READ_ONLY`, `query_only`). A missing database reads
//! `no_database`; anything other than a current scull database reads `unavailable`, with the
//! reason, and the file is left untouched. Reading a current database leaves it and the audit
//! mirrors byte-identical; like any SQLite reader of a WAL database it may create the WAL index
//! (`-shm`) and an empty `-wal` beside it. Same approach as `scull status`
//! (`scull_cli::db::open_current`), reimplemented here because the bridge must not depend on
//! the CLI crate (which links the executor once Phase 2 lands; SAFE-10).
//!
//! # What is shown
//!
//! Operations and their audit chain are written by the Executor (`scull-cleaner`, P2-EXEC-09)
//! and Quick Clean through `scull-storage`'s audit API. [`read_history`] reads them back with
//! that API (`audit_api: true`): [`Store::operations`] newest first, each with its recorded
//! plan ([`Store::get_plan`]: kind, tiers, item order), its item outcomes
//! ([`Store::audit_items`]), its chain rows as the audit log ([`Store::audit_rows`]), the
//! verification of its hash chain ([`Store::verify_chain`]) and, for quarantined items, the
//! manifest of record ([`Store::quarantine_entries`]: restorable until, restored to, purged).
//! A plan item without an outcome (cancelled or interrupted operation) reads `skipped` with the
//! reason `not_attempted`.
//!
//! Restoring a quarantined item is a mutation, so it is not here: the History screen calls a
//! screen-local stub (`apps/desktop/src/components/history/bridge.ts`) until the restore
//! command goes through the core (never overwriting: a taken path restores to the sibling
//! `<name>.scull-restored-<n>`, P2-EXEC-06).

use crate::db::DbState;
use schemars::JsonSchema;
use scull_core::{Action, Plan, PlanOrigin, Tier};
use scull_storage::{
    AuditRow, ItemAudit, OperationRecord, QuarantineEntry, Store, StoreError, StoreOptions,
};
use serde::Serialize;
use std::collections::BTreeMap;
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
    /// Whether this build of the core can read operations and their audit chain (always
    /// `true` since the Phase 2 audit API; `false` only from a build without it, whose list
    /// is empty by construction).
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
            audit_api: true,
            operations: Vec::new(),
        }
    }
}

/// Read the operation history from the database in `data_dir` (read-only; see the module
/// docs). `None`: no data directory, so nothing was ever recorded.
pub fn read_history(data_dir: Option<&Path>, now: i64) -> HistoryView {
    let Some(dir) = data_dir else {
        return HistoryView::empty(now, HistoryReadState::NoDatabase, None);
    };
    let probe = crate::db::probe(&dir.join(scull_storage::DB_FILE_NAME));
    match probe.state {
        DbState::Missing | DbState::Empty => {
            HistoryView::empty(now, HistoryReadState::NoDatabase, None)
        }
        DbState::NewerSchema
        | DbState::NeedsMigration
        | DbState::NotScull
        | DbState::Unreadable => {
            HistoryView::empty(now, HistoryReadState::Unavailable, probe.detail)
        }
        DbState::Ok => {
            let db = dir.join(scull_storage::DB_FILE_NAME);
            match Store::open_read_only(&db, &StoreOptions::default())
                .and_then(|store| read_operations(&store, now))
            {
                Ok(operations) => HistoryView {
                    operations,
                    ..HistoryView::empty(now, HistoryReadState::Ok, None)
                },
                Err(e) => {
                    HistoryView::empty(now, HistoryReadState::Unavailable, Some(e.to_string()))
                }
            }
        }
    }
}

/// Every recorded operation, newest first.
fn read_operations(store: &Store, now: i64) -> Result<Vec<HistoryOperation>, StoreError> {
    let mut out = Vec::new();
    for rec in store.operations(0)?.into_iter().rev() {
        out.push(read_operation(store, &rec, now)?);
    }
    Ok(out)
}

fn read_operation(
    store: &Store,
    rec: &OperationRecord,
    now: i64,
) -> Result<HistoryOperation, StoreError> {
    let id = &rec.operation_id;
    let plan = store.get_plan(&rec.plan_id)?;
    let audited = store.audit_items(id)?;
    let quarantine: BTreeMap<String, QuarantineEntry> = store
        .quarantine_entries(Some(id))?
        .into_iter()
        .map(|q| (q.entry.item_id.as_str().to_owned(), q))
        .collect();
    let chain = match store.verify_chain(id) {
        Ok(c) => ChainCheck {
            intact: c.is_intact(),
            rows: c.rows,
            problems: c.problems.iter().map(|p| format!("{p:?}")).collect(),
        },
        Err(e) => ChainCheck {
            intact: false,
            rows: 0,
            problems: vec![e.to_string()],
        },
    };
    let log = store
        .audit_rows(id)?
        .iter()
        .filter_map(audit_line)
        .collect();
    let items = items_of(plan.as_ref(), &audited, &quarantine, now);
    let (finished_at, status, totals) = match &rec.finished {
        Some(f) => (Some(f.finished_at), status_of(f.status), Some(f.totals)),
        None => (None, OperationStatus::Running, None),
    };
    let predicted_bytes = totals.map_or_else(
        || {
            plan.as_ref()
                .map_or(0, |p| p.items().iter().map(|i| i.reclaimable_bytes).sum())
        },
        |t| t.predicted_bytes,
    );
    Ok(HistoryOperation {
        operation_id: id.as_str().to_owned(),
        plan_id: rec.plan_id.as_str().to_owned(),
        kind: kind_of(plan.as_ref()),
        approver: rec.approver.clone(),
        method: rec.method.as_str().to_owned(),
        origin: plan.as_ref().map(|p| origin_label(p.origin()).to_owned()),
        agent: None,
        summary: None,
        started_at: rec.started_at,
        finished_at,
        status,
        predicted_bytes,
        measured_delta_bytes: totals.and_then(|t| t.measured_delta_bytes),
        delta_explanations: Vec::new(),
        items,
        log,
        chain,
    })
}

/// The operation's items in plan order (with the plan), else in chain order.
fn items_of(
    plan: Option<&Plan>,
    audited: &[ItemAudit],
    quarantine: &BTreeMap<String, QuarantineEntry>,
    now: i64,
) -> Vec<HistoryItem> {
    let by_id: BTreeMap<&str, &ItemAudit> =
        audited.iter().map(|a| (a.item_id.as_str(), a)).collect();
    let Some(plan) = plan else {
        return audited
            .iter()
            .map(|a| item(a.item_id.as_str(), None, Some(a), quarantine, now))
            .collect();
    };
    let mut out: Vec<HistoryItem> = plan
        .items()
        .iter()
        .map(|pi| {
            let a = by_id.get(pi.item_id.as_str()).copied();
            item(pi.item_id.as_str(), Some(pi), a, quarantine, now)
        })
        .collect();
    // An outcome for an item the plan doesn't name (only a damaged record).
    out.extend(
        audited
            .iter()
            .filter(|a| !plan.items().iter().any(|pi| pi.item_id == a.item_id))
            .map(|a| item(a.item_id.as_str(), None, Some(a), quarantine, now)),
    );
    out
}

fn item(
    item_id: &str,
    planned: Option<&scull_core::PlanItem>,
    audited: Option<&ItemAudit>,
    quarantine: &BTreeMap<String, QuarantineEntry>,
    now: i64,
) -> HistoryItem {
    let mut outcome = audited.map_or(ItemOutcome::Skipped, |a| outcome_of(a.outcome));
    let mut reason = match audited {
        Some(a) => a.reason.clone(),
        None => Some("not_attempted".to_owned()),
    };
    let mut restorable_until = None;
    let mut restored_to = None;
    if outcome == ItemOutcome::Quarantined
        && let Some(q) = quarantine.get(item_id)
    {
        if let Some((_, to)) = &q.restored {
            outcome = ItemOutcome::Restored;
            restored_to = Some(to.as_str().to_owned());
        } else if q.purged.is_some() {
            outcome = ItemOutcome::Purged;
        } else {
            restorable_until = q.entry.expires_at;
            if restorable_until.is_some_and(|t| t <= now) {
                reason.get_or_insert_with(|| "retention_ended".to_owned());
            }
        }
    }
    HistoryItem {
        item_id: item_id.to_owned(),
        path: planned.map_or_else(
            || {
                audited
                    .map(|a| a.path.as_str().to_owned())
                    .unwrap_or_default()
            },
            |p| p.path.as_str().to_owned(),
        ),
        label: planned.and_then(|p| tool_label(&p.action)),
        action: match (planned, audited) {
            (Some(p), _) => action_label(&p.action).to_owned(),
            (None, Some(a)) if a.action == "tool_command" => "tool".to_owned(),
            (None, Some(a)) => a.action.clone(),
            (None, None) => "none".to_owned(),
        },
        tier: planned.map(|p| p.tier),
        outcome,
        reason,
        predicted_bytes: audited.map_or_else(
            || planned.map_or(0, |p| p.reclaimable_bytes),
            |a| a.predicted_bytes,
        ),
        freed_bytes: audited.and_then(|a| a.freed_bytes),
        moved_bytes: audited.and_then(|a| a.moved_bytes),
        restorable_until,
        restored_to,
    }
}

/// One audit log line per chain row.
fn audit_line(row: &AuditRow) -> Option<AuditLine> {
    let b = &row.body;
    let s = |o: &Option<String>| o.clone().unwrap_or_default();
    let (kind, text) = match b.kind.as_str() {
        "started" => (
            AuditLineKind::Approval,
            format!(
                "plan {} approved · {} · {}",
                s(&b.plan_id),
                s(&b.method),
                s(&b.approver)
            ),
        ),
        "item" => {
            let outcome = s(&b.outcome);
            let kind = match outcome.as_str() {
                "removed" | "quarantined" | "moved_to_trash" | "tool_run" => AuditLineKind::Action,
                "partial" | "failed" => AuditLineKind::Warning,
                _ => AuditLineKind::Check,
            };
            let reason = b
                .reason
                .as_deref()
                .map_or_else(String::new, |r| format!(" ({r})"));
            (kind, format!("{outcome} {}{reason}", s(&b.path)))
        }
        "finished" => match b.measured_delta_bytes {
            Some(d) => (
                AuditLineKind::Measured,
                format!(
                    "finished {} · measured {d} B free-space change",
                    s(&b.status)
                ),
            ),
            None => (AuditLineKind::Action, format!("finished {}", s(&b.status))),
        },
        _ => return None,
    };
    Some(AuditLine {
        at: b.recorded_at,
        kind,
        text,
    })
}

fn kind_of(plan: Option<&Plan>) -> OperationKind {
    let Some(plan) = plan else {
        return OperationKind::Cleanup;
    };
    let items = plan.items();
    if plan.origin() == PlanOrigin::Mcp {
        OperationKind::Agent
    } else if !items.is_empty()
        && items
            .iter()
            .all(|i| matches!(i.action, Action::ToolCommand { .. }))
    {
        OperationKind::QuickClean
    } else if items.iter().any(|i| {
        matches!(
            i.action,
            Action::GitWorktreeRemove | Action::GitWorktreePrune
        )
    }) {
        OperationKind::Worktrees
    } else {
        OperationKind::Cleanup
    }
}

fn origin_label(o: PlanOrigin) -> &'static str {
    match o {
        PlanOrigin::Cli => "CLI",
        PlanOrigin::Desktop => "Desktop",
        PlanOrigin::Mcp => "MCP agent",
        PlanOrigin::Policy => "Auto-policy",
    }
}

fn action_label(a: &Action) -> &'static str {
    match a {
        Action::None => "none",
        Action::Delete => "delete",
        Action::Quarantine => "quarantine",
        Action::Trash => "trash",
        Action::ToolCommand { .. } => "tool",
        Action::GitWorktreeRemove => "worktree_remove",
        Action::GitWorktreePrune => "worktree_prune",
        _ => "other",
    }
}

/// `npm cache verify` for a recorded tool command (binary file name and its leading words).
fn tool_label(a: &Action) -> Option<String> {
    let Action::ToolCommand { tool, args } = a else {
        return None;
    };
    let name = Path::new(tool)
        .file_stem()
        .map_or_else(|| tool.clone(), |n| n.to_string_lossy().into_owned());
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .skip_while(|w| Path::new(w).is_absolute())
        .take_while(|w| !w.starts_with('-') && !Path::new(w).is_absolute())
        .collect();
    Some(if words.is_empty() {
        name
    } else {
        format!("{name} {}", words.join(" "))
    })
}

fn status_of(s: scull_storage::OperationStatus) -> OperationStatus {
    use scull_storage::OperationStatus as S;
    match s {
        S::Completed => OperationStatus::Completed,
        S::CompletedWithErrors => OperationStatus::CompletedWithErrors,
        S::Failed => OperationStatus::Failed,
        S::Cancelled => OperationStatus::Cancelled,
        S::Interrupted => OperationStatus::Interrupted,
        // A status a newer core added: shown as needing attention.
        _ => OperationStatus::CompletedWithErrors,
    }
}

fn outcome_of(o: scull_storage::ItemOutcome) -> ItemOutcome {
    use scull_storage::ItemOutcome as O;
    match o {
        O::Removed => ItemOutcome::Removed,
        O::Quarantined => ItemOutcome::Quarantined,
        O::MovedToTrash => ItemOutcome::MovedToTrash,
        O::ToolRun => ItemOutcome::ToolRun,
        O::Partial => ItemOutcome::Partial,
        O::Failed => ItemOutcome::Failed,
        O::Refused => ItemOutcome::Refused,
        O::Skipped => ItemOutcome::Skipped,
        // An outcome a newer core added: shown as needing attention.
        _ => ItemOutcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
