//! `scull caches` (P1-CLI-02, Quick Clean QC-01…05): package-manager caches (every
//! [`CacheTool`]: npm, pnpm, Bun, Yarn, pip, uv, conda, Go, Gradle, Maven, NuGet) located per
//! OS without running any tool, sized, and classified. Read-only; cleaning them is
//! `scull clean caches` (P2). Inspect-only caches (Gradle, Maven: no native cleanup command)
//! are shown and measured, never cleaned.
//!
//! Machine output: `scull/caches@1` (`--json`: the report plus one [`CacheCommandLine`] per
//! candidate in `commands`; `--ndjson`: `candidate` lines, `refused` lines, then a `summary`
//! that carries the same `commands`).

use super::pipeline::{self, CancelBridge};
use super::render;
use crate::cancel;
use crate::commands::ignore_broken_pipe;
use crate::output::escape::escape_for_terminal;
use crate::output::json::{NdjsonWriter, SchemaId, write_document};
use crate::output::style::Style;
use crate::output::{Format, Ui};
use schemars::JsonSchema;
use scull_core::Action;
use scull_core::{Candidate, ErrorKind, ItemId, ReasonCode, ScullError};
use scull_detectors::rules::Platform as RulesOs;
use scull_detectors::{CacheEnv, CacheTool, commands_for_rule};
use scull_engine::{CachesReport, Issue, RefusedCacheReport, SignalUnavailable, Totals};
use serde::Serialize;
use std::io::{self, Write};

/// Schema of `scull caches --json` / `--ndjson`.
pub const CACHES_SCHEMA: SchemaId = SchemaId::new("caches", 1);

/// One `scull caches --ndjson` record.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum CachesRecord {
    /// A cache found.
    Candidate(Box<Candidate>),
    /// A tool whose cache location was refused.
    Refused(Box<RefusedCacheReport>),
    /// The last line.
    Summary(Box<CachesSummary>),
}

/// The `summary` record of `scull caches --ndjson`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CachesSummary {
    /// Unix time (seconds) of the measurement.
    pub measured_at: i64,
    /// Whether the user cancelled.
    pub cancelled: bool,
    /// Number of `candidate` lines.
    pub candidate_count: u64,
    /// Number of `refused` lines.
    pub refused_count: u64,
    /// Totals by tier.
    pub totals: Totals,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Places that could not be read.
    pub issues: Vec<Issue>,
    /// The cleanup command of each `candidate`, in the same order (added in 1.x; absent in
    /// older output).
    #[serde(default)]
    pub commands: Vec<CacheCommandLine>,
}

/// `scull caches --json` data: the engine's report plus the cleanup command of each candidate.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CachesDocument {
    /// The located caches.
    #[serde(flatten)]
    pub report: CachesReport,
    /// The cleanup command of each candidate, in the order of `candidates` (added in 1.x;
    /// absent in older output).
    #[serde(default)]
    pub commands: Vec<CacheCommandLine>,
}

impl CachesDocument {
    /// The document for `report`.
    pub fn new(report: CachesReport) -> Self {
        let commands = report.candidates.iter().map(command_line).collect();
        Self { report, commands }
    }
}

/// Whether, and how, `scull clean caches` would clean one cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheCommandStatus {
    /// The tool's native command is offered at the cache's tier.
    Offered,
    /// The tool has a native command, but it is not offered at the cache's tier.
    NotOffered,
    /// The tool has no native cleanup command (Gradle, Maven): the cache is shown and
    /// measured, never cleaned.
    InspectOnly,
    /// Not a cache rule scull knows the commands of.
    Unknown,
}

/// The cleanup command of one cache candidate (the "Cleanup commands" line of the human
/// report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheCommandLine {
    /// ID of the candidate.
    pub id: ItemId,
    /// Rule ID of the cache (e.g. `npm.cache`).
    pub rule: String,
    /// Whether the command is offered.
    pub status: CacheCommandStatus,
    /// The preferred native command (`npm cache verify`); `null` when inspect-only or unknown.
    pub command: Option<String>,
    /// The line as printed, e.g. `npm cache verify` or
    /// `none (inspect-only: no native cleanup command)`.
    pub text: String,
}

/// The printed commands line for an inspect-only cache.
pub const INSPECT_ONLY_TEXT: &str = "none (inspect-only: no native cleanup command)";

/// The cleanup command of `c`.
pub fn command_line(c: &Candidate) -> CacheCommandLine {
    let commands = commands_for_rule(&c.rule.id);
    let command = commands.map(|k| k.preferred.to_string());
    let inspect_only = commands.is_none()
        && (CacheTool::from_rule_id(&c.rule.id).is_some()
            || c.reasons.iter().any(|r| r.code == ReasonCode::InspectOnly));
    let (status, text) = match &command {
        Some(cmd) if c.action == Action::None => (
            CacheCommandStatus::NotOffered,
            format!("{cmd} (not offered at this tier)"),
        ),
        Some(cmd) => (CacheCommandStatus::Offered, cmd.clone()),
        None if inspect_only => (
            CacheCommandStatus::InspectOnly,
            INSPECT_ONLY_TEXT.to_owned(),
        ),
        None => (CacheCommandStatus::Unknown, "unknown".to_owned()),
    };
    CacheCommandLine {
        id: c.id.clone(),
        rule: c.rule.id.clone(),
        status,
        command,
        text,
    }
}

/// The human empty-state line, naming every tool scull looks for.
pub fn nothing_found_message() -> String {
    let tools: Vec<&str> = CacheTool::ALL.iter().map(|t| t.artifact()).collect();
    format!(
        "No package-manager caches found.\nLooked for: {}.\n",
        tools.join(", ")
    )
}

/// Run `scull caches`.
pub fn run(ui: &Ui) -> Result<(), ScullError> {
    let loaded = pipeline::load_config(&[])?;
    let Some(home) = loaded.home.clone() else {
        return Err(ScullError::new(
            ErrorKind::Config,
            "the home folder is unknown (HOME / USERPROFILE is not set), so caches can't be located",
        ));
    };
    let Some(os) = RulesOs::current() else {
        return Err(ScullError::new(
            ErrorKind::UnsupportedPlatform,
            "package caches are located on Linux, macOS and Windows only",
        ));
    };
    let env = CacheEnv::new(home, os, |k| std::env::var(k).ok());
    let engine = pipeline::engine(&loaded, &[], false, ui)?;
    let bridge = CancelBridge::start();
    let report = engine.caches(
        &env,
        loaded.config.scan.threads as usize,
        bridge.token(),
        pipeline::progress(ui),
    );
    drop(bridge);
    let out = io::stdout().lock();
    let res = match ui.format {
        Format::Json => write_document(out, CACHES_SCHEMA, &CachesDocument::new(report.clone())),
        Format::Ndjson => write_ndjson(out, &report),
        Format::Human => {
            let mut out = out;
            out.write_all(render(&report, ui.color_out).as_bytes())
                .and_then(|()| out.flush())
        }
    };
    res.or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))?;
    if report.cancelled || cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    Ok(())
}

/// `candidate` lines, `refused` lines, then the `summary`.
pub fn write_ndjson<W: Write>(out: W, r: &CachesReport) -> io::Result<()> {
    let mut nd = NdjsonWriter::new(out, CACHES_SCHEMA);
    for c in &r.candidates {
        nd.record("candidate", c)?;
    }
    for x in &r.refused {
        nd.record("refused", x)?;
    }
    nd.record(
        "summary",
        &CachesSummary {
            measured_at: r.measured_at,
            cancelled: r.cancelled,
            candidate_count: r.candidates.len() as u64,
            refused_count: r.refused.len() as u64,
            totals: r.totals,
            signals_unavailable: r.signals_unavailable.clone(),
            issues: r.issues.clone(),
            commands: r.candidates.iter().map(command_line).collect(),
        },
    )
}

/// The human report.
pub fn render(r: &CachesReport, color: bool) -> String {
    let mut out = String::new();
    if r.cancelled {
        out.push_str("CANCELLED: partial results\n\n");
    }
    if r.candidates.is_empty() {
        out.push_str(&nothing_found_message());
    } else {
        out.push_str(&render::candidates_table(&r.candidates, None, color));
        out.push_str(&format!(
            "\n{}\n",
            Style::Bold.paint(
                "Cleanup commands (run by `scull clean caches`, never by this command):",
                color
            )
        ));
        for c in &r.candidates {
            out.push_str(&format!(
                "  {}: {}\n",
                escape_for_terminal(&c.artifact),
                escape_for_terminal(&command_line(c).text)
            ));
        }
        out.push_str(&format!(
            "\n{}\n",
            Style::Bold.paint("Totals by tier:", color)
        ));
        out.push_str(&render::totals_table(&r.totals, color));
        out.push_str(&render::estimate_note(&r.candidates));
    }
    if !r.refused.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            Style::Warn.paint(
                "Refused cache locations (nothing of these tools is offered):",
                color
            )
        ));
        for x in &r.refused {
            out.push_str(&format!(
                "  {} ({}): {}\n",
                escape_for_terminal(&x.rule),
                escape_for_terminal(&x.source),
                escape_for_terminal(&x.reason)
            ));
        }
    }
    out.push_str(&render::signals_section(&r.signals_unavailable, color));
    out.push_str(&render::issues_section(&r.issues, 0, color));
    out
}
