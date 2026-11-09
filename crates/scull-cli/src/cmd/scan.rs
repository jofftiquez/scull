//! `scull scan` (P1-CLI-02, PRD §12): scan roots for reclaimable developer storage, classify
//! everything found, and record the scan in history. Read-only: nothing on disk changes
//! except scull's own history database (skipped with `--no-history`).
//!
//! Machine output: `scull/scan@1` (`--json`: one document whose `data` is a
//! [`ScanReport`]; `--ndjson`: one `candidate` line per candidate, then one `summary` line).
//! Ctrl-C stops the walk; the partial report (marked `cancelled`) is still written, nothing is
//! marked gone in history, and the exit code is 130.

use super::pipeline::{self, CancelBridge};
use super::render;
use crate::cancel;
use crate::cli::ScanArgs;
use crate::commands::ignore_broken_pipe;
use crate::output::json::{NdjsonWriter, SchemaId, write_document};
use crate::output::style::Style;
use crate::output::{Format, Ui};
use schemars::JsonSchema;
use scull_core::{Candidate, Category, FsPath, ScanId, ScullError};
use scull_engine::{
    HistoryOutcome, Issue, ProjectSummary, RepoConfigWarning, ScanReport, ScanRequest,
    SignalUnavailable, Totals, WorktreeProblemReport, WorktreeReport,
};
use serde::Serialize;
use std::io::{self, Write};

/// Schema of `scull scan --json` / `--ndjson`.
pub const SCAN_SCHEMA: SchemaId = SchemaId::new("scan", 1);

/// One `scull scan --ndjson` record.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ScanRecord {
    /// A candidate (one line each, most reclaimable first).
    Candidate(Box<Candidate>),
    /// The last line: everything else in the report.
    Summary(Box<ScanSummary>),
}

/// The `summary` record: the [`ScanReport`] without its candidate list.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanSummary {
    /// This scan's ID.
    pub scan_id: ScanId,
    /// Unix time (seconds) the scan started.
    pub started_at: i64,
    /// Unix time (seconds) the scan finished or was cancelled.
    pub finished_at: i64,
    /// The roots scanned.
    pub roots: Vec<FsPath>,
    /// Whether the user cancelled (the report is partial).
    pub cancelled: bool,
    /// Number of `candidate` lines written before this one.
    pub candidate_count: u64,
    /// Projects found.
    pub projects: Vec<ProjectSummary>,
    /// Git worktrees found.
    pub worktrees: Vec<WorktreeReport>,
    /// Worktree registrations that could not be trusted.
    pub worktree_problems: Vec<WorktreeProblemReport>,
    /// Totals by tier.
    pub totals: Totals,
    /// Entries counted.
    pub entries_scanned: u64,
    /// Places that could not be read or were not descended.
    pub issues: Vec<Issue>,
    /// Issues not listed.
    pub issues_truncated: u64,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Ignored repository settings.
    pub config_warnings: Vec<RepoConfigWarning>,
    /// History recording.
    pub history: HistoryOutcome,
}

impl From<&ScanReport> for ScanSummary {
    fn from(r: &ScanReport) -> Self {
        Self {
            scan_id: r.scan_id.clone(),
            started_at: r.started_at,
            finished_at: r.finished_at,
            roots: r.roots.clone(),
            cancelled: r.cancelled,
            candidate_count: r.candidates.len() as u64,
            projects: r.projects.clone(),
            worktrees: r.worktrees.clone(),
            worktree_problems: r.worktree_problems.clone(),
            totals: r.totals,
            entries_scanned: r.entries_scanned,
            issues: r.issues.clone(),
            issues_truncated: r.issues_truncated,
            signals_unavailable: r.signals_unavailable.clone(),
            config_warnings: r.config_warnings.clone(),
            history: r.history.clone(),
        }
    }
}

/// Run `scull scan`.
pub fn run(args: &ScanArgs, ui: &Ui) -> Result<(), ScullError> {
    if let Some(target) = &args.ssh {
        return super::remote::scan(target, args.remote_bin.as_deref(), ui);
    }
    let mut loaded = pipeline::load_config(&args.paths)?;
    loaded.allow_network_roots = args.allow_network_roots;
    let roots = pipeline::roots(&loaded, args.allow_broad_roots)?;
    let engine = pipeline::engine(&loaded, &roots, !args.no_history, ui)?;
    let bridge = CancelBridge::start();
    let mut req = ScanRequest::full().history(!args.no_history);
    req.threads = loaded.config.scan.threads as usize;
    let report = engine.scan(&roots, &req, bridge.token(), pipeline::progress(ui))?;
    drop(bridge);
    emit(&report, ui)?;
    if report.cancelled || cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    Ok(())
}

/// Write the report in the selected format.
pub fn emit(report: &ScanReport, ui: &Ui) -> Result<(), ScullError> {
    let out = io::stdout().lock();
    let r = match ui.format {
        Format::Json => write_document(out, SCAN_SCHEMA, report),
        Format::Ndjson => write_ndjson(out, report),
        Format::Human => {
            let mut out = out;
            out.write_all(render(report, ui.color_out).as_bytes())
                .and_then(|()| out.flush())
        }
    };
    r.or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))
}

/// `candidate` lines, then the `summary` line.
pub fn write_ndjson<W: Write>(out: W, report: &ScanReport) -> io::Result<()> {
    let mut nd = NdjsonWriter::new(out, SCAN_SCHEMA);
    for c in &report.candidates {
        nd.record("candidate", c)?;
    }
    nd.record("summary", &ScanSummary::from(report))
}

/// The human report.
pub fn render(r: &ScanReport, color: bool) -> String {
    let mut out = String::new();
    let roots: Vec<String> = r
        .roots
        .iter()
        .map(|p| crate::output::escape::escape_for_terminal(p.as_str()).into_owned())
        .collect();
    out.push_str(&format!(
        "Scanned {} ({} entries){}\n\n",
        roots.join(", "),
        r.entries_scanned,
        if r.cancelled {
            " - CANCELLED: partial results"
        } else {
            ""
        }
    ));
    // One root: paths relative to it (the header names it).
    let base = match r.roots.as_slice() {
        [one] => Some(one.to_path_buf()),
        _ => None,
    };
    let artifacts: Vec<Candidate> = r
        .candidates
        .iter()
        .filter(|c| c.category != Category::Worktree)
        .cloned()
        .collect();
    if artifacts.is_empty() {
        out.push_str("No reclaimable artifacts found.\n");
    } else {
        out.push_str(&render::candidates_table(
            &artifacts,
            base.as_deref(),
            color,
        ));
    }
    let worktrees: Vec<Candidate> = r
        .candidates
        .iter()
        .filter(|c| c.category == Category::Worktree)
        .cloned()
        .collect();
    if !worktrees.is_empty() {
        out.push_str(&format!(
            "\n{} (details: scull worktrees)\n",
            Style::Bold.paint("Git worktrees:", color)
        ));
        out.push_str(&render::candidates_table(
            &worktrees,
            base.as_deref(),
            color,
        ));
    }
    out.push_str(&format!(
        "\n{}\n",
        Style::Bold.paint("Totals by tier:", color)
    ));
    out.push_str(&render::totals_table(&r.totals, color));
    out.push('\n');
    out.push_str(&render::reclaim_line(&r.totals));
    out.push('\n');
    out.push_str(&render::estimate_note(&r.candidates));
    out.push_str(&render::signals_section(&r.signals_unavailable, color));
    if !r.config_warnings.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            Style::Warn.paint(
                "Ignored repository settings (repository files can only add protection):",
                color
            )
        ));
        for w in &r.config_warnings {
            out.push_str(&format!(
                "  {}: {}: {}\n",
                crate::output::escape::escape_for_terminal(w.file.as_str()),
                crate::output::escape::escape_for_terminal(&w.warning.key),
                crate::output::escape::escape_for_terminal(&w.warning.message)
            ));
        }
    }
    out.push_str(&render::issues_section(
        &r.issues,
        r.issues_truncated,
        color,
    ));
    out.push_str(&format!(
        "\nScan ID {} - history: {}\n",
        r.scan_id,
        match &r.history {
            HistoryOutcome::Disabled => "off".to_owned(),
            HistoryOutcome::Recorded {
                new_items,
                disappeared,
            } => format!("recorded ({new_items} new, {disappeared} gone)"),
            HistoryOutcome::Failed { message } => format!(
                "not recorded ({})",
                crate::output::escape::escape_for_terminal(message)
            ),
        }
    ));
    out
}
