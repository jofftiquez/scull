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
//! * Below the operations (without an ID): what was refused to AI agents (PRD §13.3 step 5),
//!   newest first: denied approvals (`deny`, too many wrong codes, no terminal, a refused
//!   attempt), refused `execute_cleanup` and `prepare_cleanup` calls (#299: rate limit,
//!   pending cap), approvals the human revoked and refused revocations (#298), and agent
//!   plans the human approved and ran with `scull approve --run` (with how that ended), each with its
//!   reason code, plan, channel, who recorded it and how many times it repeated
//!   (`agent_events` in JSON). Bounded: at most `--limit` entries (default
//!   [`AGENT_EVENTS_LIMIT`]) from `--since` or, by default, the last [`AGENT_EVENTS_DAYS`]
//!   days; how many more there are is reported (`agent_events_more`).
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
use scull_storage::{AgentAuditEvent, OperationRecord, QuarantineEntry, Store};
use serde::Serialize;

/// Default number of agent audit entries `scull history` lists.
pub const AGENT_EVENTS_LIMIT: u32 = 50;

/// Without `--since`, agent audit entries come from this many recent days.
pub const AGENT_EVENTS_DAYS: i64 = 30;

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
    /// What was refused to AI agents (denied approvals, refused executions), newest first.
    #[serde(default)]
    pub agent_events: Vec<AgentEventLine>,
    /// Agent audit entries in the same period not listed (beyond `--limit`).
    #[serde(default)]
    pub agent_events_more: u64,
}

/// One audited refusal in the agent cleanup flow (PRD §13.3 step 5).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentEventLine {
    /// When (Unix seconds).
    pub at: i64,
    /// `approval_denied`, `execute_refused`, `approval_revoked`, `revoke_refused` or
    /// `prepare_refused`.
    pub kind: String,
    /// Stable reason code (`denied`, `wrong_code`, `no_terminal`, `not_approved`,
    /// `approval_used`, `plan_changed`, ...).
    pub reason: String,
    /// The plan, when the refusal was about one.
    pub plan_id: Option<PlanId>,
    /// Its content hash, when known.
    pub content_hash: Option<ContentHash>,
    /// The channel the attempt came through: `terminal`, `desktop` or `mcp`.
    pub channel: String,
    /// What recorded it: `cli`, `desktop`, `mcp` or `executor`.
    pub source: String,
    /// The OS account.
    pub user: String,
    /// This installation's machine ID.
    pub machine_id: String,
    /// How many times it happened (repeats within a minute are folded into one entry).
    #[serde(default = "one")]
    pub count: u64,
}

fn one() -> u64 {
    1
}

impl AgentEventLine {
    fn of(e: &AgentAuditEvent) -> Self {
        Self {
            at: e.at,
            kind: e.kind.as_str().to_owned(),
            reason: e.reason.clone(),
            plan_id: e.plan_id.clone(),
            content_hash: e.content_hash.clone(),
            channel: e.channel.clone(),
            source: e.source.clone(),
            user: e.user.clone(),
            machine_id: e.machine_id.clone(),
            count: e.count,
        }
    }
}

/// The most recent `limit` agent audit events at or after `since`, newest first, and how
/// many more there are in that period.
pub fn agent_events(
    store: &Store,
    since: i64,
    limit: u32,
) -> Result<(Vec<AgentEventLine>, u64), ScullError> {
    let mut events = store.agent_events(since, limit)?;
    events.reverse();
    let rows = store.agent_event_rows(since)?;
    let more = rows.saturating_sub(events.len() as u64);
    Ok((events.iter().map(AgentEventLine::of).collect(), more))
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
    /// How (`cli_flag`, `cli_interactive`, `cli_tty`, `desktop`).
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
            let (operations, (agent_events, agent_events_more)) = if missing {
                (Vec::new(), (Vec::new(), 0))
            } else {
                let store = exec::open_store()?;
                let now = scull_engine::now_unix();
                let since = args.since.map_or(0, |s| {
                    now.saturating_sub(i64::try_from(s).unwrap_or(i64::MAX))
                });
                let agent_since = if args.since.is_some() {
                    since
                } else {
                    now.saturating_sub(AGENT_EVENTS_DAYS * 24 * 3600)
                };
                (
                    list(&store, since)?,
                    agent_events(&store, agent_since, args.limit)?,
                )
            };
            let r = HistoryReport {
                operations,
                agent_events,
                agent_events_more,
            };
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
    let mut out = render_operations(r, color);
    if !r.agent_events.is_empty() {
        out.push_str(&render_agent_events(&r.agent_events, color));
    }
    if r.agent_events_more > 0 {
        out.push_str(&format!(
            "... and {} more (scull history --limit N, or --since AGE).\n",
            r.agent_events_more
        ));
    }
    out
}

fn render_agent_events(events: &[AgentEventLine], color: bool) -> String {
    let mut out = format!(
        "\n{}\n",
        Style::Bold.paint(
            &format!("Refused to AI agents ({}, newest first):", events.len()),
            color
        )
    );
    let mut t = Table::new([
        ("When", Align::Left),
        ("What", Align::Left),
        ("Reason", Align::Left),
        ("Plan", Align::Left),
        ("Channel", Align::Left),
        ("Recorded by", Align::Left),
        ("Times", Align::Right),
    ]);
    for e in events {
        let what = match e.kind.as_str() {
            "approval_denied" => "approval denied",
            "execute_refused" => "execution refused",
            "approval_revoked" => "approval revoked",
            "revoke_refused" => "revoke refused",
            "prepare_refused" => "plan refused",
            "approve_run" => "approved, run here",
            other => other,
        };
        t.push(vec![
            Cell::text(&when(e.at)),
            Cell::text(what),
            Cell::text(&e.reason),
            Cell::text(e.plan_id.as_ref().map_or("-", PlanId::as_str)),
            Cell::text(&e.channel),
            Cell::text(&e.source),
            Cell::text(&e.count.to_string()),
        ]);
    }
    out.push_str(&t.render(color));
    out
}

fn render_operations(r: &HistoryReport, color: bool) -> String {
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
