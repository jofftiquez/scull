//! The desktop approval dialog's backend (P4-MCP-02, PRD §13.3, D-7): the human approves, denies
//! or revokes an AI agent's cleanup plan in the app, through the approval broker
//! (`scull_mcp::approval`) with [`HumanChannel::Desktop`].
//!
//! **An AI tool call is not human consent.** What this module guarantees:
//!
//! * **The code stays here.** [`ApprovalDesk::open`] starts a broker challenge
//!   ([`approval::begin`]) and keeps it in this process's memory, keyed by plan ID. The webview
//!   receives the code **for display only** (it is shown next to the plan and typed back by the
//!   human); it is never written to disk, never logged (the broker's `ConfirmationCode` redacts
//!   itself in `Debug`, and no error here carries it) and never reaches the agent, which talks
//!   to a different process (`scull mcp serve`) that has no access to this memory.
//! * **The backend decides.** [`ApprovalDesk::approve`] compares the typed code with the
//!   challenge's in constant time ([`approval::approve`]); whatever the webview thinks about the
//!   code, nothing is approved without that match. A plan never opened in this app can't be
//!   approved here, and the hash the human reviewed must be the challenge's.
//! * **Bounded attempts.** [`MAX_CODE_ATTEMPTS`] wrong codes close the request and record a
//!   denial (`wrong_code`); like a denial, the agent then has to prepare a new plan.
//! * **Nothing is executed here.** The desktop only records the human's decision. The agent
//!   executes an approved plan through `scull mcp serve` and the Executor; this crate graph has
//!   no `scull-cleaner` (SAFE-10). [`ApprovalDesk::status`] reports what happened from the store.
//! * **Every decision is audited** by the broker: approvals in the approvals table, denials and
//!   refusals as agent audit events, revocations in the same transaction that spends the
//!   approval.
//!
//! A plan a human closed (denied, or too many wrong codes; in the app or in `scull approve`)
//! is no longer listed or opened here and reports [`AgentPlanState::Denied`]: the denial is
//! read back from the agent audit events.
//!
//! Without scull's database, or without this installation's machine ID, there are no agent
//! plans for this machine: [`ApprovalDesk::awaiting`] is empty and nothing is created.

use schemars::JsonSchema;
use scull_core::{
    Candidate, Category, ContentHash, ErrorKind, Flags, Plan, PlanId, PlanItem, Reclaimable,
    Regeneration, ScullError, Sizes,
};
use scull_engine::{ScanReport, history_db_path, open_history};
use scull_mcp::approval::{
    self, ApprovalState, Challenge, Denial, HumanChannel, MAX_CODE_ATTEMPTS, Principal, Reason,
    Refused,
};
use scull_storage::{EventKind, ItemOutcome, Store, StoreOptions};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// An agent plan waiting for the human, for the pending-requests notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRequest {
    /// Plan ID (open it with `open_approval_request`).
    pub plan_id: String,
    /// Content hash the approval will be bound to.
    pub content_hash: String,
    /// When the agent prepared it (Unix seconds).
    pub created_at: i64,
    /// When it expires unapproved (Unix seconds).
    pub expires_at: i64,
    /// Items in the plan.
    pub item_count: u64,
    /// Bytes the plan predicts it frees.
    pub reclaimable_bytes: u64,
}

/// Agent plans waiting for the human, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRequests {
    /// The requests (empty without a database or machine ID).
    pub requests: Vec<AgentRequest>,
}

/// One item of an agent plan as the human reviews it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanItem {
    /// The item. Its tier, action, path, identity and reclaimable bytes are always the plan's
    /// recorded ones (what the approval binds to).
    pub candidate: Candidate,
    /// `true`: evidence, reasons, flags and sizes come from this app's last scan, which found
    /// the same item (same ID, identity, path, tier, action and rule). `false`: only what the
    /// plan recorded is known; flags are unknown (never "no") and there is no evidence to show.
    pub from_last_scan: bool,
}

/// An agent plan opened for approval: the plan exactly as the broker verified it, and the
/// code to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentApprovalRequest {
    /// Plan ID.
    pub plan_id: String,
    /// Content hash (send it back with the approval).
    pub content_hash: String,
    /// When the agent prepared it (Unix seconds).
    pub created_at: i64,
    /// When it expires unapproved; an approval lapses at the same time (Unix seconds).
    pub expires_at: i64,
    /// Every item, in plan order.
    pub items: Vec<AgentPlanItem>,
    /// The confirmation code, `ABC-DEF`, **for display only**: the human reads it here and types
    /// it back. It is valid for this opening of the dialog only and never leaves this machine.
    pub code: String,
    /// Wrong codes still accepted before this opening closes.
    pub attempts_left: u32,
}

/// The human approved the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentApproved {
    /// Plan ID.
    pub plan_id: String,
    /// When (Unix seconds).
    pub approved_at: i64,
    /// When the unused approval lapses (Unix seconds).
    pub approval_expires_at: i64,
}

/// The human denied the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentDenied {
    /// Plan ID.
    pub plan_id: String,
    /// When (Unix seconds).
    pub denied_at: i64,
}

/// The human revoked the approval before the agent used it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentRevoked {
    /// Plan ID.
    pub plan_id: String,
    /// When (Unix seconds).
    pub revoked_at: i64,
}

/// Where an agent plan stands, as the desktop shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentPlanState {
    /// Waiting for the human.
    AwaitingApproval,
    /// A human denied it (or typed the code wrong too many times): nothing will run.
    Denied,
    /// Approved, not used yet.
    Approved,
    /// The agent is executing it (its operation started and hasn't finished).
    Running,
    /// The agent executed it; see `operation`.
    Executed,
    /// A target changed on disk before it ran, so the whole plan is void (D-34).
    Voided,
    /// The plan or its unused approval expired.
    Expired,
    /// The human revoked the approval before it was used.
    Revoked,
}

/// What happened to one item when the agent executed the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentItemOutcome {
    /// Deleted.
    Removed,
    /// Moved to quarantine (restorable).
    Quarantined,
    /// Moved to the system Trash.
    MovedToTrash,
    /// The ecosystem's own cleanup command ran.
    ToolRun,
    /// Only part of it was cleaned.
    Partial,
    /// Cleaning it failed.
    Failed,
    /// The re-check refused it.
    Refused,
    /// Skipped (changed since the plan, in use, ...).
    Skipped,
    /// An outcome a newer scull recorded that this build doesn't know.
    Unknown,
}

impl From<ItemOutcome> for AgentItemOutcome {
    fn from(o: ItemOutcome) -> Self {
        match o {
            ItemOutcome::Removed => Self::Removed,
            ItemOutcome::Quarantined => Self::Quarantined,
            ItemOutcome::MovedToTrash => Self::MovedToTrash,
            ItemOutcome::ToolRun => Self::ToolRun,
            ItemOutcome::Partial => Self::Partial,
            ItemOutcome::Failed => Self::Failed,
            ItemOutcome::Refused => Self::Refused,
            ItemOutcome::Skipped => Self::Skipped,
            _ => Self::Unknown,
        }
    }
}

/// One recorded item outcome of the agent's execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentItemResult {
    /// Plan item.
    pub item_id: String,
    /// What happened.
    pub outcome: AgentItemOutcome,
    /// Stable reason code (skipped, refused, failed).
    pub reason: Option<String>,
    /// Bytes freed.
    pub freed_bytes: u64,
    /// Bytes moved aside (quarantine, Trash).
    pub moved_bytes: u64,
}

/// The agent's execution of the plan, as recorded by the Executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentOperation {
    /// When it started (Unix seconds).
    pub started_at: i64,
    /// When it finished (Unix seconds); `None` while running.
    pub finished_at: Option<i64>,
    /// Item outcomes recorded so far, in order.
    pub items: Vec<AgentItemResult>,
    /// Bytes the plan predicted.
    pub predicted_bytes: u64,
    /// Bytes moved aside.
    pub moved_bytes: u64,
    /// Bytes freed (sum of the items).
    pub freed_bytes: u64,
    /// Free space after minus before, when measured.
    pub measured_delta_bytes: Option<i64>,
}

/// The status of an agent plan. Contains no code and nothing that can approve anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentPlanStatus {
    /// Plan ID.
    pub plan_id: String,
    /// The state.
    pub state: AgentPlanState,
    /// When the plan expires (Unix seconds).
    pub plan_expires_at: i64,
    /// When it was approved.
    pub approved_at: Option<i64>,
    /// How it was approved: `terminal` or `desktop`.
    pub channel: Option<String>,
    /// When the approval was used (executed, voided or revoked).
    pub used_at: Option<i64>,
    /// When a human denied it.
    pub denied_at: Option<i64>,
    /// The execution, once the agent started it.
    pub operation: Option<AgentOperation>,
}

/// An OS notification for an agent request (design 12b), posted by the app itself (never by
/// the MCP client, so the agent can't write to this channel). It approves nothing: the app's
/// Cleanup Review does, with the plan on screen and the typed code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestNotice {
    /// The plan it announces.
    pub plan_id: String,
    /// "An AI agent wants to clean 3.6 MiB".
    pub title: String,
    /// Items, plan, time left, and where to approve.
    pub body: String,
}

/// `6ae7·2359`: the first 8 characters of a plan ID without its `plan_` prefix.
fn short_plan(id: &str) -> String {
    let bare: Vec<char> = id.strip_prefix("plan_").unwrap_or(id).chars().collect();
    let part = |a: usize, b: usize| {
        bare[a.min(bare.len())..b.min(bare.len())]
            .iter()
            .collect::<String>()
    };
    format!("{}·{}", part(0, 4), part(4, 8))
}

/// Notifications for the requests in `pending` not announced yet; each plan is announced once
/// (`announced` remembers them for the life of the app).
pub fn request_notices(
    pending: &AgentRequests,
    announced: &mut HashSet<String>,
    now: i64,
) -> Vec<RequestNotice> {
    pending
        .requests
        .iter()
        .filter(|r| r.expires_at > now && announced.insert(r.plan_id.clone()))
        .map(|r| {
            let minutes = ((r.expires_at - now) + 59) / 60;
            RequestNotice {
                plan_id: r.plan_id.clone(),
                title: format!(
                    "An AI agent wants to clean {}",
                    scull_proactive::alert::format_bytes(r.reclaimable_bytes)
                ),
                body: format!(
                    "{} {} · plan {} · decide within {} min. Nothing runs until you approve it \
                     in scull's Cleanup Review.",
                    r.item_count,
                    if r.item_count == 1 { "item" } else { "items" },
                    short_plan(&r.plan_id),
                    minutes.max(1)
                ),
            }
        })
        .collect()
}

/// An opened approval dialog: the broker's challenge and the wrong codes so far.
struct Opened {
    challenge: Challenge,
    wrong: u32,
}

/// The desktop's approval broker client. One per app; holds the open challenges in memory.
pub struct ApprovalDesk {
    data_dir: Option<PathBuf>,
    principal: Option<Principal>,
    opened: Mutex<HashMap<String, Opened>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn refused(r: Refused) -> ScullError {
    ScullError::new(r.reason.kind(), r.message)
}

fn parse_plan_id(s: &str) -> Result<PlanId, ScullError> {
    PlanId::try_from(s.to_owned())
        .map_err(|_| ScullError::new(ErrorKind::Usage, "not a scull plan ID"))
}

impl ApprovalDesk {
    /// The desk for scull's data directory `data_dir` (`None`: no database, so no agent plans).
    /// The machine ID and OS user are looked up when needed, never created.
    pub fn new(data_dir: Option<PathBuf>) -> Self {
        Self {
            data_dir,
            principal: None,
            opened: Mutex::new(HashMap::new()),
        }
    }

    /// A desk acting as `principal` (tests).
    pub fn with_principal(data_dir: Option<PathBuf>, principal: Principal) -> Self {
        Self {
            principal: Some(principal),
            ..Self::new(data_dir)
        }
    }

    /// This installation's machine ID and the OS user; `None` when this machine has no ID yet
    /// (then no agent plan can be bound to it).
    fn principal(&self) -> Result<Option<Principal>, ScullError> {
        if let Some(p) = &self.principal {
            return Ok(Some(p.clone()));
        }
        let Some(data) = &self.data_dir else {
            return Ok(None);
        };
        let Some(machine_id) = scull_platform::existing_machine_id(data).map_err(|e| {
            ScullError::new(ErrorKind::Io, format!("can't read the machine ID: {e}"))
        })?
        else {
            return Ok(None);
        };
        let user = scull_platform::current_user().map_err(|e| {
            ScullError::new(ErrorKind::Io, format!("can't tell the current user: {e}"))
        })?;
        Ok(Some(Principal { machine_id, user }))
    }

    /// The database path when it exists (the desk never creates it).
    fn db(&self) -> Option<PathBuf> {
        let p = history_db_path(self.data_dir.as_deref()?);
        p.is_file().then_some(p)
    }

    /// The store for reading (never created, migrated or written), with the principal.
    fn reader(&self) -> Result<Option<(Store, Principal)>, ScullError> {
        let (Some(db), Some(who)) = (self.db(), self.principal()?) else {
            return Ok(None);
        };
        let store = Store::open_read_only(&db, &StoreOptions::default())?;
        Ok(Some((store, who)))
    }

    /// The store for recording a decision (it exists: the agent's plan is in it).
    fn writer(&self, id: &PlanId) -> Result<(Store, Principal), ScullError> {
        let unknown = || {
            ScullError::new(
                ErrorKind::NotFound,
                format!("no plan {id} was prepared by an AI agent on this machine"),
            )
        };
        let (Some(data), Some(_)) = (self.data_dir.as_deref(), self.db()) else {
            return Err(unknown());
        };
        let who = self.principal()?.ok_or_else(unknown)?;
        Ok((open_history(data)?, who))
    }

    /// Agent plans waiting for the human, newest first, without those a human denied.
    pub fn awaiting(&self, now: i64) -> Result<AgentRequests, ScullError> {
        let Some((store, who)) = self.reader()? else {
            return Ok(AgentRequests { requests: vec![] });
        };
        // The broker leaves out plans a human closed (denied, or too many wrong codes).
        let pending = approval::awaiting(&store, &who, now).map_err(refused)?;
        Ok(AgentRequests {
            requests: pending
                .into_iter()
                .map(|p| AgentRequest {
                    plan_id: p.plan_id,
                    content_hash: p.content_hash,
                    created_at: p.created_at,
                    expires_at: p.expires_at,
                    item_count: p.item_count,
                    reclaimable_bytes: p.reclaimable_bytes,
                })
                .collect(),
        })
    }

    /// Open plan `plan_id` for approval: a fresh challenge (new code) replaces any earlier
    /// opening of the same plan. `last_scan` (this app's last scan) adds evidence, reasons,
    /// flags and sizes to items it found unchanged.
    pub fn open(
        &self,
        plan_id: &str,
        last_scan: Option<&ScanReport>,
        now: i64,
    ) -> Result<AgentApprovalRequest, ScullError> {
        let id = parse_plan_id(plan_id)?;
        let Some((store, who)) = self.reader()? else {
            return Err(ScullError::new(
                ErrorKind::NotFound,
                format!("no plan {id} was prepared by an AI agent on this machine"),
            ));
        };
        let plan = approval::load_plan(&store, &id, &who, now).map_err(refused)?;
        if denials(&store, &who, plan.created_unix())?.contains_key(id.as_str()) {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!("plan {id} was closed (denied); the agent has to prepare a new plan"),
            ));
        }
        let challenge = approval::begin(&store, &id, &who, now).map_err(refused)?;
        let request = AgentApprovalRequest {
            plan_id: id.as_str().to_owned(),
            content_hash: plan.content_hash().as_str().to_owned(),
            created_at: plan.created_unix(),
            expires_at: plan.expires_unix(),
            items: review_items(challenge.plan(), last_scan),
            code: challenge.code().display(),
            attempts_left: MAX_CODE_ATTEMPTS,
        };
        lock(&self.opened).insert(
            id.as_str().to_owned(),
            Opened {
                challenge,
                wrong: 0,
            },
        );
        Ok(request)
    }

    /// Approve plan `plan_id` with the code the human typed. The plan must have been opened in
    /// this app ([`ApprovalDesk::open`]) and `content_hash` must be the hash shown there.
    pub fn approve(
        &self,
        plan_id: &str,
        content_hash: &str,
        typed: &str,
        now: i64,
    ) -> Result<AgentApproved, ScullError> {
        let id = parse_plan_id(plan_id)?;
        let mut opened = lock(&self.opened);
        let Some(entry) = opened.get_mut(id.as_str()) else {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!(
                    "plan {id} is not open for approval in this app; open the request and review it first"
                ),
            ));
        };
        let shown = entry.challenge.plan().content_hash().clone();
        if content_hash != shown.as_str() {
            return Err(refused(Refused {
                reason: Reason::ReviewedPlanDiffers,
                message: format!(
                    "plan {id} differs from the plan that was shown; nothing was approved"
                ),
            }));
        }
        let (mut store, who) = self.writer(&id)?;
        match approval::approve(
            &mut store,
            &entry.challenge,
            typed,
            HumanChannel::Desktop,
            &who,
            now,
        ) {
            Ok(a) => {
                opened.remove(id.as_str());
                Ok(AgentApproved {
                    plan_id: id.as_str().to_owned(),
                    approved_at: a.approved_at,
                    approval_expires_at: a.expires_at,
                })
            }
            Err(r) if r.reason == Reason::WrongCode => {
                entry.wrong += 1;
                if entry.wrong < MAX_CODE_ATTEMPTS {
                    let left = MAX_CODE_ATTEMPTS - entry.wrong;
                    return Err(ScullError::new(
                        r.reason.kind(),
                        format!(
                            "{} ({left} {} left)",
                            r.message,
                            if left == 1 { "try" } else { "tries" }
                        ),
                    ));
                }
                opened.remove(id.as_str());
                record(&mut store, &id, &shown, Denial::WrongCode, &who, now);
                Err(ScullError::new(
                    r.reason.kind(),
                    format!(
                        "the confirmation code was wrong {MAX_CODE_ATTEMPTS} times; nothing was \
                         approved and this request is closed. The agent has to prepare a new plan."
                    ),
                ))
            }
            Err(r) => {
                opened.remove(id.as_str());
                record(
                    &mut store,
                    &id,
                    &shown,
                    Denial::Refused(r.reason),
                    &who,
                    now,
                );
                Err(refused(r))
            }
        }
    }

    /// The human denied plan `plan_id` (the dialog's Deny button): recorded as a denial, the
    /// opened challenge is dropped, and the plan is no longer listed or opened here. Never
    /// creates, changes or spends an approval.
    pub fn deny(&self, plan_id: &str, now: i64) -> Result<AgentDenied, ScullError> {
        let id = parse_plan_id(plan_id)?;
        let shown = lock(&self.opened)
            .remove(id.as_str())
            .map(|o| o.challenge.plan().content_hash().clone());
        let (mut store, who) = self.writer(&id)?;
        // Only the requester's own pending plans can be denied (unknown, foreign, expired or
        // already approved plans are refused, and nothing is recorded).
        let plan = approval::load_plan(&store, &id, &who, now).map_err(refused)?;
        if store.get_approval(&id).map_err(ScullError::from)?.is_some() {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!("plan {id} is already approved; revoke the approval instead"),
            ));
        }
        let hash = shown.unwrap_or_else(|| plan.content_hash().clone());
        approval::record_denial(
            &mut store,
            &id,
            Some(&hash),
            Denial::Denied,
            HumanChannel::Desktop,
            &who,
            now,
        )
        .map_err(refused)?;
        Ok(AgentDenied {
            plan_id: id.as_str().to_owned(),
            denied_at: now,
        })
    }

    /// Revoke the approval of plan `plan_id` before the agent uses it ([`approval::revoke`]).
    pub fn revoke(&self, plan_id: &str, now: i64) -> Result<AgentRevoked, ScullError> {
        let id = parse_plan_id(plan_id)?;
        let (mut store, who) = self.writer(&id)?;
        let r =
            approval::revoke(&mut store, &id, HumanChannel::Desktop, &who, now).map_err(refused)?;
        Ok(AgentRevoked {
            plan_id: r.plan_id,
            revoked_at: r.revoked_at,
        })
    }

    /// Where plan `plan_id` stands, with the agent's execution once it started.
    pub fn status(&self, plan_id: &str, now: i64) -> Result<AgentPlanStatus, ScullError> {
        let id = parse_plan_id(plan_id)?;
        let Some((store, who)) = self.reader()? else {
            return Err(ScullError::new(
                ErrorKind::NotFound,
                format!("no plan {id} was prepared by an AI agent on this machine"),
            ));
        };
        let s = approval::status(&store, &id, &who, now).map_err(refused)?;
        let plan = store
            .get_plan(&id)
            .map_err(ScullError::from)?
            .ok_or_else(|| ScullError::new(ErrorKind::NotFound, format!("no plan {id}")))?;
        let mut operation = None;
        let mut denied_at = None;
        let state = match s.state {
            ApprovalState::AwaitingApproval | ApprovalState::Expired if s.approved_at.is_none() => {
                denied_at = denials(&store, &who, plan.created_unix())?
                    .get(id.as_str())
                    .copied();
                match (denied_at, s.state) {
                    (Some(_), _) => AgentPlanState::Denied,
                    (None, ApprovalState::AwaitingApproval) => AgentPlanState::AwaitingApproval,
                    (None, _) => AgentPlanState::Expired,
                }
            }
            ApprovalState::AwaitingApproval => AgentPlanState::AwaitingApproval,
            ApprovalState::Approved => AgentPlanState::Approved,
            ApprovalState::Expired => AgentPlanState::Expired,
            ApprovalState::Revoked => AgentPlanState::Revoked,
            ApprovalState::Used => {
                operation = executed(&store, &plan)?;
                match &operation {
                    None => AgentPlanState::Voided,
                    Some(op) if op.finished_at.is_none() => AgentPlanState::Running,
                    Some(_) => AgentPlanState::Executed,
                }
            }
        };
        Ok(AgentPlanStatus {
            plan_id: s.plan_id,
            state,
            plan_expires_at: s.plan_expires_at,
            approved_at: s.approved_at,
            channel: s.channel,
            used_at: s.used_at,
            denied_at,
            operation,
        })
    }

    /// Whether plan `plan_id` is open for approval in this app (tests, diagnostics).
    pub fn is_open(&self, plan_id: &str) -> bool {
        lock(&self.opened).contains_key(plan_id)
    }
}

/// Record a failed approval attempt; the refusal stands whether or not the audit write works.
fn record(
    store: &mut Store,
    id: &PlanId,
    hash: &ContentHash,
    denial: Denial,
    who: &Principal,
    now: i64,
) {
    let _ = approval::record_denial(
        store,
        id,
        Some(hash),
        denial,
        HumanChannel::Desktop,
        who,
        now,
    );
}

/// Plans of `who` a human closed since `since` (plan ID → when), from the agent audit events:
/// denied, or too many wrong codes, in the app or the terminal (the same rule as the broker's
/// list of awaiting plans).
fn denials(store: &Store, who: &Principal, since: i64) -> Result<HashMap<String, i64>, ScullError> {
    let events = store.agent_events(since, 1000).map_err(ScullError::from)?;
    Ok(events
        .into_iter()
        .filter(|e| {
            e.kind == EventKind::ApprovalDenied
                && (e.reason == Denial::Denied.code() || e.reason == Denial::WrongCode.code())
                && (e.source == "cli" || e.source == "desktop")
                && e.machine_id == who.machine_id
                && e.user == who.user
        })
        .filter_map(|e| Some((e.plan_id?.as_str().to_owned(), e.at)))
        .collect())
}

/// The agent's operation on `plan`, when the Executor started one.
fn executed(store: &Store, plan: &Plan) -> Result<Option<AgentOperation>, ScullError> {
    let Some(op) = approval::operations_of(store, plan)
        .map_err(refused)?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    let items: Vec<AgentItemResult> = store
        .audit_items(&op.operation_id)
        .map_err(ScullError::from)?
        .into_iter()
        .map(|a| AgentItemResult {
            item_id: a.item_id.as_str().to_owned(),
            outcome: a.outcome.into(),
            reason: a.reason,
            freed_bytes: a.freed_bytes.unwrap_or(0),
            moved_bytes: a.moved_bytes.unwrap_or(0),
        })
        .collect();
    let predicted: u64 = plan.items().iter().map(|i| i.reclaimable_bytes).sum();
    let (moved, freed, measured, finished_at) = match &op.finished {
        Some(f) => (
            f.totals.moved_bytes,
            f.totals.freed_bytes,
            f.totals.measured_delta_bytes,
            Some(f.finished_at),
        ),
        None => (
            items.iter().map(|i| i.moved_bytes).sum(),
            items.iter().map(|i| i.freed_bytes).sum(),
            None,
            None,
        ),
    };
    Ok(Some(AgentOperation {
        started_at: op.started_at,
        finished_at,
        items,
        predicted_bytes: predicted,
        moved_bytes: moved,
        freed_bytes: freed,
        measured_delta_bytes: measured,
    }))
}

/// The plan's items as the human reviews them: the plan's recorded fields, with the details of
/// this app's last scan when it found exactly the same item.
fn review_items(plan: &Plan, last_scan: Option<&ScanReport>) -> Vec<AgentPlanItem> {
    let scanned: HashMap<&str, &Candidate> = last_scan
        .map(|r| r.candidates.iter().map(|c| (c.id.as_str(), c)).collect())
        .unwrap_or_default();
    plan.items()
        .iter()
        .map(|item| match scanned.get(item.item_id.as_str()) {
            Some(c) if same_item(c, item) => AgentPlanItem {
                candidate: Candidate {
                    sizes: Sizes {
                        reclaimable: Reclaimable {
                            bytes: item.reclaimable_bytes,
                            ..c.sizes.reclaimable
                        },
                        ..c.sizes
                    },
                    ..(*c).clone()
                },
                from_last_scan: true,
            },
            _ => AgentPlanItem {
                candidate: recorded(item),
                from_last_scan: false,
            },
        })
        .collect()
}

fn same_item(c: &Candidate, item: &PlanItem) -> bool {
    c.identity == item.identity
        && c.path == item.path
        && c.tier == item.tier
        && c.action == item.action
        && c.rule == item.rule
}

/// A candidate holding only what the plan recorded: no evidence or reasons to show, flags
/// unknown (never "no"), sizes = the plan's reclaimable estimate.
fn recorded(item: &PlanItem) -> Candidate {
    let category_label = match item.category {
        Category::ProjectArtifact => "Project artifact",
        Category::PackageCache => "Package cache",
        Category::Worktree => "Git worktree",
        Category::MlCache => "ML cache",
        Category::Container => "Container data",
        Category::LargeFile => "Large file",
        Category::ScullData => "scull data",
        _ => "Other",
    };
    Candidate {
        id: item.item_id.clone(),
        path: item.path.clone(),
        identity: item.identity,
        category: item.category,
        artifact: category_label.to_owned(),
        rule: item.rule.clone(),
        project: None,
        tier: item.tier,
        reasons: vec![],
        evidence: vec![],
        sizes: Sizes {
            logical: item.reclaimable_bytes,
            allocated: item.reclaimable_bytes,
            reclaimable: Reclaimable {
                bytes: item.reclaimable_bytes,
                ..Reclaimable::default()
            },
        },
        flags: Flags::default(),
        regeneration: Regeneration::default(),
        action: item.action.clone(),
    }
}
