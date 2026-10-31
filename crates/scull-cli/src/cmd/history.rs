//! `scull history [OPERATION_ID] [--since AGE]` (P2-CLI-03; PRD §12, SAFE-08): what past
//! cleanup operations did, read from the append-only audit log. Read-only.
//!
//! * Without an ID: one line per operation, newest first: when, status, items done of
//!   planned, bytes freed and moved, quarantined items still restorable, and whether the
//!   operation's audit hash chain verifies (`Store::verify_chain`).
//! * With an ID: the operation in detail: plan and content hash, approver and method, totals
//!   (predicted, freed, moved, measured delta), the chain check with every problem found, one
//!   line per recorded item outcome, and its quarantine manifest entries (held, restored where,
//!   purged when) with the `scull restore` command for what is still held.
//!
//! Machine output: `scull/history@1` and `scull/history-operation@1`.

use super::{emit_machine, emit_text, exec};
use crate::cli::HistoryArgs;
use crate::db::{self, Probe};
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use crate::output::style::Style;
use crate::output::table::{Align, Cell, Table};
use schemars::JsonSchema;
use scull_core::{ContentHash, ErrorKind, FsPath, ItemId, OperationId, PlanId, ScullError};
use scull_storage::{OperationRecord, QuarantineEntry, Store};
use serde::Serialize;

/// Schema of `scull history --json`.
pub const HISTORY_SCHEMA: SchemaId = SchemaId::new("history", 1);
/// Schema of `scull history <operation-id> --json`.
pub const HISTORY_OPERATION_SCHEMA: SchemaId = SchemaId::new("history-operation", 1);

/// `scull history` (`scull/history@1`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryReport {
    /// Operations, newest first.
    pub operations: Vec<OperationLine>,
}

/// One operation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationLine {
    /// Operation ID (for `scull history <id>` and `scull restore <id>`).
    pub operation_id: OperationId,
    /// The executed plan.
    pub plan_id: PlanId,
    /// Who approved it.
    pub approver: String,
    /// How (`cli_interactive`, `cli_tty`, `desktop`).
    pub method: String,
    /// Start (Unix seconds).
    pub started_at: i64,
    /// End, once finished.
    pub finished_at: Option<i64>,
    /// `completed`, `completed_with_errors`, `failed`, `cancelled`, `interrupted`, or
    /// `unfinished` (crash recovery closes it on the next destructive run).
    pub status: String,
    /// Items in the plan (0 while unfinished).
    pub items_planned: u64,
    /// Items with a recorded outcome.
    pub items_recorded: u64,
    /// Items removed, quarantined, moved to the Trash or cleaned by their tool.
    pub items_done: u64,
    /// Items partly processed, failed or refused.
    pub items_with_errors: u64,
    /// Items skipped at revalidation.
    pub items_skipped: u64,
    /// Bytes the plan predicted.
    pub predicted_bytes: u64,
    /// Bytes freed by removal.
    pub freed_bytes: u64,
    /// Bytes moved aside (quarantine, Trash).
    pub moved_bytes: u64,
    /// Measured free-space delta, if recorded.
    pub measured_delta_bytes: Option<i64>,
    /// Quarantined items still held (restorable).
    pub quarantine_held: u64,
    /// Whether the audit hash chain verifies.
    pub chain_intact: bool,
}

/// `scull history <operation-id>` (`scull/history-operation@1`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationDetail {
    /// The summary line.
    pub operation: OperationLine,
    /// The executed plan's content hash.
    pub content_hash: ContentHash,
    /// The audit chain check.
    pub chain: ChainLine,
    /// One entry per recorded item outcome, in chain order.
    pub items: Vec<ItemLine>,
    /// The operation's quarantine manifest entries.
    pub quarantine: Vec<QuarantineLine>,
    /// How to restore what is still held.
    pub restore_hint: Option<String>,
}

/// The audit chain check of one operation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChainLine {
    /// No problem found.
    pub intact: bool,
    /// Rows found.
    pub rows: u64,
    /// Whether the `finished` row exists.
    pub finished: bool,
    /// Every problem, in chain order.
    pub problems: Vec<String>,
}

/// One recorded item outcome.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ItemLine {
    /// Position in the chain.
    pub op_seq: u64,
    /// When it was recorded.
    pub recorded_at: i64,
    /// The plan item.
    pub item_id: ItemId,
    /// Its path in the plan.
    pub path: FsPath,
    /// The plan's action.
    pub action: String,
    /// What happened (`removed`, `quarantined`, `skipped`, ...).
    pub outcome: String,
    /// Reason code.
    pub reason: Option<String>,
    /// Predicted bytes.
    pub predicted_bytes: u64,
    /// Bytes freed.
    pub freed_bytes: Option<u64>,
    /// Bytes moved.
    pub moved_bytes: Option<u64>,
}

/// One quarantine manifest entry.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuarantineLine {
    /// Manifest entry number (`scull restore <op> --item <n>`).
    pub entry_id: i64,
    /// The plan item.
    pub item_id: ItemId,
    /// Where it came from (and goes back to).
    pub original_path: FsPath,
    /// Where it is held.
    pub quarantine_path: FsPath,
    /// Allocated bytes held.
    pub bytes: u64,
    /// When it was quarantined.
    pub quarantined_at: i64,
    /// When its retention ends.
    pub expires_at: Option<i64>,
    /// `held`, `restored` or `purged`.
    pub state: String,
    /// Where it was restored to.
    pub restored_to: Option<FsPath>,
    /// When it was purged.
    pub purged_at: Option<i64>,
}

/// Run `scull history`.
pub fn run(args: &HistoryArgs, ui: &Ui) -> Result<(), ScullError> {
    let dirs = exec::dirs()?;
    let path = db::db_path(&dirs.data);
    let missing = matches!(db::probe(&path), Ok(Probe::Missing | Probe::Uninitialized));
    match &args.operation_id {
        None => {
            let operations = if missing {
                Vec::new()
            } else {
                let store = exec::open_store()?;
                let since = args.since.map_or(0, |s| {
                    scull_engine::now_unix().saturating_sub(i64::try_from(s).unwrap_or(i64::MAX))
                });
                list(&store, since)?
            };
            let r = HistoryReport { operations };
            if ui.format.is_machine() {
                emit_machine(ui.format, HISTORY_SCHEMA, "history", &r)
            } else {
                emit_text(&render_list(&r, ui.color_out))
            }
        }
        Some(id) => {
            let op = parse_operation_id(id)?;
            if missing {
                return Err(not_found(&op));
            }
            let store = exec::open_store()?;
            let d = detail(&store, &op)?;
            if ui.format.is_machine() {
                emit_machine(ui.format, HISTORY_OPERATION_SCHEMA, "operation", &d)
            } else {
                emit_text(&render_detail(&d, ui.color_out))
            }
        }
    }
}

/// Parse an operation ID given on the command line.
pub fn parse_operation_id(s: &str) -> Result<OperationId, ScullError> {
    OperationId::try_from(s.trim().to_owned()).map_err(|_| {
        ScullError::new(
            ErrorKind::Usage,
            format!(
                "{} is not an operation ID (they look like op_6ae6...; see `scull history`)",
                escape_for_terminal(s)
            ),
        )
    })
}

fn not_found(op: &OperationId) -> ScullError {
    ScullError::new(
        ErrorKind::NotFound,
        format!("no operation {op} is recorded; see `scull history`"),
    )
}

fn held(entries: &[QuarantineEntry]) -> u64 {
    entries.iter().filter(|e| e.is_held()).count() as u64
}

fn line(store: &Store, r: &OperationRecord) -> Result<OperationLine, ScullError> {
    let chain = store.verify_chain(&r.operation_id)?;
    let q = store.quarantine_entries(Some(&r.operation_id))?;
    let (finished_at, status, t) = match &r.finished {
        Some(f) => (
            Some(f.finished_at),
            f.status.as_str().to_owned(),
            Some(f.totals),
        ),
        None => (None, "unfinished".to_owned(), None),
    };
    Ok(OperationLine {
        operation_id: r.operation_id.clone(),
        plan_id: r.plan_id.clone(),
        approver: r.approver.clone(),
        method: r.method.as_str().to_owned(),
        started_at: r.started_at,
        finished_at,
        status,
        items_planned: t.map_or(0, |t| t.items_planned),
        items_recorded: r.items_recorded,
        items_done: t.map_or(0, |t| t.items_done),
        items_with_errors: t.map_or(0, |t| t.items_partial + t.items_failed + t.items_refused),
        items_skipped: t.map_or(0, |t| t.items_skipped),
        predicted_bytes: t.map_or(0, |t| t.predicted_bytes),
        freed_bytes: t.map_or(0, |t| t.freed_bytes),
        moved_bytes: t.map_or(0, |t| t.moved_bytes),
        measured_delta_bytes: t.and_then(|t| t.measured_delta_bytes),
        quarantine_held: held(&q),
        chain_intact: chain.is_intact(),
    })
}

/// Every operation started at or after `since`, newest first.
pub fn list(store: &Store, since: i64) -> Result<Vec<OperationLine>, ScullError> {
    let mut ops = store.operations(since)?;
    ops.sort_by(|a, b| {
        (b.started_at, b.operation_id.as_str()).cmp(&(a.started_at, a.operation_id.as_str()))
    });
    ops.iter().map(|r| line(store, r)).collect()
}

/// One operation in detail.
pub fn detail(store: &Store, op: &OperationId) -> Result<OperationDetail, ScullError> {
    let r = store.operation(op)?.ok_or_else(|| not_found(op))?;
    let summary = line(store, &r)?;
    let chain = store.verify_chain(op)?;
    let items = store
        .audit_items(op)?
        .into_iter()
        .map(|i| ItemLine {
            op_seq: i.op_seq,
            recorded_at: i.recorded_at,
            item_id: i.item_id,
            path: i.path,
            action: i.action,
            outcome: i.outcome.as_str().to_owned(),
            reason: i.reason,
            predicted_bytes: i.predicted_bytes,
            freed_bytes: i.freed_bytes,
            moved_bytes: i.moved_bytes,
        })
        .collect();
    let quarantine: Vec<QuarantineLine> = store
        .quarantine_entries(Some(op))?
        .into_iter()
        .map(|e| QuarantineLine {
            entry_id: e.entry_id,
            item_id: e.entry.item_id.clone(),
            original_path: e.entry.original_path.clone(),
            quarantine_path: e.entry.quarantine_path.clone(),
            bytes: e.entry.bytes,
            quarantined_at: e.entry.quarantined_at,
            expires_at: e.entry.expires_at,
            state: if e.restored.is_some() {
                "restored"
            } else if e.purged.is_some() {
                "purged"
            } else {
                "held"
            }
            .to_owned(),
            restored_to: e.restored.as_ref().map(|(_, p)| p.clone()),
            purged_at: e.purged.map(|(t, _)| t),
        })
        .collect();
    let restore_hint = quarantine
        .iter()
        .any(|q| q.state == "held")
        .then(|| format!("To put the quarantined items back: scull restore {op}"));
    Ok(OperationDetail {
        operation: summary,
        content_hash: r.content_hash,
        chain: ChainLine {
            intact: chain.is_intact(),
            rows: chain.rows,
            finished: chain.finished,
            problems: chain.problems.iter().map(|p| format!("{p:?}")).collect(),
        },
        items,
        quarantine,
        restore_hint,
    })
}

fn when(t: i64) -> String {
    super::clean::utc_datetime(t)
}

fn render_list(r: &HistoryReport, color: bool) -> String {
    if r.operations.is_empty() {
        return "No cleanup operations recorded yet.\n".into();
    }
    let mut t = Table::new([
        ("Operation", Align::Left),
        ("Started", Align::Left),
        ("Status", Align::Left),
        ("Items", Align::Right),
        ("Freed", Align::Right),
        ("Moved", Align::Right),
        ("Restorable", Align::Right),
        ("Audit", Align::Left),
    ]);
    for o in &r.operations {
        t.push(vec![
            Cell::text(o.operation_id.as_str()),
            Cell::text(&when(o.started_at)),
            Cell::text(&o.status.replace('_', " ")),
            Cell::text(&format!(
                "{}/{}",
                o.items_done,
                o.items_planned.max(o.items_recorded)
            )),
            Cell::bytes(o.freed_bytes),
            Cell::bytes(o.moved_bytes),
            Cell::text(&o.quarantine_held.to_string()),
            if o.chain_intact {
                Cell::text("intact")
            } else {
                Cell::text("BROKEN").styled(Style::Warn)
            },
        ]);
    }
    let mut out = t.render(color);
    out.push_str("\nDetails: scull history <operation>. Restore quarantined items: scull restore <operation>.\n");
    out
}

fn render_detail(d: &OperationDetail, color: bool) -> String {
    let o = &d.operation;
    let mut out = format!(
        "{}\n",
        Style::Bold.paint(&format!("Operation {}", o.operation_id), color)
    );
    let kv = |k: &str, v: &str| format!("  {k:<12} {}\n", escape_for_terminal(v));
    out.push_str(&kv(
        "plan",
        &format!("{} (hash {})", o.plan_id, d.content_hash),
    ));
    out.push_str(&kv(
        "approved by",
        &format!("{} ({})", o.approver, o.method),
    ));
    out.push_str(&kv(
        "ran",
        &format!(
            "{} -> {}",
            when(o.started_at),
            o.finished_at.map_or("not finished".to_owned(), when)
        ),
    ));
    out.push_str(&kv("status", &o.status.replace('_', " ")));
    out.push_str(&kv(
        "items",
        &format!(
            "{} planned: {} done, {} with errors, {} skipped",
            o.items_planned, o.items_done, o.items_with_errors, o.items_skipped
        ),
    ));
    out.push_str(&kv(
        "bytes",
        &format!(
            "predicted {}, freed {}, moved {}, measured {}",
            format_bytes(o.predicted_bytes),
            format_bytes(o.freed_bytes),
            format_bytes(o.moved_bytes),
            o.measured_delta_bytes
                .map_or("not recorded".to_owned(), |m| format!("{m:+} B"))
        ),
    ));
    let chain = if d.chain.intact {
        format!("intact ({} rows, hash-chained)", d.chain.rows)
    } else {
        format!("BROKEN: {}", d.chain.problems.join("; "))
    };
    out.push_str(&if d.chain.intact {
        kv("audit", &chain)
    } else {
        Style::Warn.paint(&kv("audit", &chain), color)
    });

    out.push_str(&format!("\n{}\n", Style::Bold.paint("Items:", color)));
    let mut t = Table::new([
        ("#", Align::Right),
        ("Outcome", Align::Left),
        ("Action", Align::Left),
        ("Freed", Align::Right),
        ("Moved", Align::Right),
        ("Path", Align::Left),
        ("Reason", Align::Left),
    ]);
    for i in &d.items {
        t.push(vec![
            Cell::text(&i.op_seq.to_string()),
            Cell::text(&i.outcome),
            Cell::text(&i.action),
            Cell::bytes(i.freed_bytes.unwrap_or(0)),
            Cell::bytes(i.moved_bytes.unwrap_or(0)),
            Cell::path(i.path.as_str()),
            Cell::text(i.reason.as_deref().unwrap_or("")),
        ]);
    }
    out.push_str(&t.render(color));
    if !d.quarantine.is_empty() {
        out.push_str(&format!("\n{}\n", Style::Bold.paint("Quarantine:", color)));
        let mut t = Table::new([
            ("Entry", Align::Right),
            ("State", Align::Left),
            ("Held", Align::Right),
            ("Until", Align::Left),
            ("Original path", Align::Left),
            ("Now", Align::Left),
        ]);
        for q in &d.quarantine {
            let now = match q.state.as_str() {
                "restored" => q
                    .restored_to
                    .as_ref()
                    .map_or(String::new(), |p| format!("restored to {}", p.as_str())),
                "purged" => format!("purged {}", q.purged_at.map_or(String::new(), when)),
                _ => q.quarantine_path.as_str().to_owned(),
            };
            t.push(vec![
                Cell::text(&q.entry_id.to_string()),
                Cell::text(&q.state),
                Cell::bytes(q.bytes),
                Cell::text(&q.expires_at.map_or("-".to_owned(), when)),
                Cell::path(q.original_path.as_str()),
                Cell::path(&now),
            ]);
        }
        out.push_str(&t.render(color));
    }
    if let Some(h) = &d.restore_hint {
        out.push_str(&format!("\n{}\n", escape_for_terminal(h)));
    }
    out
}
