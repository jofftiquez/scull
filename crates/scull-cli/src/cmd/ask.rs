//! `scull ask` (P5-AI-01, PRD AI-05, D-13): an optional bring-your-own-model assistant over the
//! read-only tools.
//!
//! * **Off by default**: refuses (exit 2) until `ask.toml` in scull's config folder says
//!   `enabled = true` (or `SCULL_ASK_ENABLED=1`). `--show-request` works while off and sends
//!   nothing.
//! * **Consent per provider and endpoint**: the first question to one shows
//!   [`scull_ask::disclosure`] and asks on the terminal (or takes `--yes`); without either it
//!   refuses (exit 2) and sends nothing. Consent is recorded in scull's state folder
//!   ([`scull_ask::CONSENT_FILE`]: provider, endpoint, notice version; never the key).
//! * **Read-only**: the model gets exactly the tools `scull mcp serve` offers minus
//!   `register_worktree`, over `scan.roots` with the `[mcp]` redaction settings. stdout is the
//!   answer (escaped for the terminal); the tools it reads are listed on stderr.
//! * **Preparing plans (AI-05), opt-in**: with `[mcp] destructive = true` in the user config
//!   file and scull not elevated (MCP's gates), the model may also prepare cleanup plans
//!   (`prepare_cleanup` and the read-only plan views; never approval or execution). Each plan
//!   is an agent plan from client `scull ask`, limited by `mcp.max_pending_plans` and
//!   `mcp.prepare_per_minute`. After the answer, scull itself (not the model) prints every
//!   prepared plan's ID and the `scull approve <plan-id>` command, and that nothing changed.
//!   The consent notice says so, and read-only consent doesn't cover it.

use super::mcp::build_server;
use super::{exec, pipeline};
use crate::cancel;
use crate::cli::AskArgs;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::style::Style;
use scull_ask::{
    AskConfig, CONFIG_FILE, CONSENT_FILE, ConsentLog, Event, PreparedPlan, Session, Stop,
    UreqTransport,
};
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_mcp::ScullMcp;
use scull_mcp::approval::Principal;
use scull_platform::app_dirs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

/// Setup notes shown by `scull ask --help`.
pub const AFTER_HELP: &str = "\
Setup (the assistant is off by default). Create ask.toml in scull's config folder, next to
config.toml (`scull doctor` shows where):

  enabled = true
  provider = \"anthropic\"        # or \"openai\" for any OpenAI-compatible endpoint
  model = \"claude-opus-5-5\"     # the default for anthropic
  # base_url = \"http://localhost:11434/v1\"   # e.g. Ollama or LM Studio, with provider = \"openai\"
  # api_key_env = \"MY_KEY_VAR\"  # default: ANTHROPIC_API_KEY or OPENAI_API_KEY
  # effort = \"low\"              # anthropic only: low, medium, high, xhigh, max
  # max_turns = 10              # most model calls per question
  # fallback = false            # anthropic: don't retry a declined request on Anthropic's
  #                             # recommended fallback model (on by default, api.anthropic.com)

The API key is read from the environment and is never stored or logged by scull. Environment
overrides: SCULL_ASK_ENABLED, SCULL_ASK_PROVIDER, SCULL_ASK_MODEL, SCULL_ASK_BASE_URL.
Endpoints must use https:// unless they are on this machine.

Before the first question to a provider and endpoint, scull shows what will be sent and asks
for your consent. The model calls scull's read-only tools (the same ones `scull mcp serve`
offers, without register_worktree): it cannot delete, move or change files, or approve or run
a cleanup. Paths are redacted as configured in [mcp] (redact_home, hash_names).

With `[mcp] destructive = true` in config.toml (and scull not running elevated), the model may
also prepare a cleanup plan when you ask it to free space. Preparing changes nothing: scull
prints the plan ID afterwards, and the plan runs only after you review and approve it yourself
with `scull approve <plan-id>`. Limits: [mcp] max_pending_plans, prepare_per_minute.";

/// `scull ask`.
pub fn run(args: &AskArgs, ui: &Ui) -> Result<(), ScullError> {
    if ui.format.is_machine() {
        return Err(ScullError::new(
            ErrorKind::Usage,
            "scull ask prints a prose answer; --json and --ndjson are not supported (use the \
             read-only commands or `scull mcp serve` for structured data)",
        ));
    }
    let question = args.question.join(" ").trim().to_owned();
    if question.is_empty() {
        return Err(ScullError::new(ErrorKind::Usage, "the question is empty"));
    }
    let dirs = app_dirs().ok();
    let cfg = load_ask_config(dirs.as_ref().map(|d| d.config.join(CONFIG_FILE)).as_deref())?;
    let loaded = pipeline::load_config(&[])?;
    let mcp = &loaded.config.mcp;
    let server = ask_server(&loaded, dirs.as_ref().map(|d| d.data.clone()), ui)?;
    let can_prepare = server.in_process_plan_tools_enabled();
    let notice = scull_ask::disclosure(&cfg, mcp.redact_home, mcp.hash_names, can_prepare);

    if args.show_request {
        let transport = UreqTransport::new(cfg.timeout, false);
        let body = Session::new(&cfg, &transport, &server).first_request(&question);
        let state = if cfg.enabled { "on" } else { "off" };
        let text = format!(
            "{notice}\n\nscull ask is {state}. First request (nothing was sent):\n\n{}",
            scull_ask::render_request(&cfg, &body)
        );
        return print_stdout(&escape_lines(&text));
    }

    cfg.ready()?;
    consent(
        &cfg,
        &notice,
        args.yes,
        dirs.as_ref().map(|d| d.state.as_path()),
        can_prepare,
    )?;
    cancel::check()?;

    let transport = UreqTransport::new(cfg.timeout, !cfg.endpoint.is_loopback());
    let session = Session::new(&cfg, &transport, &server);
    let bridge = pipeline::CancelBridge::start();
    let status = |msg: &str| {
        if !ui.quiet {
            let line = escape_for_terminal(msg);
            let _ = writeln!(io::stderr(), "{}", Style::Dim.paint(&line, ui.color_err));
        }
    };
    status(&format!(
        "scull ask: {} ({}) at {}",
        cfg.provider.label(),
        cfg.model,
        cfg.endpoint.origin()
    ));
    let mut on_event = |e: Event<'_>| match e {
        Event::Asking { turn } if turn > 1 => status(&format!("  asking the model (turn {turn})")),
        Event::Asking { .. } => {}
        Event::ToolCall { name, args } => {
            let a = serde_json::Value::Object(args.clone()).to_string();
            if a == "{}" {
                status(&format!("  reading: {name}"));
            } else {
                status(&format!("  reading: {name} {a}"));
            }
        }
        Event::ToolDone { name, is_error } if is_error => {
            status(&format!("  ({name} was refused or failed)"));
        }
        Event::ToolDone { .. } => {}
    };
    let result = session.run(&question, bridge.token(), &mut on_event);
    drop(bridge);
    if cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    let answer = result?;
    if !answer.text.is_empty() {
        print_stdout(&format!("{}\n", escape_lines(&answer.text)))?;
    }
    if !answer.prepared_plans.is_empty() {
        print_stdout(&prepared_notice(&answer.prepared_plans))?;
    }
    status(&format!(
        "{} model call(s), {} tool call(s); {} input + {} cached input, {} output tokens",
        answer.turns,
        answer.tool_calls.len(),
        answer.usage.input_tokens,
        answer.usage.cache_read_tokens,
        answer.usage.output_tokens
    ));
    match answer.stop {
        Stop::Done | Stop::ToolUse => Ok(()),
        Stop::MaxTokens => {
            pipeline::warn(
                ui,
                "the answer was cut off at max_tokens (raise max_tokens in ask.toml)",
            );
            Ok(())
        }
        Stop::Refusal(category) => Err(ScullError::new(
            ErrorKind::Io,
            match category {
                Some(c) => format!("the model declined to answer (category: {c})"),
                None => "the model declined to answer".to_owned(),
            },
        )),
        Stop::Other(reason) if reason == "turn_limit" => Err(ScullError::new(
            ErrorKind::Io,
            format!(
                "the model did not finish within {} model calls (raise max_turns in ask.toml, \
                 or ask a narrower question)",
                cfg.max_turns
            ),
        )),
        Stop::Other(reason) => {
            pipeline::warn(ui, &format!("the model stopped early ({reason})"));
            Ok(())
        }
    }
}

/// The in-process tools for the model: the read-only server, plus plan preparation when the
/// user opted in (`[mcp] destructive = true` in the config file) and scull is not elevated.
fn ask_server(
    loaded: &pipeline::Loaded,
    data_dir: Option<std::path::PathBuf>,
    ui: &Ui,
) -> Result<ScullMcp, ScullError> {
    let server = build_server(loaded, data_dir, ui)?;
    if !loaded.config.mcp.destructive {
        return Ok(server);
    }
    if exec::elevation().blocks_destructive() {
        pipeline::warn(
            ui,
            "plan preparation is never offered to the assistant when scull runs as \
             root/administrator (or when that can't be told); read-only tools only",
        );
        return Ok(server);
    }
    let who = exec::who()?;
    Ok(server.with_in_process_plan_tools(
        Principal {
            machine_id: who.machine_id,
            user: who.user,
        },
        scull_storage::AgentClient::new("scull ask", Some(env!("CARGO_PKG_VERSION"))),
    ))
}

/// What scull (not the model) prints after an answer that prepared plans.
pub fn prepared_notice(plans: &[PreparedPlan]) -> String {
    let mut out = String::from(
        "\nThe assistant prepared a cleanup plan; nothing has been changed. It runs only after you \
         review and approve it yourself:\n",
    );
    for p in plans {
        out.push_str(&format!(
            "  plan {}: {} item(s), {} reclaimable\n    scull approve {}\n",
            escape_for_terminal(&p.plan_id),
            p.item_count,
            format_bytes(p.reclaimable_bytes),
            escape_for_terminal(&p.plan_id)
        ));
    }
    out.push_str(
        "Run that in your own terminal (or review the plan in the scull desktop app); it expires \
         15 minutes after it was prepared. scull ask cannot run this plan: after you approve it, \
         an AI agent connected to `scull mcp serve` in destructive mode can run it, or use the \
         follow-up command once it exists.\n",
    );
    out
}

/// `ask.toml` (if it exists) plus the environment.
fn load_ask_config(path: Option<&Path>) -> Result<AskConfig, ScullError> {
    let text = match path.map(std::fs::read_to_string) {
        Some(Ok(t)) => Some(t),
        Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => None,
        None => None,
        Some(Err(e)) => {
            let mut err =
                ScullError::new(ErrorKind::Config, format!("cannot read {CONFIG_FILE}: {e}"));
            if let Some(p) = path {
                err = err.with_path(FsPath::from_path(p));
            }
            return Err(err);
        }
    };
    AskConfig::resolve(text.as_deref(), |k| std::env::var(k).ok()).map_err(|e| match path {
        Some(p) if text.is_some() => e.with_path(FsPath::from_path(p)),
        _ => e,
    })
}

/// Recorded consent for this provider and endpoint, or the notice and a yes (typed, or
/// `--yes`), recorded. Refuses without either; nothing is sent then.
fn consent(
    cfg: &AskConfig,
    notice: &str,
    yes: bool,
    state: Option<&Path>,
    can_prepare: bool,
) -> Result<(), ScullError> {
    let file = state.map(|s| s.join(CONSENT_FILE));
    let mut log = file
        .as_deref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|t| ConsentLog::parse(&t))
        .unwrap_or_default();
    if log.allows(cfg, can_prepare) {
        return Ok(());
    }
    let mut err = io::stderr().lock();
    let _ = writeln!(err, "{}\n", escape_lines(notice));
    if !yes {
        let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
        if !interactive {
            return Err(ScullError::new(
                ErrorKind::Config,
                format!(
                    "scull ask needs your consent before sending anything to {}: run it in a \
                     terminal to answer the question above, or pass --yes after reading \
                     `scull ask --show-request`",
                    cfg.endpoint.origin()
                ),
            ));
        }
        let _ = write!(
            err,
            "Send questions and read-only tool results to {}? [y/N] ",
            escape_for_terminal(&cfg.endpoint.origin())
        );
        let _ = err.flush();
        drop(err);
        let mut answer = String::new();
        io::stdin()
            .lock()
            .read_line(&mut answer)
            .map_err(|e| ScullError::from_io(&e))?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err(ScullError::new(
                ErrorKind::Cancelled,
                "nothing was sent (consent not given)",
            ));
        }
    }
    log.record(
        cfg,
        can_prepare,
        u64::try_from(scull_engine::now_unix()).unwrap_or(0),
    );
    // Consent still applies to this run if it cannot be recorded; it is asked again next time.
    if let (Some(state), Some(file)) = (state, file.as_deref())
        && scull_platform::create_dir_owner_only(state).is_ok()
    {
        let _ = super::init::replace_owner_only(file, &log.to_text());
    }
    Ok(())
}

/// Escape every line for the terminal (SAFE-17), keeping the line breaks themselves: a model's
/// answer and the notice are multi-line prose, but no control or bidi character in them may
/// reach the terminal.
fn escape_lines(s: &str) -> String {
    s.split('\n')
        .map(|l| escape_for_terminal(l.strip_suffix('\r').unwrap_or(l)).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_stdout(text: &str) -> Result<(), ScullError> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .or_else(crate::commands::ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))
}

#[cfg(test)]
mod tests {
    use super::escape_lines;

    #[test]
    fn answers_keep_line_breaks_but_never_control_characters() {
        let s = escape_lines("one\r\ntwo \u{1b}[2J\u{202e}three\n");
        assert_eq!(s, "one\ntwo \\x1b[2J\\u{202e}three\n");
    }
}
