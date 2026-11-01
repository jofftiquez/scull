//! The `events` stream (INS-06 "Alerts and responses"): alerts raised by `scull check` and
//! other timestamped facts that are not scans, plans or audit rows.
//!
//! Each row is `(occurred_at, kind, source, payload_json)`. The payload is the caller's JSON
//! object; this crate stores it as given and only requires it to be an object. It must
//! follow INS-07 (paths, categories, sizes, counts and times; never file contents,
//! environment values or secrets).
//!
//! Alerts ([`EventKind::Alert`]) carry their stable key next to the payload so repeated
//! checks can be de-duplicated and responses recorded against them later. Events are not
//! append-only: retention (DATA-06) may thin them.
//!
//! # Agent audit events (PRD §13.3 step 5)
//!
//! "Every agent request, approval, denial and result is audited." Plans, approvals and
//! operations have their own append-only tables; what an agent was **refused** is recorded
//! here as an [`AgentAuditEvent`]: a human denying an approval, a refused `execute_cleanup`,
//! a human revoking an approval before it was used, and a `prepare_cleanup` refused by the
//! agent limits ([`EventKind::ApprovalDenied`], [`EventKind::ExecuteRefused`],
//! [`EventKind::ApprovalRevoked`], [`EventKind::PrepareRefused`]; a revocation is recorded
//! in the same transaction that spends the approval, [`Store::revoke_approval`]). Each
//! carries only stable codes and identifiers (reason, plan ID, plan hash, channel, machine
//! and user), never free text, so it can never hold a confirmation code or a secret
//! ([`Store::record_agent_event`] validates every field). Read them with
//! [`Store::agent_events`] (`scull history`) and count them with [`Store::event_counts`]
//! (`scull data show`). Nothing here creates, changes or spends an approval.
//!
//! **Bounded**: a looping agent must not be able to grow the database through its refusals.
//! A refusal that repeats one recorded in the last [`AGENT_EVENT_MERGE_SECS`] (same kind,
//! source, reason, channel, machine and user, and the same recorded plan) is folded into that
//! row, whose [`AgentAuditEvent::count`] goes up. Refusals naming plans that are not recorded
//! (made-up IDs) fold together regardless of the ID, so varying it doesn't help either. Rows
//! therefore grow with the plans that exist (themselves capped) and with time, not with calls.

use crate::error::{Result, StoreError};
use crate::store::{Store, internal};
use rusqlite::{OptionalExtension, params};
use scull_core::{ContentHash, ErrorKind, FsPath, PlanId, ScanId};

/// What an event is.
///
/// Append-only: variants and their stored names are never renamed or removed, only added
/// (the `kind` column has no CHECK constraint, so a new variant needs no migration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EventKind {
    /// An alert raised by a check (PRD §16).
    Alert,
    /// A reclaim plan prepared for review (PRO-05); never executed by itself.
    ReviewPlan,
    /// An agent plan was not approved: the human denied it, typed the code wrong too often,
    /// or the approval attempt was refused (no terminal, elevated, expired plan, ...). An
    /// [`AgentAuditEvent`].
    ApprovalDenied,
    /// An `execute_cleanup` of an agent plan was refused (no approval, used, expired, void
    /// under D-34, refused by the Executor's gate, ...). An [`AgentAuditEvent`].
    ExecuteRefused,
    /// A human revoked an agent plan's approval before it was used
    /// ([`Store::revoke_approval`], the only way to record one). An [`AgentAuditEvent`].
    ApprovalRevoked,
    /// An attempt to revoke an approval was refused (no approval, already used or revoked,
    /// expired, another account's plan, lost the race to the execution). An
    /// [`AgentAuditEvent`].
    RevokeRefused,
    /// The emergency headroom file (PRO-06) was created, released or refused (audit trail of
    /// `scull_cleaner::headroom`).
    Headroom,
    /// A `prepare_cleanup` was refused by the agent limits (`rate_limited`,
    /// `too_many_pending_plans`; #299). An [`AgentAuditEvent`] without a plan.
    PrepareRefused,
    /// Recorded trends were forgotten (`scull insights purge`): written in the same
    /// transaction as the deletion by [`Store::purge_insights_recorded`], with the cutoff and
    /// the rows deleted per stream. Never deleted by the purge itself.
    InsightsPurged,
    /// The MCP client an agent plan was prepared through, as the client named itself in
    /// `initialize` (#78; sanitized, length-capped: [`AgentClient`]). Recorded in the same
    /// transaction as the plan by [`Store::insert_agent_plan`]; shown to the human who
    /// approves. Display only: it is never trusted for anything.
    AgentPlanClient,
}

impl EventKind {
    /// Every kind, in declaration order.
    pub const ALL: [EventKind; 10] = [
        EventKind::Alert,
        EventKind::ReviewPlan,
        EventKind::ApprovalDenied,
        EventKind::ExecuteRefused,
        EventKind::ApprovalRevoked,
        EventKind::RevokeRefused,
        EventKind::Headroom,
        EventKind::PrepareRefused,
        EventKind::InsightsPurged,
        EventKind::AgentPlanClient,
    ];

    /// The kinds recorded as [`AgentAuditEvent`]s.
    pub const AGENT: [EventKind; 5] = [
        EventKind::ApprovalDenied,
        EventKind::ExecuteRefused,
        EventKind::ApprovalRevoked,
        EventKind::RevokeRefused,
        EventKind::PrepareRefused,
    ];

    /// The stored name: `alert`, `review_plan`, `approval_denied`, `execute_refused`,
    /// `approval_revoked`, `revoke_refused`, `headroom`, `prepare_refused`, `insights_purged`,
    /// `agent_plan_client`.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Alert => "alert",
            EventKind::ReviewPlan => "review_plan",
            EventKind::ApprovalDenied => "approval_denied",
            EventKind::ExecuteRefused => "execute_refused",
            EventKind::ApprovalRevoked => "approval_revoked",
            EventKind::RevokeRefused => "revoke_refused",
            EventKind::Headroom => "headroom",
            EventKind::PrepareRefused => "prepare_refused",
            EventKind::InsightsPurged => "insights_purged",
            EventKind::AgentPlanClient => "agent_plan_client",
        }
    }

    /// The kind stored as `s`, if this build knows it.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Whether events of this kind are [`AgentAuditEvent`]s.
    pub fn is_agent(self) -> bool {
        Self::AGENT.contains(&self)
    }
}

/// An audited refusal in the agent cleanup flow (PRD §13.3 step 5; see the module docs).
///
/// Every field is a stable code or identifier: there is deliberately no message field, so a
/// confirmation code, a path or any other secret can't end up in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAuditEvent {
    /// Row id (assigned by the store; ignored when recording).
    pub event_id: i64,
    /// When it happened (Unix seconds).
    pub at: i64,
    /// One of [`EventKind::AGENT`].
    pub kind: EventKind,
    /// What recorded it: `cli` (`scull approve`), `desktop`, `mcp` (the MCP server) or
    /// `executor` (the Executor's own gate).
    pub source: String,
    /// The plan, when the refusal is about one.
    pub plan_id: Option<PlanId>,
    /// The plan's content hash, when known.
    pub content_hash: Option<ContentHash>,
    /// Stable reason code (`denied`, `wrong_code`, `not_approved`, `approval_used`, ...):
    /// lowercase ASCII letters, digits and `_`, 1 to 64 characters.
    pub reason: String,
    /// The channel the attempt came through: `terminal`, `desktop` or `mcp` (same alphabet
    /// as `reason`).
    pub channel: String,
    /// This installation's machine ID.
    pub machine_id: String,
    /// The OS account.
    pub user: String,
    /// How many occurrences this row stands for (repeats within [`AGENT_EVENT_MERGE_SECS`]
    /// are folded into it; `at` is the first). Ignored when recording.
    pub count: u64,
}

/// Window in which a repeated agent refusal is folded into the row already recorded
/// (see the module docs).
pub const AGENT_EVENT_MERGE_SECS: i64 = 60;

/// [`EventKind::ApprovalDenied`] reasons that are a human's final "no" (#78): `denied` (typed
/// `deny`, an empty answer, the desktop Deny button) and `wrong_code` (the code typed wrong
/// too often). Recorded only by [`Store::deny_plan`]; once one is recorded for a plan, the
/// plan can never be approved. Refused attempts (`no_terminal`, an agent-driven terminal,
/// an expired plan, ...) are not denials.
pub const FINAL_DENIAL_REASONS: [&str; 2] = ["denied", "wrong_code"];

/// Sources of a human denial: `scull approve` (`cli`) and the desktop dialog (`desktop`).
pub const HUMAN_DENIAL_SOURCES: [&str; 2] = ["cli", "desktop"];

/// SQL condition (on an `events` row aliased `e`) that the row is a human's final denial of
/// the plan whose ID is the SQL expression `plan`.
pub(crate) fn final_denial_sql(plan: &str) -> String {
    format!(
        "e.kind = 'approval_denied' AND e.source IN ('cli', 'desktop') \
         AND json_extract(e.payload_json, '$.plan_id') = {plan} \
         AND json_extract(e.payload_json, '$.reason') IN ('denied', 'wrong_code')"
    )
}

/// A human's final denial of an agent plan ([`Store::plan_denial`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDenial {
    /// When it was first denied (Unix seconds).
    pub at: i64,
    /// One of [`FINAL_DENIAL_REASONS`].
    pub reason: String,
    /// Where: `terminal` or `desktop`.
    pub channel: String,
}

/// Longest [`AgentClient::name`] kept.
pub const MAX_CLIENT_NAME_CHARS: usize = 64;
/// Longest [`AgentClient::version`] kept.
pub const MAX_CLIENT_VERSION_CHARS: usize = 32;

/// The MCP client an agent plan came through, as the client **declared** itself in
/// `initialize` (`clientInfo.name` / `version`): unverified, display only (#78). Every
/// character outside ASCII letters, digits, space and `. _ - + / ( ) @ :` becomes `?`, runs
/// of spaces collapse, and the parts are capped at [`MAX_CLIENT_NAME_CHARS`] and
/// [`MAX_CLIENT_VERSION_CHARS`] characters, so nothing in it can reach a terminal or a
/// dialog as a control sequence or a homoglyph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentClient {
    /// The client's name.
    pub name: String,
    /// Its version, if it gave a usable one.
    pub version: Option<String>,
}

impl AgentClient {
    /// The sanitized client info, or `None` when the name is empty after sanitizing.
    pub fn new(name: &str, version: Option<&str>) -> Option<Self> {
        let name = sanitize_client_text(name, MAX_CLIENT_NAME_CHARS)?;
        let version = version.and_then(|v| sanitize_client_text(v, MAX_CLIENT_VERSION_CHARS));
        Some(Self { name, version })
    }

    /// `name version` (or the name alone).
    pub fn display(&self) -> String {
        match &self.version {
            Some(v) => format!("{} {v}", self.name),
            None => self.name.clone(),
        }
    }

    fn is_sanitized(&self) -> bool {
        Self::new(&self.name, self.version.as_deref()).as_ref() == Some(self)
    }
}

fn sanitize_client_text(s: &str, max: usize) -> Option<String> {
    let mut out = String::new();
    for c in s.chars() {
        let c = if c.is_ascii_alphanumeric() || " ._-+/()@:".contains(c) {
            c
        } else {
            '?'
        };
        if c == ' ' && (out.is_empty() || out.ends_with(' ')) {
            continue;
        }
        if out.chars().count() == max {
            break;
        }
        out.push(c);
    }
    let out = out.trim_end().to_owned();
    (!out.is_empty() && out.chars().any(|c| c != '?')).then_some(out)
}

/// Longest `machine_id` or `user` an [`AgentAuditEvent`] may carry.
const MAX_PRINCIPAL_CHARS: usize = 256;

fn is_code(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl AgentAuditEvent {
    fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(StoreError::rejected(ErrorKind::Usage, m));
        if !self.kind.is_agent() {
            return bad(format!(
                "{} is not an agent audit event kind",
                self.kind.as_str()
            ));
        }
        for (field, v, max) in [
            ("source", &self.source, 32),
            ("reason", &self.reason, 64),
            ("channel", &self.channel, 32),
        ] {
            if !is_code(v, max) {
                return bad(format!(
                    "agent audit {field} must be a lowercase code of at most {max} characters"
                ));
            }
        }
        for (field, v) in [("machine_id", &self.machine_id), ("user", &self.user)] {
            if v.is_empty()
                || v.chars().count() > MAX_PRINCIPAL_CHARS
                || v.chars().any(char::is_control)
            {
                return bad(format!(
                    "agent audit {field} must be 1 to {MAX_PRINCIPAL_CHARS} printable characters"
                ));
            }
        }
        Ok(())
    }

    fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "plan_id": self.plan_id.as_ref().map(PlanId::as_str),
            "content_hash": self.content_hash.as_ref().map(ContentHash::as_str),
            "reason": self.reason,
            "channel": self.channel,
            "machine_id": self.machine_id,
            "user": self.user,
            "count": 1,
        })
    }

    fn from_event(e: Event) -> Result<Self> {
        let bad = || StoreError::rejected(ErrorKind::Internal, "malformed agent audit event");
        let text = |k: &str| e.payload.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        let plan_id = text("plan_id")
            .map(|s| PlanId::try_from(s).map_err(|_| bad()))
            .transpose()?;
        let content_hash = text("content_hash")
            .map(|s| ContentHash::try_from(s).map_err(|_| bad()))
            .transpose()?;
        Ok(Self {
            event_id: e.event_id,
            at: e.at,
            kind: e.kind,
            source: e.source,
            plan_id,
            content_hash,
            reason: text("reason").ok_or_else(bad)?,
            channel: text("channel").ok_or_else(bad)?,
            machine_id: text("machine_id").ok_or_else(bad)?,
            user: text("user").ok_or_else(bad)?,
            count: e
                .payload
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1),
        })
    }

    pub(crate) fn to_event(&self) -> Result<Event> {
        self.validate()?;
        Ok(Event {
            event_id: 0,
            at: self.at,
            kind: self.kind,
            source: self.source.clone(),
            payload: self.payload(),
        })
    }
}

/// The agent kinds as an SQL list of string literals (stored names are fixed ASCII codes).
fn agent_kinds_sql() -> String {
    EventKind::AGENT
        .iter()
        .map(|k| format!("'{}'", k.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Rows of one event kind, as `scull data show` reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventCount {
    /// The stored kind name (also kinds this build does not know).
    pub kind: String,
    /// Number of rows.
    pub rows: u64,
}

/// One recorded event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Row id (assigned by the store; ignored when recording).
    pub event_id: i64,
    /// When it happened (Unix seconds).
    pub at: i64,
    /// What it is.
    pub kind: EventKind,
    /// Which interface recorded it: `check`, `cli`, `desktop`, `mcp`.
    pub source: String,
    /// The caller's JSON object.
    pub payload: serde_json::Value,
}

/// An alert as recorded: its stable key (`<kind>:<subject>`) and the full alert as JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertEvent {
    /// Row id (assigned by the store; ignored when recording).
    pub event_id: i64,
    /// When it was raised (Unix seconds).
    pub at: i64,
    /// Which interface raised it.
    pub source: String,
    /// The alert's stable key.
    pub key: String,
    /// The alert itself (a JSON object).
    pub alert: serde_json::Value,
}

fn require_object(v: &serde_json::Value) -> Result<()> {
    if v.is_object() {
        Ok(())
    } else {
        Err(StoreError::rejected(
            ErrorKind::Usage,
            "an event payload must be a JSON object",
        ))
    }
}

impl Store {
    /// The roots of scan `scan_id` as recorded (in the order given at its start), or `None`
    /// when there is no such scan.
    pub fn scan_roots(&self, scan_id: &ScanId) -> Result<Option<Vec<FsPath>>> {
        self.read(|conn| {
            conn.query_row(
                "SELECT roots_json FROM scans WHERE scan_id = ?1",
                [scan_id.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str(&json).map_err(internal))
            .transpose()
        })
    }

    /// Record events in one transaction. `event_id` is ignored.
    pub fn record_events(&mut self, events: &[Event]) -> Result<()> {
        for e in events {
            require_object(&e.payload)?;
        }
        self.write(|tx| {
            let mut stmt = tx.prepare(
                "INSERT INTO events (occurred_at, kind, source, payload_json) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in events {
                let json = serde_json::to_string(&e.payload).map_err(internal)?;
                stmt.execute(params![e.at, e.kind.as_str(), e.source, json])?;
            }
            Ok(())
        })
    }

    /// Events of `kind` that happened at or after `since`, oldest first.
    pub fn events(&self, kind: EventKind, since: i64) -> Result<Vec<Event>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT event_id, occurred_at, source, payload_json FROM events \
                 WHERE kind = ?1 AND occurred_at >= ?2 ORDER BY occurred_at, event_id",
            )?;
            let rows = stmt.query_map(params![kind.as_str(), since], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            rows.map(|row| {
                let (event_id, at, source, json) = row?;
                Ok(Event {
                    event_id,
                    at,
                    kind,
                    source,
                    payload: serde_json::from_str(&json).map_err(internal)?,
                })
            })
            .collect()
        })
    }

    /// The most recent event of `kind` (latest time, then latest row), if any.
    pub fn latest_event(&self, kind: EventKind) -> Result<Option<Event>> {
        self.read(|conn| {
            conn.query_row(
                "SELECT event_id, occurred_at, source, payload_json FROM events \
                 WHERE kind = ?1 ORDER BY occurred_at DESC, event_id DESC LIMIT 1",
                [kind.as_str()],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .map(|(event_id, at, source, json)| {
                Ok(Event {
                    event_id,
                    at,
                    kind,
                    source,
                    payload: serde_json::from_str(&json).map_err(internal)?,
                })
            })
            .transpose()
        })
    }

    /// Record one agent audit event (PRD §13.3 step 5), or fold it into the matching row
    /// recorded in the last [`AGENT_EVENT_MERGE_SECS`] (see the module docs); one transaction.
    /// Rejected with [`ErrorKind::Usage`] if its kind is not one of [`EventKind::AGENT`] or a
    /// field is not a code (see [`AgentAuditEvent`]). `event_id` and `count` are ignored.
    ///
    /// [`EventKind::ApprovalRevoked`] is refused here: a revocation is recorded only by
    /// [`Store::revoke_approval`], together with spending the approval, so a forged
    /// "revoked" entry can't make an approval look withdrawn.
    pub fn record_agent_event(&mut self, e: &AgentAuditEvent) -> Result<()> {
        if e.kind == EventKind::ApprovalRevoked {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                "approval_revoked is recorded only by Store::revoke_approval",
            ));
        }
        if is_final_denial(e) {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                "a human's denial is recorded only by Store::deny_plan",
            ));
        }
        let event = e.to_event()?;
        self.write(|tx| insert_agent_event(tx, e, &event))
    }

    /// Record a human's final denial of an agent plan (#78): `e` is an
    /// [`EventKind::ApprovalDenied`] with one of [`FINAL_DENIAL_REASONS`], from one of
    /// [`HUMAN_DENIAL_SOURCES`], naming the plan. **In one transaction** with the check for an
    /// approval: [`Store::insert_approval`] refuses a denied plan in its own transaction, so a
    /// denial and a concurrent approval can't both win. Returns `true` when the plan is now
    /// denied, `false` when it already had an approval (the attempt is still audited, but
    /// the approval stands). A repeated denial folds into the earlier row (module docs).
    pub fn deny_plan(&mut self, e: &AgentAuditEvent) -> Result<bool> {
        if !is_final_denial(e) || e.plan_id.is_none() {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                "deny_plan records a human's approval_denied (denied or wrong_code) of a plan",
            ));
        }
        let event = e.to_event()?;
        let plan = e.plan_id.as_ref().map(PlanId::as_str);
        self.write(|tx| {
            let approved: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM approvals WHERE plan_id = ?1)",
                [plan],
                |r| r.get(0),
            )?;
            insert_agent_event(tx, e, &event)?;
            Ok(!approved)
        })
    }

    /// The human's final denial of plan `plan_id`, if one was recorded ([`Store::deny_plan`];
    /// the first, when there are several).
    pub fn plan_denial(&self, plan_id: &PlanId) -> Result<Option<PlanDenial>> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT e.occurred_at, json_extract(e.payload_json, '$.reason'), \
                         json_extract(e.payload_json, '$.channel') FROM events e \
                         WHERE {} ORDER BY e.occurred_at, e.event_id LIMIT 1",
                        final_denial_sql("?1")
                    ),
                    [plan_id.as_str()],
                    |r| {
                        Ok(PlanDenial {
                            at: r.get(0)?,
                            reason: r.get(1)?,
                            channel: r.get(2)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    /// The MCP client plan `plan_id` was prepared through, as it declared itself
    /// ([`EventKind::AgentPlanClient`]), if one was recorded.
    pub fn plan_client(&self, plan_id: &PlanId) -> Result<Option<AgentClient>> {
        let row: Option<(String, Option<String>)> = self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT json_extract(payload_json, '$.name'), \
                     json_extract(payload_json, '$.version') FROM events \
                     WHERE kind = ?1 AND json_extract(payload_json, '$.plan_id') = ?2 \
                     ORDER BY event_id LIMIT 1",
                    params![EventKind::AgentPlanClient.as_str(), plan_id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?)
        })?;
        // Re-sanitized on the way out: the database is not trusted for display either.
        Ok(row.and_then(|(name, version)| AgentClient::new(&name, version.as_deref())))
    }

    /// The most recent `limit` agent audit events at or after `since` (every
    /// [`EventKind::AGENT`] kind), returned oldest first. [`Store::agent_event_rows`] says how
    /// many there are in all.
    pub fn agent_events(&self, since: i64, limit: u32) -> Result<Vec<AgentAuditEvent>> {
        let kinds = agent_kinds_sql();
        let rows: Vec<(i64, i64, String, String, String)> = self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT event_id, occurred_at, kind, source, payload_json FROM events \
                 WHERE kind IN ({kinds}) AND occurred_at >= ?1 \
                 ORDER BY occurred_at DESC, event_id DESC LIMIT ?2"
            ))?;
            let rows = stmt.query_map(params![since, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })?;
        let mut out = rows
            .into_iter()
            .map(|(event_id, at, kind, source, json)| {
                let kind = EventKind::parse(&kind).ok_or_else(|| {
                    StoreError::rejected(ErrorKind::Internal, "unknown agent event kind")
                })?;
                AgentAuditEvent::from_event(Event {
                    event_id,
                    at,
                    kind,
                    source,
                    payload: serde_json::from_str(&json).map_err(internal)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        out.reverse();
        Ok(out)
    }

    /// How many agent audit event rows there are at or after `since`.
    pub fn agent_event_rows(&self, since: i64) -> Result<u64> {
        let kinds = agent_kinds_sql();
        self.read(|conn| {
            let n: i64 = conn.query_row(
                &format!(
                    "SELECT count(*) FROM events WHERE kind IN ({kinds}) AND occurred_at >= ?1"
                ),
                [since],
                |r| r.get(0),
            )?;
            Ok(u64::try_from(n).unwrap_or(0))
        })
    }

    /// Revoke the approval of `plan_id` (#298): **in one transaction**, mark it used at `now`
    /// (the same single-use marker the Executor's atomic consumption sets, so a revocation and
    /// a concurrent `Executor::begin_operation` can't both win: exactly one update changes the
    /// row) and record `audit` ([`EventKind::ApprovalRevoked`]). Returns `true` only if a
    /// matching approval (hash `hash`) existed that was unused, already given and unexpired
    /// at `now`; otherwise nothing is written and it returns `false` (it was used, expired,
    /// bound to other content, or never given). Never creates an approval.
    pub fn revoke_approval(
        &mut self,
        plan_id: &PlanId,
        hash: &ContentHash,
        now: i64,
        audit: &AgentAuditEvent,
    ) -> Result<bool> {
        if audit.kind != EventKind::ApprovalRevoked || audit.plan_id.as_ref() != Some(plan_id) {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                "a revocation is audited as approval_revoked for the same plan",
            ));
        }
        let event = audit.to_event()?;
        let json = serde_json::to_string(&event.payload).map_err(internal)?;
        self.write(|tx| {
            let changed = tx.execute(
                "UPDATE approvals SET used_at = ?3 \
                 WHERE plan_id = ?1 AND content_hash = ?2 AND used_at IS NULL \
                   AND approved_at <= ?3 AND ?3 < expires_at",
                params![plan_id.as_str(), hash.as_str(), now],
            )?;
            if changed != 1 {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO events (occurred_at, kind, source, payload_json) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![event.at, event.kind.as_str(), event.source, json],
            )?;
            Ok(true)
        })
    }

    /// Whether the approval of `plan_id` was revoked ([`Store::revoke_approval`]).
    pub fn approval_revoked(&self, plan_id: &PlanId) -> Result<bool> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM events WHERE kind = ?1 \
                 AND json_extract(payload_json, '$.plan_id') = ?2)",
                params![EventKind::ApprovalRevoked.as_str(), plan_id.as_str()],
                |r| r.get(0),
            )?)
        })
    }

    /// Rows per event kind, by kind name.
    pub fn event_counts(&self) -> Result<Vec<EventCount>> {
        self.read(|conn| {
            let mut stmt =
                conn.prepare("SELECT kind, count(*) FROM events GROUP BY kind ORDER BY kind")?;
            let rows = stmt.query_map([], |r| {
                Ok(EventCount {
                    kind: r.get(0)?,
                    rows: u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Record one alert (see [`Store::record_alerts`]).
    pub fn record_alert(&mut self, alert: &AlertEvent) -> Result<()> {
        self.record_alerts(std::slice::from_ref(alert))
    }

    /// Record alerts as [`EventKind::Alert`] events, in one transaction. Each payload is
    /// `{"key": <key>, "alert": <alert>}`; `alert` must be a JSON object.
    pub fn record_alerts(&mut self, alerts: &[AlertEvent]) -> Result<()> {
        let events: Vec<Event> = alerts
            .iter()
            .map(|a| Event {
                event_id: 0,
                at: a.at,
                kind: EventKind::Alert,
                source: a.source.clone(),
                payload: serde_json::json!({ "key": a.key, "alert": a.alert }),
            })
            .collect();
        for a in alerts {
            require_object(&a.alert)?;
        }
        self.record_events(&events)
    }

    /// Alerts raised at or after `since`, oldest first.
    pub fn alerts(&self, since: i64) -> Result<Vec<AlertEvent>> {
        self.events(EventKind::Alert, since)?
            .into_iter()
            .map(|e| {
                let key = e
                    .payload
                    .get("key")
                    .and_then(|k| k.as_str())
                    .ok_or_else(|| {
                        StoreError::rejected(ErrorKind::Internal, "alert event without a key")
                    })?;
                Ok(AlertEvent {
                    event_id: e.event_id,
                    at: e.at,
                    source: e.source.clone(),
                    key: key.to_owned(),
                    alert: e
                        .payload
                        .get("alert")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                })
            })
            .collect()
    }
}

fn is_final_denial(e: &AgentAuditEvent) -> bool {
    e.kind == EventKind::ApprovalDenied
        && FINAL_DENIAL_REASONS.contains(&e.reason.as_str())
        && HUMAN_DENIAL_SOURCES.contains(&e.source.as_str())
}

/// Insert agent audit event `e` (already validated as `event`), or fold it into the matching
/// row recorded in the last [`AGENT_EVENT_MERGE_SECS`] (module docs).
fn insert_agent_event(
    tx: &rusqlite::Transaction<'_>,
    e: &AgentAuditEvent,
    event: &Event,
) -> Result<()> {
    let json = serde_json::to_string(&event.payload).map_err(internal)?;
    let plan = e.plan_id.as_ref().map(PlanId::as_str);
    let known: bool = match plan {
        Some(p) => tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM plans WHERE plan_id = ?1)",
            [p],
            |r| r.get(0),
        )?,
        None => false,
    };
    let existing: Option<i64> = tx
        .query_row(
            "SELECT event_id FROM events WHERE kind = ?1 AND source = ?2 \
             AND occurred_at > ?3 - ?4 AND occurred_at <= ?3 \
             AND json_extract(payload_json, '$.reason') = ?5 \
             AND json_extract(payload_json, '$.channel') = ?6 \
             AND json_extract(payload_json, '$.machine_id') = ?7 \
             AND json_extract(payload_json, '$.user') = ?8 \
             AND CASE WHEN ?10 THEN json_extract(payload_json, '$.plan_id') = ?9 \
                 ELSE (json_extract(payload_json, '$.plan_id') IS NULL \
                       OR NOT EXISTS (SELECT 1 FROM plans \
                           WHERE plan_id = json_extract(payload_json, '$.plan_id'))) \
                 END \
             ORDER BY event_id DESC LIMIT 1",
            params![
                event.kind.as_str(),
                event.source,
                event.at,
                AGENT_EVENT_MERGE_SECS,
                e.reason,
                e.channel,
                e.machine_id,
                e.user,
                plan,
                known,
            ],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => tx.execute(
            "UPDATE events SET payload_json = json_set(payload_json, '$.count', \
             coalesce(json_extract(payload_json, '$.count'), 1) + 1) WHERE event_id = ?1",
            [id],
        )?,
        None => tx.execute(
            "INSERT INTO events (occurred_at, kind, source, payload_json) \
             VALUES (?1, ?2, ?3, ?4)",
            params![event.at, event.kind.as_str(), event.source, json],
        )?,
    };
    Ok(())
}

/// Record that agent plan `plan_id` came through `client` ([`EventKind::AgentPlanClient`]).
pub(crate) fn insert_plan_client(
    tx: &rusqlite::Transaction<'_>,
    plan_id: &PlanId,
    client: &AgentClient,
    at: i64,
) -> Result<()> {
    if !client.is_sanitized() {
        return Err(StoreError::rejected(
            ErrorKind::Usage,
            "the MCP client info must be sanitized (AgentClient::new)",
        ));
    }
    let payload = serde_json::json!({
        "plan_id": plan_id.as_str(),
        "name": client.name,
        "version": client.version,
    });
    tx.execute(
        "INSERT INTO events (occurred_at, kind, source, payload_json) VALUES (?1, ?2, 'mcp', ?3)",
        params![at, EventKind::AgentPlanClient.as_str(), payload.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultPermissions, ScanStart};
    use serde_json::json;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Store::open(&dir.path().join("scull.db"), &DefaultPermissions).expect("open");
        (dir, s)
    }

    #[test]
    fn scan_roots_round_trip_in_order() {
        let (_d, mut s) = store();
        let id = ScanId::from_suffix("r1").expect("id");
        let roots = vec![FsPath::from("/z/b"), FsPath::from("/a/c")];
        s.record_scan_start(&ScanStart {
            scan_id: id.clone(),
            started_at: 10,
            roots: roots.clone(),
        })
        .expect("start");
        assert_eq!(s.scan_roots(&id).expect("roots"), Some(roots));
        let other = ScanId::from_suffix("nope").expect("id");
        assert_eq!(s.scan_roots(&other).expect("roots"), None);
    }

    #[test]
    fn alerts_are_recorded_and_read_back_by_time() {
        let (_d, mut s) = store();
        s.record_alerts(&[
            AlertEvent {
                event_id: 0,
                at: 200,
                source: "check".into(),
                key: "stale:cache".into(),
                alert: json!({"kind": "stale", "bytes": 7}),
            },
            AlertEvent {
                event_id: 0,
                at: 100,
                source: "check".into(),
                key: "low_space:/".into(),
                alert: json!({"kind": "low_space"}),
            },
        ])
        .expect("record");
        s.record_alert(&AlertEvent {
            event_id: 0,
            at: 300,
            source: "cli".into(),
            key: "growth_spike:container".into(),
            alert: json!({"kind": "growth_spike"}),
        })
        .expect("record one");
        let all = s.alerts(0).expect("alerts");
        let keys: Vec<&str> = all.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["low_space:/", "stale:cache", "growth_spike:container"]
        );
        assert_eq!(all[1].alert["bytes"], 7);
        assert_eq!(all[2].source, "cli");
        assert!(all.iter().all(|a| a.event_id > 0));
        let recent = s.alerts(150).expect("since");
        assert_eq!(recent.len(), 2);
        // Alerts are not other events.
        assert!(
            s.events(EventKind::ReviewPlan, 0)
                .expect("events")
                .is_empty()
        );
    }

    #[test]
    fn non_object_payloads_are_rejected_and_latest_event_wins() {
        let (_d, mut s) = store();
        let bad = AlertEvent {
            event_id: 0,
            at: 1,
            source: "check".into(),
            key: "k".into(),
            alert: json!("text"),
        };
        assert!(s.record_alert(&bad).is_err());
        assert!(s.alerts(0).expect("alerts").is_empty());
        assert_eq!(s.latest_event(EventKind::ReviewPlan).expect("latest"), None);
        for (at, id) in [(5, "a"), (9, "b"), (9, "c"), (7, "d")] {
            s.record_events(&[Event {
                event_id: 0,
                at,
                kind: EventKind::ReviewPlan,
                source: "check".into(),
                payload: json!({ "plan_id": id }),
            }])
            .expect("record");
        }
        let latest = s
            .latest_event(EventKind::ReviewPlan)
            .expect("latest")
            .expect("some");
        assert_eq!(latest.payload["plan_id"], "c");
        assert_eq!(latest.at, 9);
        let since = s.events(EventKind::ReviewPlan, 7).expect("events");
        assert_eq!(since.len(), 3);
    }

    fn agent(kind: EventKind, at: i64, reason: &str) -> AgentAuditEvent {
        AgentAuditEvent {
            event_id: 0,
            at,
            kind,
            source: "mcp".into(),
            plan_id: Some(PlanId::from_suffix("01JAGENT").expect("id")),
            content_hash: Some(ContentHash::try_from("ab".repeat(32)).expect("hash")),
            reason: reason.into(),
            channel: "mcp".into(),
            machine_id: "machine-1".into(),
            user: "uid:1000".into(),
            count: 1,
        }
    }

    #[test]
    fn agent_events_round_trip_oldest_first_and_are_counted() {
        let (_d, mut s) = store();
        s.record_agent_event(&agent(EventKind::ExecuteRefused, 20, "not_approved"))
            .expect("record");
        let mut denied = agent(EventKind::ApprovalDenied, 10, "denied");
        denied.source = "cli".into();
        denied.channel = "terminal".into();
        denied.content_hash = None;
        // A final denial is recorded through deny_plan only (#78); the plan isn't recorded
        // here, so it had no approval either.
        assert!(s.record_agent_event(&denied).is_err());
        assert!(s.deny_plan(&denied).expect("record"));
        let mut no_plan = agent(EventKind::ExecuteRefused, 30, "invalid_arguments");
        no_plan.plan_id = None;
        no_plan.content_hash = None;
        s.record_agent_event(&no_plan).expect("record");
        s.record_alert(&AlertEvent {
            event_id: 0,
            at: 15,
            source: "check".into(),
            key: "k".into(),
            alert: json!({}),
        })
        .expect("alert");
        let all = s.agent_events(0, 100).expect("events");
        let got: Vec<(i64, &str)> = all.iter().map(|e| (e.at, e.reason.as_str())).collect();
        assert_eq!(
            got,
            vec![
                (10, "denied"),
                (20, "not_approved"),
                (30, "invalid_arguments")
            ]
        );
        assert_eq!(all[0].kind, EventKind::ApprovalDenied);
        assert_eq!(all[0].channel, "terminal");
        assert_eq!(all[0].content_hash, None);
        assert_eq!(all[1], {
            let mut e = agent(EventKind::ExecuteRefused, 20, "not_approved");
            e.event_id = all[1].event_id;
            e
        });
        assert_eq!(all[2].plan_id, None);
        assert_eq!(s.agent_events(20, 100).expect("since").len(), 2);
        let counts = s.event_counts().expect("counts");
        let n = |k: &str| counts.iter().find(|c| c.kind == k).map_or(0, |c| c.rows);
        assert_eq!(
            (n("alert"), n("approval_denied"), n("execute_refused")),
            (1, 1, 2)
        );
        for k in EventKind::ALL {
            assert_eq!(EventKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(EventKind::parse("nope"), None);
    }

    /// Only codes and identifiers are stored: free text (a confirmation code typed as a
    /// reason, a message with spaces, control characters) and non-agent kinds are refused,
    /// and nothing is recorded.
    #[test]
    fn agent_events_refuse_free_text_and_other_kinds() {
        let (_d, mut s) = store();
        let cases: Vec<AgentAuditEvent> = vec![
            agent(EventKind::Alert, 1, "denied"),
            agent(EventKind::ExecuteRefused, 1, "the code was ABC-DEF"),
            agent(EventKind::ExecuteRefused, 1, "WRONG_CODE"),
            agent(EventKind::ExecuteRefused, 1, ""),
            agent(EventKind::ExecuteRefused, 1, &"x".repeat(65)),
            {
                let mut e = agent(EventKind::ExecuteRefused, 1, "denied");
                e.channel = "Terminal".into();
                e
            },
            {
                let mut e = agent(EventKind::ExecuteRefused, 1, "denied");
                e.source = "".into();
                e
            },
            {
                let mut e = agent(EventKind::ExecuteRefused, 1, "denied");
                e.user = "me\u{1b}[2J".into();
                e
            },
            {
                let mut e = agent(EventKind::ExecuteRefused, 1, "denied");
                e.machine_id = String::new();
                e
            },
        ];
        for e in &cases {
            let err = s.record_agent_event(e).expect_err("refused");
            assert!(
                matches!(
                    err,
                    StoreError::Rejected {
                        kind: ErrorKind::Usage,
                        ..
                    }
                ),
                "{err:?}"
            );
        }
        assert!(s.agent_events(0, 100).expect("events").is_empty());
        assert!(s.event_counts().expect("counts").is_empty());
    }
}
