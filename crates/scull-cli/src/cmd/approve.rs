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

use super::clean::utc_datetime;
use super::exec;
use super::render::action_label;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::style::{Style, tier_style, tier_text};
use scull_core::{ErrorKind, PlanId, Restorability, ScullError};
use scull_engine::now_unix;
use scull_mcp::approval::{
    self, Challenge, Denial, HumanChannel, MAX_CODE_ATTEMPTS, PlanSummary, Principal, Reason,
};
use scull_storage::{Approval, Store};
use std::io::{self, BufRead, IsTerminal, Write};

/// What the human answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Approved (the recorded approval).
    Approved(Approval),
    /// Typed `deny`, an empty line, or closed the input: nothing was approved.
    Denied,
}

/// `scull approve [<plan-id>]`.
pub fn run(plan_id: Option<&str>, ui: &Ui) -> Result<(), ScullError> {
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
    match ask(
        &mut store, &challenge, &principal, &mut input, &mut out, color,
    )? {
        Answer::Approved(_) => Ok(()),
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
    let io_err = |e: io::Error| ScullError::from_io(&e);
    let plan = challenge.plan();
    write!(out, "{}", render_plan(challenge, now_unix(), color)).map_err(io_err)?;
    let code = challenge.code().display();
    for attempt in 1..=MAX_CODE_ATTEMPTS {
        write!(
            out,
            "Type {} to approve, or `deny`: ",
            Style::Bold.paint(&code, color)
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
                        "the code was typed wrong {MAX_CODE_ATTEMPTS} times; nothing was \
                         approved. Run `scull approve {}` again to retry.",
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

/// The full plan as shown before the code.
pub fn render_plan(challenge: &Challenge, now: i64, color: bool) -> String {
    let plan = challenge.plan();
    let s = PlanSummary::of(plan);
    let mut t = String::new();
    t.push_str(&Style::Bold.paint("An AI agent asks to clean up these items", color));
    t.push_str(" (prepared through scull's MCP server; approving is up to you).\n\n");
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
        t.push_str(&format!(
            "  {}  {} item(s), {}, expires in {} min\n",
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
