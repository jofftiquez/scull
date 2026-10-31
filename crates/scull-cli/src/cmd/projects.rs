//! `scull projects [inspect <path>]` (P1-CLI-02): projects under the scan roots with their
//! reclaimable bytes per tier, or one project's candidates in detail. Read-only; never
//! records history.
//!
//! Machine output: `scull/projects@1` (`project` lines then a `summary` line with
//! `--ndjson`) and `scull/project@1` for `inspect` (`candidate` lines then a `summary`).

use super::pipeline::{self, CancelBridge};
use super::render;
use crate::cancel;
use crate::cli::RootArgs;
use crate::commands::ignore_broken_pipe;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::{NdjsonWriter, SchemaId, write_document};
use crate::output::style::Style;
use crate::output::table::{Align, Cell, Table};
use crate::output::{Format, Ui};
use schemars::JsonSchema;
use scull_core::{Candidate, Evidence, FsPath, ScullError, SizeKind, Tier};
use scull_engine::paths::canon;
use scull_engine::{Issue, ProjectSummary, ScanReport, ScanRequest, SignalUnavailable};
use serde::Serialize;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Schema of `scull projects --json` / `--ndjson`.
pub const PROJECTS_SCHEMA: SchemaId = SchemaId::new("projects", 1);
/// Schema of `scull projects inspect --json` / `--ndjson`.
pub const PROJECT_SCHEMA: SchemaId = SchemaId::new("project", 1);

/// `scull projects` data.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectsReport {
    /// The roots scanned.
    pub roots: Vec<FsPath>,
    /// Whether the user cancelled (partial).
    pub cancelled: bool,
    /// Projects, by path.
    pub projects: Vec<ProjectSummary>,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Places that could not be read.
    pub issues: Vec<Issue>,
}

/// One `scull projects --ndjson` record.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ProjectsRecord {
    /// A project.
    Project(Box<ProjectSummary>),
    /// The last line.
    Summary(ProjectsSummary),
}

/// The `summary` record of `scull projects --ndjson`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectsSummary {
    /// The roots scanned.
    pub roots: Vec<FsPath>,
    /// Whether the user cancelled.
    pub cancelled: bool,
    /// Number of `project` lines.
    pub project_count: u64,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Places that could not be read.
    pub issues: Vec<Issue>,
}

/// `scull projects inspect` data.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectInspect {
    /// The inspected directory.
    pub path: FsPath,
    /// Whether the user cancelled.
    pub cancelled: bool,
    /// The project at the path, if it is one (a marker or the owner of a candidate).
    pub project: Option<ProjectSummary>,
    /// Every candidate under the path, with evidence, regeneration and action.
    pub candidates: Vec<Candidate>,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Places that could not be read.
    pub issues: Vec<Issue>,
}

/// One `scull projects inspect --ndjson` record.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ProjectRecord {
    /// A candidate under the path.
    Candidate(Box<Candidate>),
    /// The last line: the inspection without its candidate list.
    Summary(Box<ProjectInspectSummary>),
}

/// The `summary` record of `scull projects inspect --ndjson`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectInspectSummary {
    /// The inspected directory.
    pub path: FsPath,
    /// Whether the user cancelled.
    pub cancelled: bool,
    /// The project at the path, if it is one.
    pub project: Option<ProjectSummary>,
    /// Number of `candidate` lines.
    pub candidate_count: u64,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// Places that could not be read.
    pub issues: Vec<Issue>,
}

fn scan(roots_from_cli: &[PathBuf], args: &RootArgs, ui: &Ui) -> Result<ScanReport, ScullError> {
    let mut loaded = pipeline::load_config(roots_from_cli)?;
    loaded.allow_network_roots = args.allow_network_roots;
    let roots = pipeline::roots(&loaded, args.allow_broad_roots)?;
    let engine = pipeline::engine(&loaded, &roots, false, ui)?;
    let bridge = CancelBridge::start();
    let mut req = ScanRequest::full().history(false);
    req.worktrees = false;
    req.threads = loaded.config.scan.threads as usize;
    engine.scan(&roots, &req, bridge.token(), pipeline::progress(ui))
}

/// Run `scull projects`.
pub fn run(args: &RootArgs, ui: &Ui) -> Result<(), ScullError> {
    let r = scan(&args.roots, args, ui)?;
    let report = ProjectsReport {
        roots: r.roots.clone(),
        cancelled: r.cancelled,
        projects: r.projects.clone(),
        signals_unavailable: r.signals_unavailable.clone(),
        issues: r.issues.clone(),
    };
    let out = io::stdout().lock();
    let res = match ui.format {
        Format::Json => write_document(out, PROJECTS_SCHEMA, &report),
        Format::Ndjson => {
            let mut nd = NdjsonWriter::new(out, PROJECTS_SCHEMA);
            report
                .projects
                .iter()
                .try_for_each(|p| nd.record("project", p))
                .and_then(|()| {
                    nd.record(
                        "summary",
                        &ProjectsSummary {
                            roots: report.roots.clone(),
                            cancelled: report.cancelled,
                            project_count: report.projects.len() as u64,
                            signals_unavailable: report.signals_unavailable.clone(),
                            issues: report.issues.clone(),
                        },
                    )
                })
        }
        Format::Human => write_text(out, &render_projects(&report, ui.color_out)),
    };
    res.or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))?;
    finish(report.cancelled)
}

/// Run `scull projects inspect <path>`.
pub fn inspect(path: &Path, args: &RootArgs, ui: &Ui) -> Result<(), ScullError> {
    let r = scan(&[path.to_path_buf()], args, ui)?;
    let want = canon(path);
    let report = ProjectInspect {
        path: FsPath::from_path(&want),
        cancelled: r.cancelled,
        project: r
            .projects
            .iter()
            .find(|p| p.root.to_path_buf() == want)
            .cloned(),
        candidates: r.candidates.clone(),
        signals_unavailable: r.signals_unavailable.clone(),
        issues: r.issues.clone(),
    };
    let out = io::stdout().lock();
    let res = match ui.format {
        Format::Json => write_document(out, PROJECT_SCHEMA, &report),
        Format::Ndjson => {
            let mut nd = NdjsonWriter::new(out, PROJECT_SCHEMA);
            report
                .candidates
                .iter()
                .try_for_each(|c| nd.record("candidate", c))
                .and_then(|()| {
                    nd.record(
                        "summary",
                        &ProjectInspectSummary {
                            path: report.path.clone(),
                            cancelled: report.cancelled,
                            project: report.project.clone(),
                            candidate_count: report.candidates.len() as u64,
                            signals_unavailable: report.signals_unavailable.clone(),
                            issues: report.issues.clone(),
                        },
                    )
                })
        }
        Format::Human => write_text(out, &render_inspect(&report, ui.color_out)),
    };
    res.or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))?;
    finish(report.cancelled)
}

fn finish(cancelled: bool) -> Result<(), ScullError> {
    if cancelled || cancel::is_cancelled() {
        Err(cancel::cancelled_error())
    } else {
        Ok(())
    }
}

fn write_text<W: Write>(mut out: W, text: &str) -> io::Result<()> {
    out.write_all(text.as_bytes())?;
    out.flush()
}

fn badge(t: Option<Tier>) -> Cell {
    match t {
        Some(t) => Cell::tier(t),
        None => Cell::text("-"),
    }
}

/// The `scull projects` table.
pub fn render_projects(r: &ProjectsReport, color: bool) -> String {
    let mut out = String::new();
    if r.cancelled {
        out.push_str("CANCELLED: partial results\n\n");
    }
    if let [one] = r.roots.as_slice() {
        out.push_str(&format!(
            "Projects under {}\n\n",
            escape_for_terminal(one.as_str())
        ));
    }
    if r.projects.is_empty() {
        out.push_str("No projects found.\n");
    } else {
        let base = match r.roots.as_slice() {
            [one] => Some(one.to_path_buf()),
            _ => None,
        };
        let mut t = Table::new([
            ("Project", Align::Left),
            ("Ecosystems", Align::Left),
            ("Items", Align::Right),
            Table::size_column(SizeKind::Reclaimable),
            ("Badge", Align::Left),
            ("Path", Align::Left),
        ]);
        for p in &r.projects {
            t.push(vec![
                Cell::text(&p.name),
                Cell::text(&p.ecosystems.join(", ")),
                Cell::text(&p.candidates.to_string()),
                Cell::bytes(p.reclaimable.total()),
                badge(p.badge),
                Cell::path(&render::display_path(&p.root, base.as_deref())),
            ]);
        }
        out.push_str(&t.render(color));
        out.push_str(&format!(
            "\n{} project(s). Badge: most protective tier among a project's items.\n",
            r.projects.len()
        ));
    }
    out.push_str(&render::signals_section(&r.signals_unavailable, color));
    out.push_str(&render::issues_section(&r.issues, 0, color));
    out
}

/// The `scull projects inspect` view: the project, then every candidate in detail.
pub fn render_inspect(r: &ProjectInspect, color: bool) -> String {
    let mut out = String::new();
    if r.cancelled {
        out.push_str("CANCELLED: partial results\n\n");
    }
    match &r.project {
        Some(p) => {
            out.push_str(&format!(
                "{} {}\n",
                Style::Bold.paint("Project", color),
                escape_for_terminal(&p.name)
            ));
            out.push_str(&format!(
                "  path:        {}\n",
                escape_for_terminal(p.root.as_str())
            ));
            out.push_str(&format!(
                "  ecosystems:  {}\n",
                escape_for_terminal(&p.ecosystems.join(", "))
            ));
            out.push_str(&format!(
                "  markers:     {}\n",
                escape_for_terminal(&p.markers.join(", "))
            ));
            if let Some(g) = &p.git_worktree {
                out.push_str(&format!(
                    "  git:         {}\n",
                    escape_for_terminal(g.as_str())
                ));
            }
            if let Some(a) = p.total_allocated {
                out.push_str(&format!("  size:        {} allocated\n", format_bytes(a)));
            }
        }
        None => out.push_str(&format!(
            "No project marker at {}; showing everything found under it.\n",
            escape_for_terminal(r.path.as_str())
        )),
    }
    out.push('\n');
    if r.candidates.is_empty() {
        out.push_str("No candidates.\n");
    }
    for c in &r.candidates {
        out.push_str(&candidate_detail(c, color));
        out.push('\n');
    }
    out.push_str(&render::signals_section(&r.signals_unavailable, color));
    out.push_str(&render::issues_section(&r.issues, 0, color));
    out
}

/// Everything the preview shows for one candidate (PRD §10.2).
pub fn candidate_detail(c: &Candidate, color: bool) -> String {
    let e = |s: &str| escape_for_terminal(s).into_owned();
    let tier = crate::output::style::tier_style(c.tier)
        .paint(&crate::output::style::tier_text(c.tier), color);
    let mut out = format!("{tier}  {}\n", e(c.path.as_str()));
    out.push_str(&format!(
        "  artifact:     {} (rule {} v{})\n",
        e(&c.artifact),
        e(&c.rule.id),
        c.rule.version
    ));
    if let Some(p) = &c.project {
        out.push_str(&format!(
            "  project:      {} ({})\n",
            e(&p.name),
            e(p.root.as_str())
        ));
    }
    out.push_str(&format!(
        "  sizes:        {} logical, {} allocated, {} reclaimable ({} confidence)\n",
        format_bytes(c.sizes.logical),
        format_bytes(c.sizes.allocated),
        format_bytes(c.sizes.reclaimable.bytes),
        crate::output::bytes::confidence_label(c.sizes.reclaimable.confidence)
    ));
    if c.sizes.reclaimable.shared_bytes > 0 {
        out.push_str(&format!(
            "  shared:       {} hard-linked elsewhere (frees ~0 until the other links go)\n",
            format_bytes(c.sizes.reclaimable.shared_bytes)
        ));
    }
    out.push_str(&format!(
        "  action:       {} ({:?})\n",
        render::action_label(&c.action),
        c.restorability()
    ));
    out.push_str("  why:\n");
    for r in &c.reasons {
        out.push_str(&format!("    {} {}\n", r.tier, e(&r.message)));
    }
    let evidence: Vec<String> = c.evidence.iter().map(evidence_line).collect();
    if !evidence.is_empty() {
        out.push_str("  evidence:\n");
        for line in evidence {
            out.push_str(&format!("    {}\n", e(&line)));
        }
    }
    out.push_str(&format!(
        "  regenerate:   {}\n",
        e(c.regeneration.instructions.as_deref().unwrap_or("unknown"))
    ));
    out
}

fn evidence_line(ev: &Evidence) -> String {
    match ev {
        Evidence::Marker { path } => format!("marker {path}"),
        Evidence::Lockfile { path } => format!("lockfile {path}"),
        Evidence::KnownArtifact { artifact } => format!("known artifact: {artifact}"),
        Evidence::GitTracked { files } => format!("tracked by Git (at least {files} file(s))"),
        Evidence::ProtectedRoot { root } => format!("protected root {root}"),
        Evidence::UserExclusion { rule } => format!("user exclusion {rule}"),
        Evidence::ProtectMarker { path } => format!("protect marker {path}"),
        Evidence::InUse { signal, by } => match by {
            Some(b) => format!("in use ({signal}) by {b}"),
            None => format!("in use ({signal})"),
        },
        Evidence::LastModified {
            newest_unix,
            sampled,
        } => format!(
            "last modified at unix time {newest_unix}{}",
            if *sampled { " (sampled)" } else { "" }
        ),
        Evidence::CrossesMount => "crosses a mount or volume boundary".into(),
        Evidence::LinkEscape { target } => format!("link escapes to {target}"),
        Evidence::CloudSync { provider } => format!("cloud-synced ({provider})"),
        Evidence::Hardlinked { shared_bytes } => {
            format!("{} hard-linked elsewhere", format_bytes(*shared_bytes))
        }
        Evidence::SignalUnavailable { signal, reason } => {
            format!("signal unavailable: {signal}: {reason}")
        }
        Evidence::Note { text } => text.clone(),
        other => format!("{other:?}"),
    }
}
