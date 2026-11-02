//! `scull approve [<plan-id>]` (P4-MCP-01, MCP-04, PRD §13.3): the human approves, in their
//! own terminal, a cleanup plan an AI agent prepared through the MCP server.
//! `scull approve --revoke <plan-id>` (#298) withdraws an approval before the agent uses it.
//!
//! * **Interactive terminal only** (FX-MCP-08, T-41): stdin, stdout and stderr must all be
//!   terminals, and the output format human. Anything without a terminal (an agent's shell
//!   tool, a pipe, a script, `echo yes |`) gets `safety_blocked` (exit 5) and nothing is
//!   approved. There is no flag to skip this. The typed code proves the plan was on a screen
//!   someone read; it does not prove a human: a process that drives a pseudo-terminal on the
//!   same host (`expect`, `script`, `tmux`) passes these checks, which is the out-of-scope
//!   case of an agent with unrestricted shell access (PRD §13.4, `docs/THREAT_MODEL.md`
//!   T-41). The desktop dialog (#109) is the preferred approval surface.
//! * **Never elevated**: refused as root/administrator, with no override (#199: agent
//!   approvals are off when running elevated).
//! * Without a plan ID it lists the agent plans awaiting approval and approves nothing.
//! * With one, it shows the **full plan** (every item: tier, action and whether it can be
//!   undone, size, exact path, escaped), its content hash and expiry, then a random code
//!   ([`scull_mcp::approval::ConfirmationCode`]) the user must type back (case, spaces and
//!   hyphens don't matter; paste is fine). A wrong code re-prompts, up to
//!   [`MAX_CODE_ATTEMPTS`] times; `deny` (or an empty line, or end of input) approves nothing.
//! * The approval is recorded through the broker ([`scull_mcp::approval::approve`]): bound to
//!   the plan's content hash, channel `terminal`, approver = this OS user, single use, lapsing
//!   with the plan (at most 15 minutes after the agent prepared it). The agent's
//!   `execute_cleanup` can then run it once; the Executor consumes the approval.
//! * Every attempt that ends without an approval is audited (PRD §13.3 step 5;
//!   [`approval::record_denial`]): `deny`, too many wrong codes, no terminal (when a plan ID
//!   was given and scull's database already exists; nothing is created for it), and a
//!   refused attempt (expired, foreign or unknown plan, already approved). `scull history`
//!   lists them. Not when elevated: an elevated process never writes the user's database.
//! * **Agent-driven terminals (#35)**: a process that drives a pseudo-terminal passes the
//!   terminal checks. Before showing a plan, `scull approve` refuses (`safety_blocked`, exit 5,
//!   audited) when an AI agent's environment variable is set, an AI agent client or a PTY
//!   driver (`script`, `expect`, ...) is among its parent processes, or the parent processes
//!   can't be read ([`approval::check_approver`], [`scull_platform::ancestry()`]). A heuristic
//!   that raises the bar, not a proof of a human: see `docs/APPROVAL_HARDENING.md`.
//! * **`--revoke <plan-id>`**: the same checks (interactive terminal, never elevated, an agent
//!   plan of this machine and user, approved by this user), then
//!   [`approval::revoke`]: the approval is spent in the same atomic update the Executor uses,
//!   so if the agent's execution already started the revocation is refused (`approval_used`)
//!   and if the revocation wins the execution is refused (`approval_revoked`). Audited.
//!   No code to type: revoking only takes a permission away.
//! * **`<plan-id> --run`**: the human approves and runs the plan in one step, in this
//!   terminal ([`run_now`]). The same gates, all before anything is shown (interactive
//!   terminal, never elevated, the #35 agent checks), the same plan and typed code, and a
//!   denial is as final. Once approved, the plan runs exactly as the agent's
//!   `execute_cleanup` would: the approval is checked again, the plan's items are rescanned
//!   and **any change voids it** (D-34; its approval is spent and nothing runs), then the
//!   Executor runs it under the execution lock with per-item revalidation and consumes the
//!   approval atomically. So if the agent calls `execute_cleanup` at the same moment, exactly
//!   one of the two runs it. When it does not run (another operation holds the lock, the
//!   rescan failed, the confirmation could not be written, ...), an approval still unspent is
//!   revoked rather than left for the agent; when the agent's run won, that is what is
//!   reported; when this run started and then failed, it says so, with the items handled and
//!   the operation to review. How it ended is audited ([`approval::record_approve_run`],
//!   `approve_run` in `scull history`: `executed`, `failed_partway`, `approval_used`,
//!   `plan_changed`, `lock_held`, ...), next to the approval (channel `terminal`) and the
//!   operation; the Executor's own refusals during it are attributed to `cli` too. There is no MCP tool, environment
//!   variable or configuration for this: only the flag, typed by the human.

use super::clean::utc_datetime;
use super::exec;
use super::mcp_runner::{ExecutorRunner, RunCaller, RunFailure};
use super::pipeline::{self, CancelBridge, Loaded};
use super::render::action_label;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::style::{Style, tier_style, tier_text};
use scull_core::{ErrorKind, OperationId, Plan, PlanId, Restorability, ScullError};
use scull_engine::paths::is_within;
use scull_engine::{ScanRequest, now_unix};
use scull_mcp::approval::{
    self, Challenge, Denial, HumanChannel, MAX_CODE_ATTEMPTS, PlanSummary, Principal, Reason,
};
use scull_mcp::{RunRequest, RunSummary};
use scull_storage::{Approval, Store};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

/// What the human answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Approved (the recorded approval).
    Approved(Approval),
    /// Typed `deny`, an empty line, or closed the input: nothing was approved.
    Denied,
}

/// `scull approve [<plan-id>] [--run]`.
pub fn run(plan_id: Option<&str>, then_run: bool, ui: &Ui) -> Result<(), ScullError> {
    let interactive = require_interactive(
        ui.format.is_machine(),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
    );
    if let Err(e) = interactive {
        if e.kind == ErrorKind::SafetyBlocked
            && !exec::elevation().blocks_destructive()
            && let Some(id) = plan_id.and_then(|p| PlanId::try_from(p.to_owned()).ok())
        {
            audit_refused_attempt(&id, Denial::NoTerminal);
        }
        return Err(e);
    }
    if exec::elevation().blocks_destructive() {
        return Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            "scull approve refuses to run as root/administrator (or when that can't be told): \
             approve agent plans as your normal user. Nothing was approved.",
        ));
    }
    // #35: an agent driving a pseudo-terminal passes the checks above; refuse what looks like
    // one before anything is shown (a heuristic, see `approval::check_approver`).
    if let Some(id) = plan_id {
        check_terminal_owner(id)?;
    }
    let who = exec::who()?;
    let principal = Principal {
        machine_id: who.machine_id,
        user: who.user,
    };
    let mut store = exec::open_store()?;
    let now = now_unix();
    let color = ui.color_out;
    let Some(id) = plan_id else {
        let list = approval::awaiting(&store, &principal, now).map_err(ScullError::from)?;
        let approved = approval::revocable(&store, &principal, now).map_err(ScullError::from)?;
        let mut text = render_awaiting(&list, now);
        text.push_str(&render_revocable(&approved, now));
        return super::emit_text(&text);
    };
    let id = PlanId::try_from(id.to_owned()).map_err(|_| {
        ScullError::new(
            ErrorKind::Usage,
            format!("{} is not a scull plan ID", escape_for_terminal(id)),
        )
    })?;
    // `--run`: the configuration the run needs is read before the plan is shown, so a broken
    // one fails before anything is approved.
    let setup = if then_run {
        let loaded = pipeline::load_config(&[])?;
        let roots = super::mcp::allowlisted_roots(&loaded, ui);
        Some((loaded, roots))
    } else {
        None
    };
    let challenge = match approval::begin(&store, &id, &principal, now) {
        Ok(c) => c,
        Err(r) => {
            let hash = approval::recorded_hash(&store, &id);
            audit(
                &mut store,
                &id,
                hash.as_ref(),
                Denial::Refused(r.reason),
                &principal,
            );
            return Err(r.into());
        }
    };
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut out = io::stdout().lock();
    let answer = ask_to(
        &mut store, &challenge, &principal, &mut input, &mut out, color, then_run,
    );
    drop(out);
    let answer = match answer {
        Ok(a) => a,
        // `--run`: an approval recorded before the error (the confirmation could not be
        // written) is never left live for the agent.
        Err(e) if then_run && store.get_approval(&id).ok().flatten().is_some() => {
            let code = super::json_name(&e.kind);
            return Err(not_run(&mut store, challenge.plan(), &principal, &code, e));
        }
        Err(e) => return Err(e),
    };
    match answer {
        Answer::Approved(_) => match setup {
            Some((loaded, roots)) => run_now(
                &mut store,
                challenge.plan(),
                &principal,
                &loaded,
                &roots,
                ui,
            ),
            None => Ok(()),
        },
        Answer::Denied => Err(ScullError::new(
            ErrorKind::Cancelled,
            "not approved; the agent's plan will expire unused",
        )),
    }
}

/// `scull approve --revoke <plan-id>` (#298).
pub fn run_revoke(plan_id: &str, ui: &Ui) -> Result<(), ScullError> {
    require_interactive(
        ui.format.is_machine(),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
    )?;
    if exec::elevation().blocks_destructive() {
        return Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            "scull approve --revoke refuses to run as root/administrator (or when that can't be \
             told): run it as your normal user. Nothing was changed.",
        ));
    }
    let id = PlanId::try_from(plan_id.to_owned()).map_err(|_| {
        ScullError::new(
            ErrorKind::Usage,
            format!("{} is not a scull plan ID", escape_for_terminal(plan_id)),
        )
    })?;
    let who = exec::who()?;
    let principal = Principal {
        machine_id: who.machine_id,
        user: who.user,
    };
    let mut store = exec::open_store()?;
    let text = revoke(&mut store, &id, &principal, now_unix())?;
    super::emit_text(&text)
}

/// Revoke the approval of `id` through the terminal channel; the message to show.
pub fn revoke(
    store: &mut Store,
    id: &PlanId,
    who: &Principal,
    now: i64,
) -> Result<String, ScullError> {
    let r = approval::revoke(store, id, HumanChannel::Terminal, who, now)?;
    Ok(format!(
        "Revoked the approval of plan {}. The agent can't run it; nothing was changed. See \
         `scull history`.\n",
        r.plan_id
    ))
}

/// Record a denied approval attempt through the terminal (best effort: the outcome stands
/// either way; a failure to record is reported on stderr).
fn audit(
    store: &mut Store,
    id: &PlanId,
    hash: Option<&scull_core::ContentHash>,
    denial: Denial,
    who: &Principal,
) {
    if let Err(e) = approval::record_denial(
        store,
        id,
        hash,
        denial,
        HumanChannel::Terminal,
        who,
        now_unix(),
    ) {
        let _ = writeln!(
            io::stderr(),
            "scull: could not record this in the audit log: {}",
            e.message
        );
    }
}

/// #35: refuse (and audit) when the terminal looks driven by an AI agent or an automation
/// tool ([`approval::check_approver`] over this process's ancestry and environment). Fails
/// closed when the ancestry can't be read.
fn check_terminal_owner(plan_id: &str) -> Result<(), ScullError> {
    let ancestry =
        scull_platform::ancestry().map(|a| a.iter().flat_map(|p| p.names()).collect::<Vec<_>>());
    let why = ancestry.as_ref().err().map(ToString::to_string);
    let names = match &ancestry {
        Ok(n) => Ok(n.as_slice()),
        Err(_) => Err(why.as_deref().unwrap_or("unknown")),
    };
    let env_set = |v: &str| std::env::var_os(v).is_some_and(|x| !x.is_empty());
    let Err(signal) = approval::check_approver(names, &env_set) else {
        return Ok(());
    };
    if let Ok(id) = PlanId::try_from(plan_id.to_owned()) {
        audit_refused_attempt(&id, Denial::Approver(signal.kind));
    }
    Err(ScullError::new(
        ErrorKind::SafetyBlocked,
        format!(
            "scull approve refuses to approve here: {} ({}). Approving must be done by you, in a \
             terminal you opened yourself, not by an AI agent or through `script`, `expect` or a \
             session an agent started. This check is a heuristic; see \
             docs/APPROVAL_HARDENING.md. Nothing was approved.",
            escape_for_terminal(&signal.explain()),
            signal.kind.code()
        ),
    ))
}

/// Refused attempts (no terminal, FX-MCP-08; a terminal that looks agent-driven, #35) are
/// audited too, but only into a database and machine ID that already exist: nothing is
/// created for a refused attempt (not even the machine ID file). Repeats fold into one entry
/// (bounded).
fn audit_refused_attempt(id: &PlanId, denial: Denial) {
    let Ok(dirs) = exec::dirs() else { return };
    let Ok(Some(machine_id)) = scull_platform::existing_machine_id(&dirs.data) else {
        return;
    };
    let Ok(user) = scull_platform::current_user() else {
        return;
    };
    let Ok(mut store) = crate::db::open_current(&crate::db::db_path(&dirs.data)) else {
        return;
    };
    let principal = Principal { machine_id, user };
    let hash = approval::recorded_hash(&store, id);
    audit(&mut store, id, hash.as_ref(), denial, &principal);
}

/// FX-MCP-08: approving needs a human at an interactive terminal. `machine` is a JSON/NDJSON
/// output format; the rest say whether stdin, stdout and stderr are terminals.
pub fn require_interactive(
    machine: bool,
    stdin: bool,
    stdout: bool,
    stderr: bool,
) -> Result<(), ScullError> {
    if machine {
        return Err(ScullError::new(
            ErrorKind::Usage,
            "scull approve is interactive only: it has no JSON output. Run it in your own \
             terminal. Nothing was approved.",
        ));
    }
    if !(stdin && stdout && stderr) {
        return Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            "scull approve needs an interactive terminal (stdin, stdout and stderr): pipes, \
             scripts and agent tools without a terminal can't approve a cleanup (MCP-04). Run \
             `scull approve <plan-id>` yourself in a terminal. Nothing was approved.",
        ));
    }
    Ok(())
}

/// Show the challenged plan and its code on `out`, read answers from `input` until the code
/// matches (approved), `deny`/empty/end of input (denied), or [`MAX_CODE_ATTEMPTS`] wrong
/// codes (refused with `safety_blocked`). Every outcome but an approval is audited.
pub fn ask(
    store: &mut Store,
    challenge: &Challenge,
    who: &Principal,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    color: bool,
) -> Result<Answer, ScullError> {
    ask_to(store, challenge, who, input, out, color, false)
}

/// [`ask`], saying whether the approval is for the agent (`then_run` false) or to run the
/// plan right away in this terminal (`scull approve --run`).
pub fn ask_to(
    store: &mut Store,
    challenge: &Challenge,
    who: &Principal,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    color: bool,
    then_run: bool,
) -> Result<Answer, ScullError> {
    let io_err = |e: io::Error| ScullError::from_io(&e);
    let plan = challenge.plan();
    write!(out, "{}", render_plan(challenge, now_unix(), color)).map_err(io_err)?;
    let code = challenge.code().display();
    for attempt in 1..=MAX_CODE_ATTEMPTS {
        write!(
            out,
            "Type {} to approve{}, or `deny`: ",
            Style::Bold.paint(&code, color),
            if then_run { " and run it now" } else { "" }
        )
        .map_err(io_err)?;
        out.flush().map_err(io_err)?;
        let mut line = String::new();
        let n = input.read_line(&mut line).map_err(io_err)?;
        let answer = line.trim();
        if n == 0 || answer.is_empty() || answer.eq_ignore_ascii_case("deny") {
            audit(
                store,
                plan.id(),
                Some(plan.content_hash()),
                Denial::Denied,
                who,
            );
            writeln!(out, "\nNot approved. Nothing will be cleaned.").map_err(io_err)?;
            return Ok(Answer::Denied);
        }
        match approval::approve(
            store,
            challenge,
            answer,
            HumanChannel::Terminal,
            who,
            now_unix(),
        ) {
            Ok(a) if then_run => {
                writeln!(
                    out,
                    "Approved plan {}. Checking that nothing in it changed, then running it...",
                    a.plan_id
                )
                .map_err(io_err)?;
                return Ok(Answer::Approved(a));
            }
            Ok(a) => {
                writeln!(
                    out,
                    "Approved plan {} (single use, until {}). The agent can now run it once \
                     with execute_cleanup; see the result with `scull history`.",
                    a.plan_id,
                    utc_datetime(a.expires_at)
                )
                .map_err(io_err)?;
                return Ok(Answer::Approved(a));
            }
            Err(r) if r.reason == Reason::WrongCode && attempt < MAX_CODE_ATTEMPTS => {
                writeln!(
                    out,
                    "That is not the code ({} attempt(s) left).",
                    MAX_CODE_ATTEMPTS - attempt
                )
                .map_err(io_err)?;
            }
            Err(r) if r.reason == Reason::WrongCode => {
                audit(
                    store,
                    plan.id(),
                    Some(plan.content_hash()),
                    Denial::WrongCode,
                    who,
                );
                return Err(ScullError::new(
                    ErrorKind::SafetyBlocked,
                    format!(
                        "the code was typed wrong {MAX_CODE_ATTEMPTS} times, so plan {} is now \
                         denied: it can never be approved, and the agent must prepare a new plan \
                         if you still want the cleanup.",
                        plan.id()
                    ),
                ));
            }
            Err(r) => {
                audit(
                    store,
                    plan.id(),
                    Some(plan.content_hash()),
                    Denial::Refused(r.reason),
                    who,
                );
                return Err(r.into());
            }
        }
    }
    Ok(Answer::Denied)
}

/// `scull approve <plan-id> --run`, after the human approved `plan` in this terminal: run it
/// now, exactly as the agent's `execute_cleanup` would (`scull_mcp`'s execute path): the
/// approval is checked again, the plan's items must lie in the allowlisted `roots`, a fresh
/// scan must find every item unchanged (D-34: otherwise the plan is void and its approval
/// spent), then [`ExecutorRunner`] runs it under the execution lock and consumes the approval
/// atomically, so a concurrent `execute_cleanup` and this run can't both execute it.
///
/// What it reports is what happened: run here (`executed`); started here and stopped
/// part-way (`failed_partway`, with the items handled and the operation to review); run by
/// the agent first (`approval_used`); or not run at all, in which case an approval still
/// unspent is revoked (the human asked to run it here, not to hand it to the agent). Each is
/// audited ([`approval::record_approve_run`]).
pub fn run_now(
    store: &mut Store,
    plan: &Plan,
    who: &Principal,
    loaded: &Loaded,
    roots: &[PathBuf],
    ui: &Ui,
) -> Result<(), ScullError> {
    let id = plan.id();
    let auth = match approval::authorize(store, id, who, now_unix()) {
        Ok(a) => a,
        Err(r) => {
            let e = ScullError::new(r.reason.kind(), r.message);
            return Err(not_run(store, plan, who, r.reason.code(), e));
        }
    };
    let plan = &auth.plan;
    let exec_roots = match plan_roots(roots, plan) {
        Ok(r) => r,
        Err(e) => return Err(not_run(store, plan, who, "outside_allowlist", e)),
    };
    let engine = match pipeline::engine(loaded, roots, false, ui) {
        Ok(e) => e,
        Err(e) => return Err(not_run(store, plan, who, &super::json_name(&e.kind), e)),
    };
    let bridge = CancelBridge::start();
    let mut req = ScanRequest::full().history(false);
    req.threads = loaded.config.scan.threads as usize;
    let rep = match engine.scan(&exec_roots, &req, bridge.token(), |_| {}) {
        Ok(r) if r.cancelled => {
            let e = ScullError::new(ErrorKind::Cancelled, "cancelled during the re-check");
            return Err(not_run(store, plan, who, "cancelled", e));
        }
        Ok(r) => r,
        Err(e) => return Err(not_run(store, plan, who, &super::json_name(&e.kind), e)),
    };
    // D-34: any changed item voids the whole agent plan.
    if let Err(changed) = approval::check_unchanged(plan, &rep.candidates) {
        let voided = match approval::void(store, plan, now_unix()) {
            Ok(v) => v,
            Err(r) => return Err(not_run(store, plan, who, "store", r.into())),
        };
        // Already spent: the agent may have run it while this rescanned.
        if !voided && let Some(op) = ran_elsewhere(store, plan) {
            audit_run(store, plan, Reason::ApprovalUsed.code(), who);
            return Err(agent_ran(id, &op));
        }
        audit_run(store, plan, Reason::PlanChanged.code(), who);
        let list: Vec<String> = changed
            .iter()
            .map(|c| format!("{} ({})", c.item_id, c.change.code()))
            .collect();
        return Err(ScullError::new(
            ErrorKind::PlanInvalidated,
            format!(
                "{} item(s) of plan {id} changed on disk since the agent prepared it: {}. The plan \
                 is void{}; nothing was run. Ask the agent for a new plan if you still want this \
                 cleanup.",
                changed.len(),
                list.join(", "),
                if voided {
                    " and its approval spent"
                } else {
                    ""
                }
            ),
        ));
    }
    let runner = ExecutorRunner {
        quarantine: exec::quarantine_settings(&loaded.config.quarantine),
        trash: exec::trash_locations(),
        caller: RunCaller::HumanRun,
    };
    // Another operation holding the execution lock may be the agent's `execute_cleanup`
    // between taking the lock and claiming this approval: wait briefly for it to settle
    // rather than revoke the approval under it. If the wait ends first, the revocation and
    // the agent's claim are one atomic update apart: exactly one of them wins, and
    // `not_run` reports which.
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    let result = loop {
        let r = runner.run_tracked(RunRequest {
            store,
            plan,
            approval: &auth.approval,
            engine: &engine,
            roots: &exec_roots,
            principal: who,
            cancel: bridge.token(),
        });
        match r {
            Err(f)
                if f.operation_id.is_none()
                    && f.error.kind == ErrorKind::LockHeld
                    && std::time::Instant::now() < deadline
                    && !crate::cancel::is_cancelled()
                    && !approval_spent(store, id) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            other => break other,
        }
    };
    drop(bridge);
    let summary = match result {
        Ok(s) => s,
        Err(RunFailure {
            error,
            operation_id: Some(op),
        }) => {
            // This run started the operation (its approval consumed here) and then failed:
            // items may have been handled.
            let handled = store
                .audit_items(&op)
                .map(|a| a.iter().filter(|i| i.outcome.is_done()).count())
                .unwrap_or(0);
            audit_run(store, plan, "failed_partway", who);
            return Err(ScullError::new(
                error.kind,
                format!(
                    "the run of plan {id} started here as operation {op} and stopped part-way: {}. \
                     {handled} item(s) were handled before it stopped; see `scull history {op}` \
                     (quarantined items can be put back with `scull restore {op}`).",
                    error.message.trim_end_matches('.')
                ),
            ));
        }
        Err(RunFailure {
            error,
            operation_id: None,
        }) => {
            let code = super::json_name(&error.kind);
            return Err(not_run(store, plan, who, &code, error));
        }
    };
    audit_run(store, plan, "executed", who);
    super::emit_text(&render_run(plan, &summary))?;
    let failed = summary.items.iter().filter(|i| !i.succeeded).count();
    if summary.cancelled {
        return Err(ScullError::new(
            ErrorKind::Cancelled,
            "cancelled: the remaining items were not run",
        ));
    }
    if failed > 0 {
        return Err(ScullError::new(
            ErrorKind::PartialFailure,
            format!(
                "{failed} item(s) of plan {id} were not cleaned; see `scull history {}`",
                summary.operation_id
            ),
        ));
    }
    Ok(())
}

/// How long `--run` waits for another operation's execution lock before giving up.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether plan `id`'s approval was spent (used by an execution, voided or revoked).
fn approval_spent(store: &Store, id: &PlanId) -> bool {
    store
        .get_approval(id)
        .ok()
        .flatten()
        .is_some_and(|a| a.used_at.is_some())
}

/// The operation that ran `plan` when its approval was spent by one (called only where this
/// `--run` started none: the agent's `execute_cleanup`).
fn ran_elsewhere(store: &Store, plan: &Plan) -> Option<OperationId> {
    if !approval_spent(store, plan.id()) {
        return None;
    }
    approval::operations_of(store, plan)
        .ok()?
        .first()
        .map(|o| o.operation_id.clone())
}

fn agent_ran(id: &PlanId, op: &OperationId) -> ScullError {
    ScullError::new(
        Reason::ApprovalUsed.kind(),
        format!(
            "plan {id} was run by the agent's execute_cleanup first, as operation {op} (approvals \
             are single use): see `scull history {op}`. Nothing was run here."
        ),
    )
}

/// The agent's `execute_cleanup` spent the approval but no operation of it is recorded: it
/// either is still starting the run or found the plan changed and voided it.
fn used_by_agent(id: &PlanId) -> ScullError {
    ScullError::new(
        Reason::ApprovalUsed.kind(),
        format!(
            "the approval of plan {id} was used by the agent's execute_cleanup first: it either \
             ran the plan or found it changed and voided it; see `scull history`. Nothing was \
             run here."
        ),
    )
}

/// `--run` did not start `plan` (error `e`, audited as `outcome`). Reports the agent's run if
/// the agent's `execute_cleanup` spent the approval; otherwise revokes the approval if it is
/// still unspent, so it is never left live for the agent after the human asked to run it
/// here (the revocation and an agent's claim are atomic: if the claim won, that is reported).
fn not_run(
    store: &mut Store,
    plan: &Plan,
    who: &Principal,
    outcome: &str,
    e: ScullError,
) -> ScullError {
    // The execution lock is held and the approval is spent (not revoked): the agent's
    // `execute_cleanup` used it. It claims the approval before it records its operation, so
    // give it a moment to record one, then report it (#118). Without an operation the agent
    // may also have voided the plan (its rescan found a change, D-34): then nothing is claimed
    // to have run.
    if e.kind == ErrorKind::LockHeld
        && approval_spent(store, plan.id())
        && matches!(store.approval_revoked(plan.id()), Ok(false))
    {
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        let op = loop {
            if let Some(op) = ran_elsewhere(store, plan) {
                break Some(op);
            }
            if std::time::Instant::now() >= deadline || crate::cancel::is_cancelled() {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        if let Some(op) = op {
            audit_run(store, plan, Reason::ApprovalUsed.code(), who);
            return agent_ran(plan.id(), &op);
        }
        audit_run(store, plan, outcome, who);
        return used_by_agent(plan.id());
    }
    if let Some(op) = ran_elsewhere(store, plan) {
        audit_run(store, plan, Reason::ApprovalUsed.code(), who);
        return agent_ran(plan.id(), &op);
    }
    let unspent =
        store.get_approval(plan.id()).ok().flatten().is_some() && !approval_spent(store, plan.id());
    let revoked = unspent
        && approval::revoke(store, plan.id(), HumanChannel::Terminal, who, now_unix()).is_ok();
    if unspent
        && !revoked
        && let Some(op) = ran_elsewhere(store, plan)
    {
        audit_run(store, plan, Reason::ApprovalUsed.code(), who);
        return agent_ran(plan.id(), &op);
    }
    audit_run(store, plan, outcome, who);
    let tail = if revoked {
        "; the approval was revoked, so the agent can't use it either".to_owned()
    } else if unspent {
        format!(
            "; the approval could not be revoked: run `scull approve --revoke {}` before the \
             agent uses it",
            plan.id()
        )
    } else {
        String::new()
    };
    ScullError::new(
        e.kind,
        format!(
            "{}. Nothing was run{tail}.",
            e.message.trim_end_matches('.')
        ),
    )
}

/// The allowlisted roots holding `plan`'s items (each item must lie in one), like the MCP
/// server's own check before `execute_cleanup`.
fn plan_roots(roots: &[PathBuf], plan: &Plan) -> Result<Vec<PathBuf>, ScullError> {
    let mut used: Vec<PathBuf> = Vec::new();
    for item in plan.items() {
        let p = item.path.to_path_buf();
        let Some(root) = roots.iter().find(|r| is_within(&p, r)) else {
            return Err(ScullError::new(
                ErrorKind::SafetyBlocked,
                format!(
                    "{} is outside scan.roots (without broad roots), which is all an agent plan \
                     may touch",
                    escape_for_terminal(item.path.as_str())
                ),
            ));
        };
        if !used.contains(root) {
            used.push(root.clone());
        }
    }
    Ok(used)
}

/// Record how `--run` ended (best effort, reported on stderr if it can't be recorded).
fn audit_run(store: &mut Store, plan: &Plan, outcome: &str, who: &Principal) {
    if let Err(e) = approval::record_approve_run(store, plan, outcome, who, now_unix()) {
        let _ = writeln!(
            io::stderr(),
            "scull: could not record this in the audit log: {}",
            e.message
        );
    }
}

/// What `--run` did: one line per item, then the operation.
fn render_run(plan: &Plan, s: &RunSummary) -> String {
    let mut t = String::new();
    for i in &s.items {
        let path = plan
            .items()
            .iter()
            .find(|p| p.item_id == i.item_id)
            .map(|p| escape_for_terminal(p.path.as_str()))
            .unwrap_or_default();
        let why = i
            .reason
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        t.push_str(&format!("  {:<15} {path}{why}\n", i.outcome));
    }
    let done = s.items.iter().filter(|i| i.succeeded).count();
    t.push_str(&format!(
        "Ran plan {} as operation {}: {done} item(s) done, {} not{}.",
        plan.id(),
        s.operation_id,
        s.items.len() - done,
        if s.cancelled { " (cancelled)" } else { "" }
    ));
    if let Some(d) = s.measured_delta_bytes.filter(|d| *d > 0) {
        t.push_str(&format!(" Free space grew by {}.", format_bytes(d as u64)));
    }
    t.push_str(&format!(
        " Review it with `scull history {0}`; restore quarantined items with `scull restore {0}`.\n",
        s.operation_id
    ));
    t
}

/// The full plan as shown before the code.
pub fn render_plan(challenge: &Challenge, now: i64, color: bool) -> String {
    let plan = challenge.plan();
    let s = PlanSummary::of(plan);
    let mut t = String::new();
    t.push_str(&Style::Bold.paint("An AI agent asks to clean up these items", color));
    t.push_str(" (prepared through scull's MCP server; approving is up to you).\n");
    if let Some(c) = challenge.client() {
        t.push_str(&format!(
            "Prepared through the MCP client {} (the name it gave itself; not verified).\n",
            Style::Bold.paint(&escape_for_terminal(&c.display()), color)
        ));
    }
    t.push('\n');
    for (n, i) in plan.items().iter().enumerate() {
        let undo = match i.action.restorability() {
            Restorability::Restorable => "restorable with `scull restore`",
            Restorability::ViaTrash => "restorable from the Trash",
            Restorability::Regenerate => "not undoable: regenerated by its tool",
            _ => "not undoable",
        };
        t.push_str(&format!(
            "{:>3}. {:<15} {:<10} {:>10}  {}\n     {}\n",
            n + 1,
            tier_style(i.tier).paint(&tier_text(i.tier), color),
            action_label(&i.action),
            format_bytes(i.reclaimable_bytes),
            escape_for_terminal(i.path.as_str()),
            Style::Dim.paint(undo, color),
        ));
    }
    let mins = (plan.expires_unix() - now).max(0) / 60;
    t.push_str(&format!(
        "\n{} item(s) (S0 {}, S1 {}, S2 {}), about {} reclaimable. S0 items are deleted; S1/S2 \
         items are moved to quarantine. Every item is re-checked right before it is touched, \
         and the whole plan is void if any of them changed.\n",
        s.item_count,
        s.items_by_tier[0],
        s.items_by_tier[1],
        s.items_by_tier[2],
        format_bytes(s.reclaimable_bytes)
    ));
    t.push_str(&format!(
        "Plan {}  hash {}  expires {} (in {mins} min)\n",
        plan.id(),
        approval::short_hash(plan.content_hash()),
        utc_datetime(plan.expires_unix()),
    ));
    t.push_str("Approve only if you asked your agent for this cleanup.\n\n");
    t
}

/// Approved agent plans not yet run (empty when there are none).
pub fn render_revocable(list: &[PlanSummary], now: i64) -> String {
    if list.is_empty() {
        return String::new();
    }
    let mut t = format!("\n{} approved agent plan(s) not yet run:\n", list.len());
    for p in list {
        t.push_str(&format!(
            "  {}  {} item(s), {}, approval lapses in {} min\n",
            p.plan_id,
            p.item_count,
            format_bytes(p.reclaimable_bytes),
            (p.expires_at - now).max(0) / 60
        ));
    }
    t.push_str("Withdraw an approval with `scull approve --revoke <plan-id>`.\n");
    t
}

/// The agent plans awaiting approval.
pub fn render_awaiting(list: &[PlanSummary], now: i64) -> String {
    if list.is_empty() {
        return "No agent plans are awaiting approval.\n".to_owned();
    }
    let mut t = format!("{} agent plan(s) awaiting approval:\n", list.len());
    for p in list {
        let via = p
            .client
            .as_deref()
            .map(|c| format!(", via {}", escape_for_terminal(c)))
            .unwrap_or_default();
        t.push_str(&format!(
            "  {}  {} item(s), {}, expires in {} min{via}\n",
            p.plan_id,
            p.item_count,
            format_bytes(p.reclaimable_bytes),
            (p.expires_at - now).max(0) / 60
        ));
    }
    t.push_str("Review and approve one with `scull approve <plan-id>`.\n");
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fx_mcp_08_refuses_without_an_interactive_terminal() {
        assert!(require_interactive(false, true, true, true).is_ok());
        for (stdin, stdout, stderr) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            let e = require_interactive(false, stdin, stdout, stderr).unwrap_err();
            assert_eq!(e.kind, ErrorKind::SafetyBlocked);
            assert!(e.message.contains("Nothing was approved"), "{}", e.message);
        }
        let e = require_interactive(true, true, true, true).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
    }

    #[test]
    fn awaiting_list_names_the_command() {
        assert!(render_awaiting(&[], 0).contains("No agent plans"));
    }
}
