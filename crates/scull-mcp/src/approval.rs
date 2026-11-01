//! The approval broker (MCP-04, PRD §13.3): plans an AI agent prepares run only after a human
//! approves them **outside the model**.
//!
//! **An AI tool call is not human consent.** The flow, and where each step is enforced:
//!
//! 1. The agent calls `prepare_cleanup`: the server records an immutable plan (content hash H,
//!    origin `mcp`, 15-minute expiry, bound to this machine and user) and returns its ID and a
//!    summary. **Nothing the agent receives can approve anything**: there is no approval token,
//!    and the confirmation code below is never stored or sent through MCP.
//! 2. The human approves on a channel the model does not write to:
//!    * `scull approve <plan-id>` in their own **interactive terminal** (the CLI refuses
//!      without one: stdin, stdout and stderr must all be terminals, FX-MCP-08), which shows
//!      the full plan and asks the human to type a short code shown on screen
//!      ([`begin`], [`approve`] with [`HumanChannel::Terminal`]); or
//!    * the desktop Cleanup Review dialog (#109), through the same two calls with
//!      [`HumanChannel::Desktop`]; [`awaiting`] lists what can be approved.
//! 3. The approval is written to the local store (owner-only database): plan ID, hash H,
//!    channel, approver, time, expiry (the plan's own expiry, so at most 15 minutes after the
//!    plan was made), single use. The store refuses a second approval of the same plan, an
//!    approval bound to other content, and an approval of an expired plan (append-only
//!    triggers).
//! 4. `execute_cleanup` succeeds only with a matching, unexpired, unused, human approval
//!    ([`authorize`] is the server's read-only pre-check; the authority is the Executor, which
//!    **consumes** the approval atomically in `Executor::begin_operation` and refuses an MCP
//!    plan without it). Before execution the plan's items are re-derived from the disk: if
//!    **anything** about any of them changed (gone, replaced, moved, a different tier in
//!    either direction, a different action, a different reclaimable size, a different
//!    detection), the whole plan is void (D-34, provisional: [`check_unchanged`], [`void`])
//!    and the agent must prepare a new one. Every item is revalidated again by the Executor
//!    right before it is touched (SAFE-05).
//! 5. Without a valid approval everything fails closed. The plan, the approval and the
//!    executed operation (with the approval's channel and approver, per-item outcomes) are
//!    recorded in the store's append-only tables. What was **refused** is audited too, as
//!    agent audit events (`scull history` lists them): a denied approval ([`record_denial`]:
//!    `deny`, too many wrong codes, no terminal, or a refused attempt) and a refused
//!    `execute_cleanup` ([`record_execute_refusal`]; the Executor's gate records its own).
//!    They hold codes and identifiers only, never a confirmation code, and recording one
//!    never creates, changes or spends an approval.
//! 6. Until the agent executes it, the human can **revoke** the approval ([`revoke`]:
//!    `scull approve --revoke <plan-id>` in their terminal, or the desktop dialog). Revoking
//!    spends the approval in the same atomic update the Executor uses to consume it, so a
//!    revocation and a concurrent execution can't both win: exactly one does. It is audited
//!    in the same transaction, and the plan reports [`ApprovalState::Revoked`].
//!
//! # What the confirmation code is for, and what it is not
//!
//! The code proves that whoever approves *reads the screen the plan is shown on*: a pipe, a
//! piped `yes`, an agent's shell tool without a terminal, or a client that relays keystrokes
//! without showing the plan cannot approve. It is drawn from the OS random number generator
//! for each challenge (one `scull approve` run, or one opening of the desktop dialog;
//! [`ConfirmationCode::generate`]), stays the same across that challenge's retries, is shown
//! only on the approving surface, and is compared in constant time, case-, space- and
//! hyphen-insensitive (paste allowed, WCAG 2.2 SC 3.3.8).
//!
//! It is **not** a secret and not a proof of a human: a process that drives a
//! pseudo-terminal on the same host (`expect`, `script`, `tmux`) passes the terminal checks
//! and can read and type the code, and anything that can write the user's database can
//! insert an approval row. Both are the same trust boundary as an agent with unrestricted
//! shell access, which can delete files without scull anyway: out of scope (PRD §13.4,
//! `docs/THREAT_MODEL.md` T-41). The desktop dialog (#109) is the preferred approval
//! surface.
//!
//! # API for the desktop bridge (#109)
//!
//! [`awaiting`] (pending MCP plans, newest first, as [`PlanSummary`]), [`begin`] (a
//! [`Challenge`] holding the plan as reviewed and the code to show; keep it in memory, never
//! send it to the agent), [`approve`] (with [`HumanChannel::Desktop`] and the typed code) and
//! [`status`]; when the human cancels the dialog, [`record_denial`] with [`Denial::Denied`];
//! [`revocable`] lists approved plans not yet run and [`revoke`] (with
//! [`HumanChannel::Desktop`]) withdraws one.
//! Each takes the store, the [`Principal`] (this machine's ID and the current user) and the
//! current time; nothing here touches the disk beyond the store.

use scull_core::{
    Action, Candidate, ContentHash, DropReason, ErrorKind, ItemId, Plan, PlanId, PlanOrigin,
    ScullError, Tier, narrow,
};
use scull_storage::{
    AgentAuditEvent, Approval, ApprovalMethod, EventKind, NewApproval, StoreError,
};
use scull_storage::{OperationRecord, Store};
use serde::Serialize;

/// Characters a confirmation code is drawn from: `A`–`Z` and `2`–`9` without `I`, `O`, `0`
/// and `1`, which are easy to confuse (32 symbols, 5 bits each).
pub const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Length of a confirmation code (shown as `ABC-DEF`).
pub const CODE_LEN: usize = 6;

/// How many wrong codes `scull approve` accepts before it gives up (each attempt re-prompts).
pub const MAX_CODE_ATTEMPTS: u32 = 3;

/// Most plans [`awaiting`] lists.
pub const AWAITING_LIMIT: u32 = 50;

/// The human channel an approval comes through. Model-mediated channels (MCP elicitation, a
/// client's permission prompt, a CLI flag) have no variant: they are never an approval
/// (PRD §13.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HumanChannel {
    /// `scull approve <plan-id>` in an interactive terminal, with the typed code.
    Terminal,
    /// The desktop Cleanup Review dialog, with the typed code (#109).
    Desktop,
}

impl HumanChannel {
    /// The method recorded with the approval.
    pub fn method(self) -> ApprovalMethod {
        match self {
            HumanChannel::Terminal => ApprovalMethod::CliTty,
            HumanChannel::Desktop => ApprovalMethod::Desktop,
        }
    }

    /// Whether `method` is a human channel (the only methods that may approve an MCP plan).
    pub fn is_human(method: ApprovalMethod) -> bool {
        matches!(method, ApprovalMethod::CliTty | ApprovalMethod::Desktop)
    }
}

/// Who acts: this installation's machine ID and the OS user. Plans are bound to both
/// (SAFE-11), so another account can neither see nor approve them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// `scull_platform::machine_id`.
    pub machine_id: String,
    /// `scull_platform::current_user`.
    pub user: String,
}

/// Why the broker refused. Every refusal leaves the store unchanged except where noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reason {
    /// No plan with this ID was prepared through MCP on this machine (CLI and desktop plans
    /// are reported the same way: the agent never sees them).
    UnknownPlan,
    /// The plan expired (15 minutes after it was prepared).
    PlanExpired,
    /// The plan belongs to another machine or user.
    PlanForeign,
    /// The plan record does not match its hash, or is structurally invalid.
    PlanTampered,
    /// The plan has no approval yet (FX-MCP-01).
    NotApproved,
    /// The plan already has an approval (single use: one per plan).
    AlreadyApproved,
    /// The approval is bound to different plan content (FX-MCP-02).
    ApprovalMismatch,
    /// The approval expired (FX-MCP-03).
    ApprovalExpired,
    /// The approval was already used, or the plan was voided (FX-MCP-04).
    ApprovalUsed,
    /// The approval was not given through a human channel.
    NotHuman,
    /// The typed confirmation code is wrong.
    WrongCode,
    /// Items changed on disk since the plan was prepared: the plan is void (D-34).
    PlanChanged,
    /// The plan changed between showing it and approving it.
    ReviewedPlanDiffers,
    /// scull's database failed.
    Store,
    /// The OS random number generator failed (no confirmation code could be drawn).
    RandomUnavailable,
    /// The human revoked the approval before it was used (#298).
    ApprovalRevoked,
}

impl Reason {
    /// Stable machine code (the serialized form).
    pub fn code(self) -> &'static str {
        match self {
            Reason::UnknownPlan => "unknown_plan",
            Reason::PlanExpired => "plan_expired",
            Reason::PlanForeign => "plan_foreign",
            Reason::PlanTampered => "plan_tampered",
            Reason::NotApproved => "not_approved",
            Reason::AlreadyApproved => "already_approved",
            Reason::ApprovalMismatch => "approval_mismatch",
            Reason::ApprovalExpired => "approval_expired",
            Reason::ApprovalUsed => "approval_used",
            Reason::NotHuman => "not_human",
            Reason::WrongCode => "wrong_code",
            Reason::PlanChanged => "plan_changed",
            Reason::ReviewedPlanDiffers => "reviewed_plan_differs",
            Reason::Store => "store",
            Reason::RandomUnavailable => "random_unavailable",
            Reason::ApprovalRevoked => "approval_revoked",
        }
    }

    /// The error kind (and so the CLI exit code): plan problems are 4 (rejected), missing or
    /// spent approvals are 5 (blocked by safety policy).
    pub fn kind(self) -> ErrorKind {
        match self {
            Reason::UnknownPlan => ErrorKind::NotFound,
            Reason::PlanExpired | Reason::ApprovalExpired => ErrorKind::PlanExpired,
            Reason::PlanForeign => ErrorKind::PlanForeign,
            Reason::PlanTampered | Reason::ApprovalMismatch | Reason::ReviewedPlanDiffers => {
                ErrorKind::PlanTampered
            }
            Reason::PlanChanged => ErrorKind::PlanInvalidated,
            Reason::NotApproved
            | Reason::AlreadyApproved
            | Reason::ApprovalUsed
            | Reason::NotHuman
            | Reason::WrongCode
            | Reason::ApprovalRevoked => ErrorKind::SafetyBlocked,
            Reason::Store | Reason::RandomUnavailable => ErrorKind::Io,
        }
    }
}

/// A broker refusal: a stable [`Reason`] and a human explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// Why.
    pub reason: Reason,
    /// Explanation (plain text; contains plan IDs, never paths or codes).
    pub message: String,
}

impl Refused {
    fn new(reason: Reason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.reason.code())
    }
}

impl std::error::Error for Refused {}

impl From<Refused> for ScullError {
    fn from(r: Refused) -> Self {
        ScullError::new(r.reason.kind(), r.message)
    }
}

fn store_err(e: StoreError) -> Refused {
    Refused::new(Reason::Store, ScullError::from(e).message)
}

// ---------------------------------------------------------------------------------------------
// Confirmation codes
// ---------------------------------------------------------------------------------------------

/// A short code shown on the approving surface and typed back by the human. Never stored,
/// logged or sent through MCP. See the module docs for what it does and does not protect.
#[derive(Clone, PartialEq, Eq)]
pub struct ConfirmationCode([u8; CODE_LEN]);

impl std::fmt::Debug for ConfirmationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfirmationCode(..)")
    }
}

impl ConfirmationCode {
    /// A fresh code from the OS cryptographic random number generator (`getrandom`). Fails
    /// closed ([`Reason::RandomUnavailable`]) if the OS can't provide randomness.
    pub fn generate() -> Result<Self, Refused> {
        let mut bytes = [0u8; CODE_LEN];
        getrandom::fill(&mut bytes).map_err(|e| {
            Refused::new(
                Reason::RandomUnavailable,
                format!("the OS random number generator failed ({e}); nothing was approved"),
            )
        })?;
        // 32 symbols: the low 5 bits of each byte are uniform.
        Ok(Self(bytes.map(|b| CODE_ALPHABET[usize::from(b & 31)])))
    }

    /// A code from its characters (tests and fixed demonstrations). `None` unless it has
    /// [`CODE_LEN`] characters of [`CODE_ALPHABET`] after normalization.
    pub fn parse(s: &str) -> Option<Self> {
        let n = normalize(s);
        let bytes: [u8; CODE_LEN] = n.as_bytes().try_into().ok()?;
        bytes
            .iter()
            .all(|b| CODE_ALPHABET.contains(b))
            .then_some(Self(bytes))
    }

    /// The code as shown: `ABC-DEF`.
    pub fn display(&self) -> String {
        let s = String::from_utf8_lossy(&self.0).into_owned();
        format!("{}-{}", &s[..3], &s[3..])
    }

    /// Whether `typed` is this code, ignoring case, spaces and hyphens. Constant time in the
    /// code's length.
    pub fn matches(&self, typed: &str) -> bool {
        let n = normalize(typed);
        let typed = n.as_bytes();
        if typed.len() != CODE_LEN {
            return false;
        }
        self.0
            .iter()
            .zip(typed)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Plans awaiting approval
// ---------------------------------------------------------------------------------------------

/// A plan an agent prepared, as listed for approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct PlanSummary {
    /// Plan ID (what `scull approve` takes).
    pub plan_id: String,
    /// Content hash the approval binds to.
    pub content_hash: String,
    /// When it was prepared (Unix seconds).
    pub created_at: i64,
    /// When it expires unapproved (Unix seconds).
    pub expires_at: i64,
    /// Number of items.
    pub item_count: u64,
    /// Items by tier: S0, S1, S2 (S3 is never planned).
    pub items_by_tier: [u64; 3],
    /// Estimated reclaimable bytes at planning time.
    pub reclaimable_bytes: u64,
}

impl PlanSummary {
    /// The summary of `plan`.
    pub fn of(plan: &Plan) -> Self {
        let mut by_tier = [0u64; 3];
        for i in plan.items() {
            match i.tier {
                Tier::S0 => by_tier[0] += 1,
                Tier::S1 => by_tier[1] += 1,
                Tier::S2 => by_tier[2] += 1,
                Tier::S3 => {}
            }
        }
        Self {
            plan_id: plan.id().as_str().to_owned(),
            content_hash: plan.content_hash().as_str().to_owned(),
            created_at: plan.created_unix(),
            expires_at: plan.expires_unix(),
            item_count: plan.items().len() as u64,
            items_by_tier: by_tier,
            reclaimable_bytes: plan
                .items()
                .iter()
                .fold(0u64, |a, i| a.saturating_add(i.reclaimable_bytes)),
        }
    }
}

/// Plans prepared through MCP for `who` that still await an approval at `now`, newest first
/// (at most [`AWAITING_LIMIT`]). Plans that fail verification (tampered records) are left out.
pub fn awaiting(store: &Store, who: &Principal, now: i64) -> Result<Vec<PlanSummary>, Refused> {
    let ids = store
        .plans_awaiting_approval(PlanOrigin::Mcp, now, AWAITING_LIMIT)
        .map_err(store_err)?;
    let mut out = Vec::new();
    for id in ids {
        if let Ok(Some(plan)) = store.get_plan(&id)
            && plan.verify(now, &who.machine_id, &who.user).is_ok()
        {
            out.push(PlanSummary::of(&plan));
        }
    }
    Ok(out)
}

/// The MCP plan `id` as recorded, verified for `who` at `now` (hash, structure, machine and
/// user, expiry). CLI and desktop plans are reported as unknown.
pub fn load_plan(store: &Store, id: &PlanId, who: &Principal, now: i64) -> Result<Plan, Refused> {
    let plan = recorded_plan(store, id)?;
    plan.verify(now, &who.machine_id, &who.user)
        .map_err(|e| match e.kind {
            ErrorKind::PlanExpired => Refused::new(
                Reason::PlanExpired,
                format!(
                    "plan {id} expired (agent plans last 15 minutes): the agent must prepare a \
                     new plan"
                ),
            ),
            ErrorKind::PlanForeign => Refused::new(
                Reason::PlanForeign,
                format!("plan {id} was prepared for another machine or user"),
            ),
            _ => Refused::new(
                Reason::PlanTampered,
                format!("plan {id} is not valid: {}", e.message),
            ),
        })?;
    Ok(plan)
}

/// The recorded MCP plan `id`, without the expiry and principal checks.
fn recorded_plan(store: &Store, id: &PlanId) -> Result<Plan, Refused> {
    let unknown = || {
        Refused::new(
            Reason::UnknownPlan,
            format!("no plan {id} was prepared by an AI agent on this machine"),
        )
    };
    match store.get_plan(id) {
        Ok(Some(p)) if p.origin() == PlanOrigin::Mcp => Ok(p),
        Ok(_) => Err(unknown()),
        Err(StoreError::Rejected {
            kind: ErrorKind::PlanTampered,
            ..
        }) => Err(Refused::new(
            Reason::PlanTampered,
            format!("the record of plan {id} does not match its hash"),
        )),
        Err(e) => Err(store_err(e)),
    }
}

// ---------------------------------------------------------------------------------------------
// Who is approving (#35: agents that drive a pseudo-terminal)
// ---------------------------------------------------------------------------------------------

/// Executable names (case-insensitive, without `.exe`) of AI agent clients. A `scull approve`
/// with one of them among its ancestors is being run by that agent (its shell tool, or a
/// terminal it started), so it is refused. A heuristic: see [`check_approver`].
pub const AGENT_PROCESSES: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "aider",
    "goose",
    "opencode",
    "cursor-agent",
    "copilot",
];

/// Executable names of tools that drive a pseudo-terminal programmatically. Nobody needs one
/// to approve a plan in their own terminal; an agent uses one to pass the terminal checks and
/// type the code. (Terminal multiplexers such as `tmux` and `screen` are not on this list:
/// people use them every day. Their sessions are covered by [`AGENT_ENV_VARS`].)
pub const PTY_DRIVERS: &[&str] = &["expect", "unbuffer", "script", "empty"];

/// Environment variables AI agent clients set for the commands they run. They are inherited
/// by everything those commands start, including a `tmux` server and its panes.
pub const AGENT_ENV_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "GEMINI_CLI",
];

/// Which [`ApproverSignal`] it is (the stable reason code).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ApproverSignalKind {
    /// An agent's environment variable is set.
    AgentEnvironment,
    /// An agent client is among the ancestor processes.
    AgentProcess,
    /// A pseudo-terminal driver is among the ancestor processes.
    PtyDriver,
    /// The ancestor processes can't be read (fail closed).
    AncestryUnknown,
}

impl ApproverSignalKind {
    /// Stable code: `agent_environment`, `agent_process`, `pty_driver`, `ancestry_unknown`.
    pub fn code(self) -> &'static str {
        match self {
            ApproverSignalKind::AgentEnvironment => "agent_environment",
            ApproverSignalKind::AgentProcess => "agent_process",
            ApproverSignalKind::PtyDriver => "pty_driver",
            ApproverSignalKind::AncestryUnknown => "ancestry_unknown",
        }
    }
}

/// Why [`check_approver`] refused: the kind and what was seen (a variable or process name, or
/// why the ancestry couldn't be read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproverSignal {
    /// The kind.
    pub kind: ApproverSignalKind,
    /// The variable, process name or error behind it.
    pub detail: String,
}

impl ApproverSignal {
    /// A sentence for the human (no code, no path).
    pub fn explain(&self) -> String {
        match self.kind {
            ApproverSignalKind::AgentEnvironment => format!(
                "this terminal carries the environment of an AI agent ({} is set)",
                self.detail
            ),
            ApproverSignalKind::AgentProcess => format!(
                "this terminal was started by an AI agent ({} is among its parent processes)",
                self.detail
            ),
            ApproverSignalKind::PtyDriver => format!(
                "this terminal is driven by an automation tool ({} is among its parent processes)",
                self.detail
            ),
            ApproverSignalKind::AncestryUnknown => format!(
                "scull can't tell which processes started this terminal ({})",
                self.detail
            ),
        }
    }
}

/// The name of a process as compared with [`AGENT_PROCESSES`] and [`PTY_DRIVERS`]: lowercase,
/// without a trailing `.exe`.
fn process_key(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase();
    n.strip_suffix(".exe").map(str::to_owned).unwrap_or(n)
}

/// #35: whether the terminal `scull approve` (or another human approval surface) runs in
/// looks driven by an AI agent or an automation tool. `ancestry` is the names of this
/// process's ancestors, parent first, or why they couldn't be read (then this fails closed);
/// `env_set` says whether an environment variable is set (to anything non-empty).
///
/// Checked in order: [`AGENT_ENV_VARS`], an unreadable ancestry, [`AGENT_PROCESSES`] among the
/// ancestors, then [`PTY_DRIVERS`] among them.
///
/// **A heuristic, not a proof of a human.** It stops an agent that runs `scull approve`
/// through its shell tool, under `script`/`expect`, or in a `tmux` session it started (those
/// inherit its environment). It does not stop an agent that renames its tools, scrubs its
/// environment and launches a daemonized multiplexer, nor anything that writes the database
/// directly: the out-of-scope case of PRD §13.4 (`docs/APPROVAL_HARDENING.md`).
pub fn check_approver(
    ancestry: Result<&[String], &str>,
    env_set: &dyn Fn(&str) -> bool,
) -> Result<(), ApproverSignal> {
    if let Some(var) = AGENT_ENV_VARS.iter().find(|v| env_set(v)) {
        return Err(ApproverSignal {
            kind: ApproverSignalKind::AgentEnvironment,
            detail: (*var).to_owned(),
        });
    }
    let names = ancestry.map_err(|why| ApproverSignal {
        kind: ApproverSignalKind::AncestryUnknown,
        detail: why.to_owned(),
    })?;
    let keys: Vec<String> = names.iter().map(|n| process_key(n)).collect();
    if let Some(k) = keys.iter().find(|k| AGENT_PROCESSES.contains(&k.as_str())) {
        return Err(ApproverSignal {
            kind: ApproverSignalKind::AgentProcess,
            detail: k.clone(),
        });
    }
    if let Some(k) = keys.iter().find(|k| PTY_DRIVERS.contains(&k.as_str())) {
        return Err(ApproverSignal {
            kind: ApproverSignalKind::PtyDriver,
            detail: k.clone(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Approving
// ---------------------------------------------------------------------------------------------

/// One approval attempt: the plan exactly as shown to the human and the code to show with it.
/// Not serializable and not cloneable: it lives in the approving process only (the terminal
/// running `scull approve`, or the desktop backend) and never reaches the agent.
#[derive(Debug)]
pub struct Challenge {
    plan: Plan,
    code: ConfirmationCode,
}

impl Challenge {
    /// The plan to show in full (verified when the challenge was made).
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The code to show next to the plan; the human types it back.
    pub fn code(&self) -> &ConfirmationCode {
        &self.code
    }

    /// A challenge with a fixed code (tests and reproducible demonstrations only; real
    /// approvals use [`begin`], which draws a random code).
    pub fn with_code(mut self, code: ConfirmationCode) -> Self {
        self.code = code;
        self
    }
}

/// Start approving plan `id`: it must be an MCP plan for `who`, unexpired, valid and not yet
/// approved. Returns the [`Challenge`] to show.
pub fn begin(store: &Store, id: &PlanId, who: &Principal, now: i64) -> Result<Challenge, Refused> {
    let plan = load_plan(store, id, who, now)?;
    if store.get_approval(id).map_err(store_err)?.is_some() {
        return Err(Refused::new(
            Reason::AlreadyApproved,
            format!("plan {id} was already approved (an approval is single-use)"),
        ));
    }
    Ok(Challenge {
        plan,
        code: ConfirmationCode::generate()?,
    })
}

/// Record the human approval of the challenged plan when `typed` is its code. Refuses with
/// [`Reason::WrongCode`] (nothing recorded) on a wrong code, and re-checks the plan: still
/// recorded with the same content hash as shown, unexpired, unapproved. The approval lapses
/// with the plan (at most 15 minutes after it was prepared) and is single-use.
pub fn approve(
    store: &mut Store,
    challenge: &Challenge,
    typed: &str,
    channel: HumanChannel,
    who: &Principal,
    now: i64,
) -> Result<Approval, Refused> {
    if !challenge.code.matches(typed) {
        return Err(Refused::new(
            Reason::WrongCode,
            "the confirmation code does not match; nothing was approved",
        ));
    }
    let shown = &challenge.plan;
    let plan = load_plan(store, shown.id(), who, now)?;
    if plan.content_hash() != shown.content_hash() || plan != *shown {
        return Err(Refused::new(
            Reason::ReviewedPlanDiffers,
            format!(
                "plan {} differs from the plan that was shown; nothing was approved",
                shown.id()
            ),
        ));
    }
    let new = NewApproval {
        plan_id: plan.id().clone(),
        content_hash: plan.content_hash().clone(),
        method: channel.method(),
        approver: who.user.clone(),
        approved_at: now,
        expires_at: plan.expires_unix(),
    };
    if let Err(e) = store.insert_approval(&new) {
        let kind = match &e {
            StoreError::Rejected { kind, .. } => Some(*kind),
            _ => None,
        };
        return Err(match kind {
            // Two approvals raced: the database keeps exactly one (UNIQUE plan_id).
            Some(ErrorKind::Usage) if store.get_approval(plan.id()).ok().flatten().is_some() => {
                Refused::new(
                    Reason::AlreadyApproved,
                    format!("plan {} was already approved (single use)", plan.id()),
                )
            }
            Some(ErrorKind::PlanExpired) => Refused::new(
                Reason::PlanExpired,
                format!("plan {} expired before it was approved", plan.id()),
            ),
            Some(ErrorKind::PlanTampered) => Refused::new(
                Reason::ApprovalMismatch,
                "the approval does not match the recorded plan",
            ),
            _ => store_err(e),
        });
    }
    store
        .get_approval(plan.id())
        .map_err(store_err)?
        .ok_or_else(|| Refused::new(Reason::Store, "the approval was not recorded"))
}

// ---------------------------------------------------------------------------------------------
// Status and execution checks
// ---------------------------------------------------------------------------------------------

/// Where a plan stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// Waiting for the human (`scull approve <plan-id>` or the desktop app).
    AwaitingApproval,
    /// Approved and executable once, until `approval_expires_at`.
    Approved,
    /// The approval was used: by an execution, or to void the plan (D-34).
    Used,
    /// The plan or its approval expired unused.
    Expired,
    /// The human revoked the approval before it was used: nothing will run.
    Revoked,
}

/// The approval status of a plan. Contains no token and no code: nothing in it can approve
/// or execute anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ApprovalStatus {
    /// Plan ID.
    pub plan_id: String,
    /// The state.
    pub state: ApprovalState,
    /// When the plan expires (Unix seconds).
    pub plan_expires_at: i64,
    /// When it was approved.
    pub approved_at: Option<i64>,
    /// When the approval lapses.
    pub approval_expires_at: Option<i64>,
    /// How it was approved: `terminal` or `desktop`.
    pub channel: Option<String>,
    /// When the approval was used.
    pub used_at: Option<i64>,
    /// Whether `execute_cleanup` would be accepted now (it still re-checks everything).
    pub executable: bool,
}

/// The approval status of MCP plan `id` at `now`. Expired plans still report (as
/// [`ApprovalState::Expired`] or, once used, [`ApprovalState::Used`]); foreign and unknown
/// plans are refused.
pub fn status(
    store: &Store,
    id: &PlanId,
    who: &Principal,
    now: i64,
) -> Result<ApprovalStatus, Refused> {
    let plan = recorded_plan(store, id)?;
    if plan.machine_id() != who.machine_id || plan.user() != who.user {
        return Err(Refused::new(
            Reason::PlanForeign,
            format!("plan {id} was prepared for another machine or user"),
        ));
    }
    let approval = store.get_approval(id).map_err(store_err)?;
    let plan_live = now < plan.expires_unix();
    let revoked = approval.as_ref().is_some_and(|a| a.used_at.is_some())
        && store.approval_revoked(id).map_err(store_err)?;
    let state = match &approval {
        None if plan_live => ApprovalState::AwaitingApproval,
        None => ApprovalState::Expired,
        Some(_) if revoked => ApprovalState::Revoked,
        Some(a) if a.used_at.is_some() => ApprovalState::Used,
        Some(a) if plan_live && now < a.expires_at => ApprovalState::Approved,
        Some(_) => ApprovalState::Expired,
    };
    Ok(ApprovalStatus {
        plan_id: id.as_str().to_owned(),
        state,
        plan_expires_at: plan.expires_unix(),
        approved_at: approval.as_ref().map(|a| a.approved_at),
        approval_expires_at: approval.as_ref().map(|a| a.expires_at),
        channel: approval.as_ref().map(|a| channel_name(a.method).to_owned()),
        used_at: approval.as_ref().and_then(|a| a.used_at),
        executable: state == ApprovalState::Approved
            && approval
                .as_ref()
                .is_some_and(|a| HumanChannel::is_human(a.method)),
    })
}

/// The stable channel name of an approval method (`terminal`, `desktop`, or the method's
/// stored name for non-human methods).
pub fn channel_name(m: ApprovalMethod) -> &'static str {
    match m {
        ApprovalMethod::CliTty => "terminal",
        ApprovalMethod::Desktop => "desktop",
        other => other.as_str(),
    }
}

/// A plan with an approval that passed [`authorize`].
#[derive(Debug, Clone)]
pub struct Authorized {
    /// The verified plan.
    pub plan: Plan,
    /// Its unused, unexpired, human approval bound to the plan's hash.
    pub approval: Approval,
}

/// The server's read-only pre-check before `execute_cleanup` (FX-MCP-01…04): the MCP plan
/// `id` is valid for `who` at `now`, and has an approval that is human, bound to its exact
/// content hash, unexpired and unused. Nothing is consumed here: the Executor consumes the
/// approval atomically when the operation starts (and refuses on its own if it can't).
pub fn authorize(
    store: &Store,
    id: &PlanId,
    who: &Principal,
    now: i64,
) -> Result<Authorized, Refused> {
    let plan = match load_plan(store, id, who, now) {
        Ok(p) => p,
        // An approval that was used before the plan expired reads better as "used".
        Err(r) if r.reason == Reason::PlanExpired => {
            if let Ok(Some(a)) = store.get_approval(id)
                && a.used_at.is_some()
            {
                return Err(spent(store, id));
            }
            return Err(r);
        }
        Err(r) => return Err(r),
    };
    let Some(approval) = store.get_approval(id).map_err(store_err)? else {
        return Err(Refused::new(
            Reason::NotApproved,
            format!(
                "plan {id} has not been approved. Only the user can approve it, outside this \
                 chat: ask them to run `scull approve {id}` in their own terminal (or to review \
                 it in the scull desktop app). Nothing was changed."
            ),
        ));
    };
    if !HumanChannel::is_human(approval.method) {
        return Err(Refused::new(
            Reason::NotHuman,
            format!("plan {id} was not approved through the terminal or the desktop app"),
        ));
    }
    if approval.content_hash != *plan.content_hash() {
        return Err(Refused::new(
            Reason::ApprovalMismatch,
            format!("the approval of plan {id} is for different plan content; nothing was changed"),
        ));
    }
    if approval.used_at.is_some() {
        return Err(spent(store, id));
    }
    if now < approval.approved_at || now >= approval.expires_at {
        return Err(Refused::new(
            Reason::ApprovalExpired,
            format!(
                "the approval of plan {id} has expired: the agent must prepare a new plan and \
                 the user approve it again"
            ),
        ));
    }
    Ok(Authorized { plan, approval })
}

/// The refusal for a spent approval: revoked by the human, or used.
fn spent(store: &Store, id: &PlanId) -> Refused {
    if store.approval_revoked(id).unwrap_or(false) {
        return revoked(id);
    }
    used(id)
}

fn revoked(id: &PlanId) -> Refused {
    Refused::new(
        Reason::ApprovalRevoked,
        format!(
            "the user revoked the approval of plan {id}: it will not run. Nothing was changed; \
             prepare a new plan only if the user still wants this cleanup"
        ),
    )
}

fn used(id: &PlanId) -> Refused {
    Refused::new(
        Reason::ApprovalUsed,
        format!(
            "the approval of plan {id} was already used (approvals are single-use, and a plan \
             whose items changed is void): prepare a new plan"
        ),
    )
}

/// How a plan item differs from what is on disk now (D-34).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Change {
    /// [`narrow`] dropped it (gone, replaced, moved, ambiguous, now protected, tier raised,
    /// action no longer allowed, ...).
    Dropped(DropReason),
    /// Its tier is different (here: lower; a higher tier is a drop).
    Tier {
        /// Tier when the plan was approved.
        planned: Tier,
        /// Tier now.
        fresh: Tier,
    },
    /// The proposed action is different.
    Action,
    /// The reclaimable size is different (the contents changed).
    Size {
        /// Bytes when the plan was approved.
        planned: u64,
        /// Bytes now.
        fresh: u64,
    },
    /// It is detected differently (category or rule).
    Detection,
}

impl Change {
    /// Stable machine code: `narrow`'s drop reason, or `tier_changed`, `action_changed`,
    /// `size_changed`, `detection_changed`.
    pub fn code(&self) -> &'static str {
        match self {
            Change::Dropped(r) => r.code(),
            Change::Tier { .. } => "tier_changed",
            Change::Action => "action_changed",
            Change::Size { .. } => "size_changed",
            Change::Detection => "detection_changed",
        }
    }
}

/// One plan item that no longer matches the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedItem {
    /// The item.
    pub item_id: ItemId,
    /// How it changed.
    pub change: Change,
}

/// D-34 (provisional): an agent's plan runs only exactly as approved. Re-derive the
/// candidates from the disk (`fresh`): if **any** item changed in any way, the whole plan is
/// void. That is stricter than [`narrow`], which CLI and desktop plans keep using (it skips
/// changed items, and accepts a lower tier, a safer action and a new size): here an item
/// [`narrow`] drops, or whose fresh candidate differs from the approved item in tier, action,
/// reclaimable bytes, category or rule, is a change. Returns every changed item.
pub fn check_unchanged(plan: &Plan, fresh: &[Candidate]) -> Result<(), Vec<ChangedItem>> {
    let n = narrow(plan, fresh);
    let mut changed: Vec<ChangedItem> = n
        .dropped
        .into_iter()
        .map(|(item_id, reason)| ChangedItem {
            item_id,
            change: Change::Dropped(reason),
        })
        .collect();
    for item in plan.items() {
        if changed.iter().any(|c| c.item_id == item.item_id) {
            continue;
        }
        // `narrow` kept it, so exactly one fresh candidate has this ID, path and identity.
        let Some(c) = fresh.iter().find(|c| c.id == item.item_id) else {
            changed.push(ChangedItem {
                item_id: item.item_id.clone(),
                change: Change::Dropped(DropReason::Missing),
            });
            continue;
        };
        let change = if c.tier != item.tier {
            Some(Change::Tier {
                planned: item.tier,
                fresh: c.tier,
            })
        } else if c.action != item.action {
            Some(Change::Action)
        } else if c.sizes.reclaimable.bytes != item.reclaimable_bytes {
            Some(Change::Size {
                planned: item.reclaimable_bytes,
                fresh: c.sizes.reclaimable.bytes,
            })
        } else if c.category != item.category || c.rule != item.rule {
            Some(Change::Detection)
        } else {
            None
        };
        if let Some(change) = change {
            changed.push(ChangedItem {
                item_id: item.item_id.clone(),
                change,
            });
        }
    }
    if changed.is_empty() {
        Ok(())
    } else {
        Err(changed)
    }
}

/// Void plan `plan` after [`check_unchanged`] failed (D-34): its approval is marked used, so
/// it can never execute, even if the disk changes back. Returns whether an approval was
/// consumed.
pub fn void(store: &mut Store, plan: &Plan, now: i64) -> Result<bool, Refused> {
    store
        .consume_approval(plan.id(), plan.content_hash(), now)
        .map_err(store_err)
}

// ---------------------------------------------------------------------------------------------
// Revoking an approval (#298)
// ---------------------------------------------------------------------------------------------

/// A revoked approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Revoked {
    /// The plan whose approval was revoked.
    pub plan_id: String,
    /// Its content hash.
    pub content_hash: String,
    /// When (Unix seconds).
    pub revoked_at: i64,
    /// How: `terminal` or `desktop`.
    pub channel: String,
}

/// Revoke the human approval of MCP plan `id` before the agent uses it (#298), through
/// `channel` (the `scull approve --revoke` terminal or the desktop dialog). Same principal
/// checks as approving: an agent plan of this machine and user, approved by this user. The
/// approval is spent in the same atomic update the Executor uses to consume it, and the
/// revocation is audited ([`scull_storage::EventKind::ApprovalRevoked`]) in that
/// transaction: if the agent's execution started first, the revocation is refused with
/// [`Reason::ApprovalUsed`] and nothing is written. Refused with [`Reason::NotApproved`]
/// when there is no approval (the plan just expires unused), [`Reason::ApprovalRevoked`]
/// when already revoked, [`Reason::ApprovalExpired`] when it lapsed. Never creates an
/// approval. Every refusal is audited as `revoke_refused` with its reason code (that
/// event says nothing about the approval itself).
pub fn revoke(
    store: &mut Store,
    id: &PlanId,
    channel: HumanChannel,
    who: &Principal,
    now: i64,
) -> Result<Revoked, Refused> {
    let r = revoke_checked(store, id, channel, who, now);
    if let Err(refusal) = &r {
        // Audited (bounded like every refusal); the outcome stands either way.
        let _ = store.record_agent_event(&AgentAuditEvent {
            event_id: 0,
            at: now,
            kind: EventKind::RevokeRefused,
            source: channel_source(channel).into(),
            plan_id: Some(id.clone()),
            content_hash: recorded_hash(store, id),
            reason: refusal.reason.code().into(),
            channel: channel_name(channel.method()).into(),
            machine_id: who.machine_id.clone(),
            user: who.user.clone(),
            count: 1,
        });
    }
    r
}

fn channel_source(channel: HumanChannel) -> &'static str {
    match channel {
        HumanChannel::Terminal => "cli",
        HumanChannel::Desktop => "desktop",
    }
}

fn revoke_checked(
    store: &mut Store,
    id: &PlanId,
    channel: HumanChannel,
    who: &Principal,
    now: i64,
) -> Result<Revoked, Refused> {
    let plan = recorded_plan(store, id)?;
    if plan.machine_id() != who.machine_id || plan.user() != who.user {
        return Err(Refused::new(
            Reason::PlanForeign,
            format!("plan {id} was prepared for another machine or user"),
        ));
    }
    let Some(approval) = store.get_approval(id).map_err(store_err)? else {
        return Err(Refused::new(
            Reason::NotApproved,
            format!("plan {id} has no approval to revoke; unapproved, it expires unused"),
        ));
    };
    if approval.approver != who.user {
        return Err(Refused::new(
            Reason::PlanForeign,
            format!("plan {id} was approved by another account"),
        ));
    }
    let refusal = |store: &Store, approval: &Approval| {
        if approval.used_at.is_some() {
            spent(store, id)
        } else {
            Refused::new(
                Reason::ApprovalExpired,
                format!("the approval of plan {id} already expired; nothing can run it"),
            )
        }
    };
    if approval.used_at.is_some() || now < approval.approved_at || now >= approval.expires_at {
        return Err(refusal(store, &approval));
    }
    let source = match channel {
        HumanChannel::Terminal => "cli",
        HumanChannel::Desktop => "desktop",
    };
    let audit = AgentAuditEvent {
        event_id: 0,
        at: now,
        kind: EventKind::ApprovalRevoked,
        source: source.into(),
        plan_id: Some(id.clone()),
        content_hash: Some(approval.content_hash.clone()),
        reason: "revoked".into(),
        channel: channel_name(channel.method()).into(),
        machine_id: who.machine_id.clone(),
        user: who.user.clone(),
        count: 1,
    };
    let done = store
        .revoke_approval(id, &approval.content_hash, now, &audit)
        .map_err(store_err)?;
    if !done {
        // Lost the race (the Executor consumed it first) or it lapsed meanwhile.
        let now_approval = store
            .get_approval(id)
            .map_err(store_err)?
            .unwrap_or(approval);
        return Err(refusal(store, &now_approval));
    }
    Ok(Revoked {
        plan_id: id.as_str().to_owned(),
        content_hash: plan.content_hash().as_str().to_owned(),
        revoked_at: now,
        channel: channel_name(channel.method()).to_owned(),
    })
}

/// MCP plans of `who` with an approval that is unused and unexpired at `now` (approved, not
/// yet run): what [`revoke`] can withdraw, newest first (at most [`AWAITING_LIMIT`]).
pub fn revocable(store: &Store, who: &Principal, now: i64) -> Result<Vec<PlanSummary>, Refused> {
    let ids = store
        .plans_with_live_approval(PlanOrigin::Mcp, now, AWAITING_LIMIT)
        .map_err(store_err)?;
    let mut out = Vec::new();
    for id in ids {
        if let Ok(Some(plan)) = store.get_plan(&id)
            && plan.verify(now, &who.machine_id, &who.user).is_ok()
        {
            out.push(PlanSummary::of(&plan));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Audit of refusals (PRD §13.3 step 5)
// ---------------------------------------------------------------------------------------------

/// Why an approval attempt ended without an approval (recorded by [`record_denial`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Denial {
    /// The human said no: typed `deny`, an empty line, closed the input or the dialog.
    Denied,
    /// The confirmation code was typed wrong [`MAX_CODE_ATTEMPTS`] times.
    WrongCode,
    /// `scull approve` ran without an interactive terminal (FX-MCP-08): a pipe, a script, an
    /// agent's shell tool.
    NoTerminal,
    /// The broker refused the attempt (expired, foreign, unknown or changed plan, already
    /// approved, ...).
    Refused(Reason),
    /// The terminal looked driven by an agent or an automation tool ([`check_approver`]).
    Approver(ApproverSignalKind),
}

impl Denial {
    /// Stable reason code: `denied`, `wrong_code`, `no_terminal`, or the [`Reason`]'s code.
    pub fn code(self) -> &'static str {
        match self {
            Denial::Denied => "denied",
            Denial::WrongCode => "wrong_code",
            Denial::NoTerminal => "no_terminal",
            Denial::Refused(r) => r.code(),
            Denial::Approver(k) => k.code(),
        }
    }
}

/// Record that an attempt to approve plan `plan_id` through `channel` ended without an
/// approval ([`EventKind::ApprovalDenied`]): reason, plan ID and hash (when known), channel,
/// machine and user. Never records the code, and never touches the approval itself.
pub fn record_denial(
    store: &mut Store,
    plan_id: &PlanId,
    content_hash: Option<&ContentHash>,
    denial: Denial,
    channel: HumanChannel,
    who: &Principal,
    now: i64,
) -> Result<(), Refused> {
    let source = match channel {
        HumanChannel::Terminal => "cli",
        HumanChannel::Desktop => "desktop",
    };
    store
        .record_agent_event(&AgentAuditEvent {
            event_id: 0,
            at: now,
            kind: EventKind::ApprovalDenied,
            source: source.into(),
            plan_id: Some(plan_id.clone()),
            content_hash: content_hash.cloned(),
            reason: denial.code().into(),
            channel: channel_name(channel.method()).into(),
            machine_id: who.machine_id.clone(),
            user: who.user.clone(),
            count: 1,
        })
        .map_err(store_err)
}

/// Record that the MCP server refused an `execute_cleanup` of `plan_id` (when the agent gave
/// a valid one) with the stable code `reason` ([`EventKind::ExecuteRefused`], source and
/// channel `mcp`). Refusals by the Executor's own gate are recorded by the Executor.
pub fn record_execute_refusal(
    store: &mut Store,
    plan_id: Option<&PlanId>,
    content_hash: Option<&ContentHash>,
    reason: &str,
    who: &Principal,
    now: i64,
) -> Result<(), Refused> {
    store
        .record_agent_event(&AgentAuditEvent {
            event_id: 0,
            at: now,
            kind: EventKind::ExecuteRefused,
            source: "mcp".into(),
            plan_id: plan_id.cloned(),
            content_hash: content_hash.cloned(),
            reason: reason.into(),
            channel: "mcp".into(),
            machine_id: who.machine_id.clone(),
            user: who.user.clone(),
            count: 1,
        })
        .map_err(store_err)
}

/// Record that the MCP server refused a `prepare_cleanup` with the stable code `reason`
/// (`rate_limited`, `too_many_pending_plans`; [`EventKind::PrepareRefused`], no plan).
pub fn record_prepare_refusal(
    store: &mut Store,
    reason: &str,
    who: &Principal,
    now: i64,
) -> Result<(), Refused> {
    store
        .record_agent_event(&AgentAuditEvent {
            event_id: 0,
            at: now,
            kind: EventKind::PrepareRefused,
            source: "mcp".into(),
            plan_id: None,
            content_hash: None,
            reason: reason.into(),
            channel: "mcp".into(),
            machine_id: who.machine_id.clone(),
            user: who.user.clone(),
            count: 1,
        })
        .map_err(store_err)
}

/// The content hash of the recorded plan `id`, if there is a valid one (for audit events).
pub fn recorded_hash(store: &Store, id: &PlanId) -> Option<ContentHash> {
    store
        .get_plan(id)
        .ok()
        .flatten()
        .map(|p| p.content_hash().clone())
}

/// The operations recorded for plan `id` (at most one for an MCP plan: approvals are
/// single-use), oldest first.
pub fn operations_of(store: &Store, plan: &Plan) -> Result<Vec<OperationRecord>, Refused> {
    Ok(store
        .operations(plan.created_unix())
        .map_err(store_err)?
        .into_iter()
        .filter(|o| o.plan_id == *plan.id())
        .collect())
}

/// Whether `c` may be put into an agent plan, and if not why: the same rule as `scull clean`
/// (never S3, never inspect-only, a delete or quarantine allowed for its tier, D-3/D-28).
/// Package caches belong to `scull clean caches` (their own tools) and Git worktree
/// operations are not executed by this build.
pub fn ineligible(c: &Candidate) -> Option<&'static str> {
    if !c.tier.is_actionable() {
        return Some("protected (S3): never cleaned");
    }
    match &c.action {
        Action::None => Some("no cleanup action at this tier (inspect only)"),
        Action::ToolCommand { .. } => {
            Some("package cache: the user cleans it with `scull clean caches`")
        }
        Action::Delete | Action::Quarantine
            if c.is_consistent() && scull_core::action_allowed_for(c.tier, &c.action) =>
        {
            None
        }
        Action::Delete | Action::Quarantine => Some("its tier, reasons and action disagree"),
        _ => Some("this action is not executed through MCP in this build"),
    }
}

/// A new plan ID: Unix seconds then 64 random bits from the OS random number generator, in
/// hex. (If the OS generator fails, the bits come from the standard library's randomly keyed
/// hasher: plan IDs are identifiers, not secrets.)
pub fn new_plan_id(now: i64) -> PlanId {
    let mut bytes = [0u8; 8];
    let random = if getrandom::fill(&mut bytes).is_ok() {
        u64::from_le_bytes(bytes)
    } else {
        use std::hash::{BuildHasher, RandomState};
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        RandomState::new().hash_one((nanos, std::process::id()))
    };
    let suffix = format!("{:x}{random:016x}", u64::try_from(now).unwrap_or(0));
    #[allow(clippy::expect_used)] // Lowercase hex is always a valid suffix.
    PlanId::from_suffix(&suffix).expect("hex suffix is a valid plan ID")
}

/// The content hash `h` as a short display form (first 12 hex digits).
pub fn short_hash(h: &ContentHash) -> &str {
    &h.as_str()[..12]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_drawn_from_the_alphabet_and_vary() {
        let a = ConfirmationCode::generate().unwrap();
        let shown = a.display();
        assert_eq!(shown.len(), CODE_LEN + 1);
        assert_eq!(&shown[3..4], "-");
        assert!(
            shown
                .bytes()
                .filter(|b| *b != b'-')
                .all(|b| CODE_ALPHABET.contains(&b))
        );
        let many: std::collections::BTreeSet<String> = (0..64)
            .map(|_| ConfirmationCode::generate().unwrap().display())
            .collect();
        assert!(many.len() > 60, "codes repeat too often: {many:?}");
        assert_eq!(format!("{a:?}"), "ConfirmationCode(..)", "never logged");
    }

    #[test]
    fn code_matching_ignores_case_spaces_and_hyphens_only() {
        let c = ConfirmationCode::parse("ABC-DEF").unwrap();
        for ok in ["ABC-DEF", "abcdef", " abc def\n", "a-b-c-d-e-f", "AbCdEf"] {
            assert!(c.matches(ok), "{ok:?}");
        }
        for bad in [
            "", "ABC-DEG", "ABCDE", "ABCDEFF", "ABC_DEF", "ÀBCDEF", "yes", "y",
        ] {
            assert!(!c.matches(bad), "{bad:?}");
        }
        assert!(
            ConfirmationCode::parse("ABC-DE0").is_none(),
            "0 is not in the alphabet"
        );
        assert!(ConfirmationCode::parse("ABCDE").is_none());
    }

    #[test]
    fn plan_ids_are_valid_and_unique() {
        assert_ne!(new_plan_id(1_800_000_000), new_plan_id(1_800_000_000));
    }

    #[test]
    fn reasons_map_to_exit_codes() {
        assert_eq!(Reason::NotApproved.kind(), ErrorKind::SafetyBlocked);
        assert_eq!(Reason::ApprovalExpired.kind(), ErrorKind::PlanExpired);
        assert_eq!(Reason::ApprovalMismatch.kind(), ErrorKind::PlanTampered);
        assert_eq!(Reason::PlanChanged.kind(), ErrorKind::PlanInvalidated);
        assert_eq!(Reason::WrongCode.code(), "wrong_code");
        assert_eq!(
            serde_json::to_value(Reason::ApprovalUsed).unwrap(),
            serde_json::json!("approval_used")
        );
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn none(_: &str) -> bool {
        false
    }

    /// #35: a human's terminal passes; agents, their environment, PTY drivers and an
    /// unreadable ancestry are refused, in that order, with stable codes.
    #[test]
    fn approver_check_refuses_agents_and_pty_drivers() {
        let human = names(&["zsh", "gnome-terminal-server", "systemd"]);
        assert_eq!(check_approver(Ok(&human), &none), Ok(()));
        let tmux = names(&["bash", "tmux: server", "systemd"]);
        assert_eq!(
            check_approver(Ok(&tmux), &none),
            Ok(()),
            "tmux alone is fine"
        );
        let win = names(&["pwsh.exe", "WindowsTerminal.exe", "explorer.exe"]);
        assert_eq!(check_approver(Ok(&win), &none), Ok(()));

        let code = |a: &[String], env: &dyn Fn(&str) -> bool| {
            check_approver(Ok(a), env).unwrap_err().kind.code()
        };
        assert_eq!(
            code(&names(&["zsh", "claude", "zsh"]), &none),
            "agent_process"
        );
        assert_eq!(code(&names(&["sh", "Codex.EXE"]), &none), "agent_process");
        assert_eq!(code(&names(&["sh", "script", "bash"]), &none), "pty_driver");
        assert_eq!(code(&names(&["expect", "zsh"]), &none), "pty_driver");
        // An agent above a PTY driver is reported as the agent.
        let both = names(&["sh", "script", "claude"]);
        let e = check_approver(Ok(&both), &none).unwrap_err();
        assert_eq!(
            (e.kind.code(), e.detail.as_str()),
            ("agent_process", "claude")
        );
        // The agent's environment wins even over a clean ancestry (a tmux session it made).
        let env = |v: &str| v == "CLAUDECODE";
        assert_eq!(code(&tmux, &env), "agent_environment");
        assert!(
            check_approver(Ok(&tmux), &env)
                .unwrap_err()
                .explain()
                .contains("CLAUDECODE")
        );
        // Fail closed.
        let e = check_approver(Err("no /proc"), &none).unwrap_err();
        assert_eq!(e.kind, ApproverSignalKind::AncestryUnknown);
        // Similar names are not matches.
        let similar = names(&["claude-desktop-helper", "scripts", "expected", "zsh"]);
        assert_eq!(check_approver(Ok(&similar), &none), Ok(()));
        assert_eq!(
            Denial::Approver(ApproverSignalKind::PtyDriver).code(),
            "pty_driver"
        );
    }

    #[test]
    fn only_terminal_and_desktop_are_human() {
        assert!(HumanChannel::is_human(ApprovalMethod::CliTty));
        assert!(HumanChannel::is_human(ApprovalMethod::Desktop));
        assert!(!HumanChannel::is_human(ApprovalMethod::CliFlag));
        assert!(!HumanChannel::is_human(ApprovalMethod::CliInteractive));
    }

    // ---- D-34: any change voids an agent plan --------------------------------------------

    use scull_core::{
        Category, FileIdentity, Flags, FsPath, PlanItem, PlanParams, ReasonCode, Regeneration,
        RuleRef, Sizes, TierReason,
    };

    fn cand(tier: Tier, action: Action, bytes: u64) -> Candidate {
        let identity = FileIdentity::new(1, 42);
        let path = "/work/app/target";
        let mut sizes = Sizes::default();
        sizes.reclaimable.bytes = bytes;
        Candidate {
            id: ItemId::derive(identity, path.as_bytes()),
            path: FsPath::from(path),
            identity,
            category: Category::ProjectArtifact,
            artifact: "Cargo build output".into(),
            rule: RuleRef {
                id: "rust.target".into(),
                version: 1,
            },
            project: None,
            tier,
            reasons: vec![TierReason::new(tier, ReasonCode::Regenerable, "test")],
            evidence: vec![],
            sizes,
            flags: Flags::default(),
            regeneration: Regeneration::default(),
            action,
        }
    }

    fn mcp_plan(c: &Candidate) -> Plan {
        Plan::new(
            PlanParams {
                id: PlanId::from_suffix("01JD34").unwrap(),
                scan_id: scull_core::ScanId::from_suffix("01JD34").unwrap(),
                created_unix: 1_800_000_000,
                ttl_secs: None,
                machine_id: "m".into(),
                user: "u".into(),
                origin: PlanOrigin::Mcp,
                operations_watermark: None,
            },
            vec![PlanItem::from_candidate(c).unwrap()],
        )
        .unwrap()
    }

    fn change_of(plan: &Plan, fresh: Candidate) -> Option<Change> {
        check_unchanged(plan, &[fresh])
            .err()
            .map(|mut v| v.remove(0).change)
    }

    #[test]
    fn d34_unchanged_items_pass() {
        let c = cand(Tier::S1, Action::Quarantine, 4096);
        assert!(check_unchanged(&mcp_plan(&c), std::slice::from_ref(&c)).is_ok());
    }

    /// `narrow` keeps these for CLI/desktop plans (lower tier, safer action, new size);
    /// D-34 voids an agent plan for each of them.
    #[test]
    fn d34_changes_narrow_accepts_still_void_an_agent_plan() {
        let plan = mcp_plan(&cand(Tier::S1, Action::Quarantine, 4096));
        // Same inode, contents changed: a different reclaimable size.
        assert_eq!(
            change_of(&plan, cand(Tier::S1, Action::Quarantine, 9000)),
            Some(Change::Size {
                planned: 4096,
                fresh: 9000
            })
        );
        // Tier lowered (S1 -> S0).
        assert_eq!(
            change_of(&plan, cand(Tier::S0, Action::Quarantine, 4096)),
            Some(Change::Tier {
                planned: Tier::S1,
                fresh: Tier::S0
            })
        );
        // Same tier, another proposed action (S1 quarantine -> S1 trash).
        assert_eq!(
            change_of(&plan, cand(Tier::S1, Action::Trash, 4096)),
            Some(Change::Action)
        );
        // Detected by another rule version.
        let mut other = cand(Tier::S1, Action::Quarantine, 4096);
        other.rule.version = 2;
        assert_eq!(change_of(&plan, other), Some(Change::Detection));
        // `narrow`'s skip semantics for CLI/desktop plans are unchanged: it keeps these.
        for fresh in [
            cand(Tier::S1, Action::Quarantine, 9000),
            cand(Tier::S0, Action::Quarantine, 4096),
        ] {
            assert_eq!(narrow(&plan, &[fresh]).executable.len(), 1);
        }
    }

    #[test]
    fn d34_dropped_items_void_an_agent_plan() {
        let plan = mcp_plan(&cand(Tier::S1, Action::Quarantine, 4096));
        assert_eq!(
            check_unchanged(&plan, &[]).unwrap_err()[0].change,
            Change::Dropped(DropReason::Missing)
        );
        let raised = change_of(&plan, cand(Tier::S2, Action::Quarantine, 4096)).unwrap();
        assert!(matches!(
            raised,
            Change::Dropped(DropReason::TierRaised { .. })
        ));
    }
}
