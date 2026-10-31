//! `scull caches` (P1-CLI-02, Quick Clean QC-01…05): package-manager caches (npm, pnpm, Bun,
//! Yarn) located per OS without running any tool, sized, and classified. Read-only; cleaning
//! them is `scull clean caches` (P2).
//!
//! Machine output: `scull/caches@1` (`--ndjson`: `candidate` lines, `refused` lines, then a
//! `summary`).

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
use scull_core::{Candidate, ErrorKind, ScullError};
use scull_detectors::rules::Platform as RulesOs;
use scull_detectors::{CacheEnv, commands_for_rule};
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
        Format::Json => write_document(out, CACHES_SCHEMA, &report),
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
        out.push_str("No package-manager caches found (npm, pnpm, Bun, Yarn).\n");
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
            let cmd = commands_for_rule(&c.rule.id)
                .map_or_else(|| "unknown".to_owned(), |k| k.preferred.to_string());
            let note = if c.action == Action::None {
                " (not offered at this tier)"
            } else {
                ""
            };
            out.push_str(&format!(
                "  {}: {}{note}\n",
                escape_for_terminal(&c.artifact),
                escape_for_terminal(&cmd)
            ));
        }
        out.push_str(&format!(
            "\n{}\n",
            Style::Bold.paint("Totals by tier:", color)
        ));
        out.push_str(&render::totals_table(&r.totals, color));
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
