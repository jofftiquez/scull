//! `scull mcp serve` (P1-MCP-01, MCP-01…05): the read-only MCP server over stdio.
//!
//! * **Opt-in** (MCP-01): refuses to start unless `mcp.enabled = true`. Run once in a terminal,
//!   it shows the privacy disclosure and offers to write the setting; without a terminal
//!   (e.g. started by an AI client) it fails with a configuration error (exit 2).
//! * **Disclosure** (MCP-05): [`scull_mcp::DISCLOSURE`] is printed to stderr the first time
//!   the server starts after being enabled (a marker in scull's state directory records it).
//! * The allowlist is `scan.roots`; broad roots (a filesystem root, the home folder or a
//!   folder containing it, CFG-04) are left out with a warning.
//! * stdout carries only JSON-RPC; everything else goes to stderr. Ctrl-C stops the server.
//! * **Destructive mode** (MCP-04, FX-MCP-07): the plan and execute tools are offered only
//!   when `[mcp] destructive = true` is in the user config file **and** the server is started
//!   with `--allow-destructive`, and never when running elevated. Plans the agent prepares
//!   run only after the user approves them with `scull approve` ([`super::approve`]); execution
//!   goes through the Executor ([`super::mcp_runner`]).

use super::doctor::CONFIG_FILE_NAME;
use super::init::replace_owner_only;
use super::mcp_runner::ExecutorRunner;
use super::{exec, pipeline};
use crate::cancel;
use crate::output::Ui;
use scull_core::config::{ConfigLayer, validate_roots};
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_detectors::CacheEnv;
use scull_detectors::rules::Platform as RulesOs;
use scull_engine::paths::canon;
use scull_mcp::approval::Principal;
use scull_mcp::{DISCLOSURE, ScullMcp, ServerOptions};
use scull_platform::app_dirs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// File in scull's state directory recording that the disclosure was shown.
pub const DISCLOSURE_MARKER: &str = "mcp-disclosure-shown";

/// `scull mcp serve`.
pub fn serve(allow_destructive: bool, ui: &Ui) -> Result<(), ScullError> {
    let loaded = pipeline::load_config(&[])?;
    let dirs = app_dirs().ok();
    let config_path = dirs.as_ref().map(|d| d.config.join(CONFIG_FILE_NAME));
    if !loaded.config.mcp.enabled {
        let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
        if !interactive {
            return Err(disabled(config_path.as_deref()));
        }
        return enable_interactively(config_path.as_deref(), dirs.as_ref().map(|d| &d.state));
    }
    if let Some(d) = &dirs {
        disclose_once(&d.state);
    }
    let server = build_server(&loaded, dirs.map(|d| d.data), ui)?;
    let server = if allow_destructive {
        destructive(server, &loaded, ui)?
    } else {
        server
    };
    // Ctrl-C ends the session with exit 130 (CLI-05), whether it came before or after the
    // client's `initialize` (#251): a handshake cut short by the stop is reported as cancelled,
    // not as a failed session.
    match scull_mcp::serve_stdio(server, cancel::is_cancelled) {
        Err(_) if cancel::is_cancelled() => Err(cancel::cancelled_error()),
        other => other,
    }
}

/// `--allow-destructive`: offer the plan tools when the config file also allows them and the
/// server is not elevated; otherwise say why it stays read-only.
fn destructive(
    server: ScullMcp,
    loaded: &pipeline::Loaded,
    ui: &Ui,
) -> Result<ScullMcp, ScullError> {
    if !loaded.config.mcp.destructive {
        pipeline::warn(
            ui,
            "--allow-destructive has no effect without `[mcp] destructive = true` in scull's \
             config file (both are required); serving read-only tools only",
        );
        return Ok(server);
    }
    if exec::elevation().blocks_destructive() {
        pipeline::warn(
            ui,
            "destructive mode is never offered to agents when scull runs as root/administrator \
             (or when that can't be told); serving read-only tools only",
        );
        return Ok(server);
    }
    let who = exec::who()?;
    let runner = ExecutorRunner {
        quarantine: exec::quarantine_settings(&loaded.config.quarantine),
        trash: exec::trash_locations(),
        audit_lock_refusal: true,
    };
    let server = server.with_plan_tools(
        true,
        std::sync::Arc::new(runner),
        Principal {
            machine_id: who.machine_id,
            user: who.user,
        },
    );
    if server.plan_tools_enabled() {
        pipeline::warn(
            ui,
            "destructive mode: agents can prepare cleanup plans; each one runs only after you \
             approve it with `scull approve <plan-id>` in your own terminal",
        );
    }
    Ok(server)
}

/// The read-only server over `scan.roots` (without broad roots) and the `[mcp]` settings: what
/// `scull mcp serve` offers its client, and what `scull ask` offers the model.
pub(crate) fn build_server(
    loaded: &pipeline::Loaded,
    data_dir: Option<PathBuf>,
    ui: &Ui,
) -> Result<ScullMcp, ScullError> {
    let roots = allowlisted_roots(loaded, ui);
    if roots.is_empty() {
        pipeline::warn(
            ui,
            "no scan roots are configured, so every path will be refused; set scan.roots \
             (scull init)",
        );
    }
    let engine = pipeline::engine(loaded, &roots, false, ui)?;
    let cache_env = loaded.home.clone().and_then(|home| {
        RulesOs::current().map(|os| CacheEnv::new(home, os, |k| std::env::var(k).ok()))
    });
    ScullMcp::new(
        engine,
        ServerOptions {
            roots,
            home: loaded.home.clone(),
            data_dir,
            mcp: loaded.config.mcp.clone(),
            threads: loaded.config.scan.threads as usize,
            cache_env,
            forecast_policy: scull_proactive::forecast_policy(&loaded.config.proactive),
        },
    )
}

fn disabled(config_path: Option<&Path>) -> ScullError {
    let where_ = config_path.map_or_else(
        || "scull's config file".to_owned(),
        |p| p.display().to_string(),
    );
    let e = ScullError::new(
        ErrorKind::Config,
        format!(
            "the MCP server is not enabled: add `[mcp]` `enabled = true` to {where_}, or run \
             `scull mcp serve` once in a terminal to review the privacy notice and enable it"
        ),
    );
    match config_path {
        Some(p) => e.with_path(FsPath::from_path(p)),
        None => e,
    }
}

/// The disclosure, a yes/no question on the terminal, and `mcp.enabled = true` on yes.
fn enable_interactively(
    config_path: Option<&Path>,
    state: Option<&PathBuf>,
) -> Result<(), ScullError> {
    let Some(config_path) = config_path else {
        return Err(disabled(None));
    };
    let mut err = io::stderr().lock();
    let _ = writeln!(err, "{DISCLOSURE}\n");
    let _ = write!(err, "Enable the scull MCP server? [y/N] ");
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
            "the MCP server was not enabled",
        ));
    }
    write_enabled(config_path)?;
    if let Some(s) = state {
        mark_disclosed(s);
    }
    let _ = writeln!(
        io::stderr(),
        "MCP enabled in {}.\nNow register `scull mcp serve` with your AI client, e.g. \
         `claude mcp add scull -- scull mcp serve` (Claude Code) or `[mcp_servers.scull]` \
         with `command = \"scull\"` and `args = [\"mcp\", \"serve\"]` in ~/.codex/config.toml \
         (Codex).",
        config_path.display()
    );
    Ok(())
}

/// Set `mcp.enabled = true` in the user config file, keeping every other setting (the old file
/// is saved as `config.toml.bak`).
fn write_enabled(config_path: &Path) -> Result<(), ScullError> {
    let fs_err = |p: &Path, e: &dyn std::fmt::Display| {
        ScullError::new(ErrorKind::Io, e.to_string()).with_path(FsPath::from_path(p))
    };
    let existing = match std::fs::read_to_string(config_path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(fs_err(config_path, &e)),
    };
    let mut layer = match &existing {
        Some(t) => {
            ConfigLayer::from_toml(t).map_err(|e| e.with_path(FsPath::from_path(config_path)))?
        }
        None => ConfigLayer::default(),
    };
    layer.mcp.enabled = Some(true);
    let text = format!(
        "# scull user config (CFG-01). `mcp.enabled` written by `scull mcp serve`.\n{}",
        toml::to_string(&layer).map_err(|e| ScullError::new(ErrorKind::Internal, e.to_string()))?
    );
    ConfigLayer::from_toml(&text).map_err(|e| ScullError::new(ErrorKind::Internal, e.message))?;
    if let Some(dir) = config_path.parent() {
        scull_platform::create_dir_owner_only(dir).map_err(|e| fs_err(dir, &e))?;
    }
    if let Some(old) = &existing {
        let backup = config_path.with_file_name(format!("{CONFIG_FILE_NAME}.bak"));
        replace_owner_only(&backup, old).map_err(|e| fs_err(&backup, &e))?;
    }
    replace_owner_only(config_path, &text).map_err(|e| fs_err(config_path, &e))
}

/// Print the disclosure to stderr unless it was shown before, and remember that it was.
fn disclose_once(state: &Path) {
    if std::fs::symlink_metadata(state.join(DISCLOSURE_MARKER)).is_ok() {
        return;
    }
    let _ = writeln!(io::stderr(), "{DISCLOSURE}");
    mark_disclosed(state);
}

/// Remember that the disclosure was shown (also used by `scull agent install`).
pub(crate) fn mark_disclosed(state: &Path) {
    if scull_platform::create_dir_owner_only(state).is_ok() {
        let _ = replace_owner_only(
            &state.join(DISCLOSURE_MARKER),
            "The scull MCP privacy notice was shown.\n",
        );
    }
}

/// `scan.roots`, canonical, without broad roots (CFG-04).
pub(crate) fn allowlisted_roots(loaded: &pipeline::Loaded, ui: &Ui) -> Vec<PathBuf> {
    let home = loaded
        .home
        .as_deref()
        .map(canon)
        .unwrap_or_else(|| PathBuf::from("/nonexistent-home-for-scull"));
    let mut out = Vec::new();
    for r in &loaded.config.scan.roots {
        if !r.is_absolute() {
            pipeline::warn(ui, &format!("ignoring relative scan root {}", r.display()));
            continue;
        }
        let c = canon(r);
        match validate_roots(std::slice::from_ref(&c), &home, &[]) {
            Ok(concerns) if concerns.is_empty() => out.push(c),
            Ok(concerns) => {
                for x in concerns {
                    pipeline::warn(ui, &format!("not offered to MCP clients: {}", x.message));
                }
            }
            Err(e) => pipeline::warn(ui, &format!("ignoring scan root: {}", e.message)),
        }
    }
    out
}
