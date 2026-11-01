//! `scull worktrees [inspect <path>]` (P1-CLI-02, GIT-02/03): every Git worktree under the
//! scan roots with its state and removal tier, or one worktree in detail. Read-only: Git is
//! run hardened (no hooks, fsmonitor or fetch; SAFE-15) and history is not recorded.
//!
//! `scull worktrees register <path> --ttl 7d` (P4-AI-02, PRO-08) records a time-to-live for an
//! agent or script worktree in scull's registry file ([`scull_mcp::registry`]). The listing
//! shows each registration under the scan roots with its expiry; an expired one is a review
//! suggestion. Nothing is ever removed because a TTL ran out.
//!
//! Machine output: `scull/worktrees@1` (`worktree` lines then a `summary` with `--ndjson`),
//! `scull/worktree@1` for `inspect` (one `worktree` line with `--ndjson`) and
//! `scull/worktree-registration@1` for `register` (one `registration` line).

use super::pipeline::{self, CancelBridge};
use super::render;
use crate::cancel;
use crate::cli::RootArgs;
use crate::commands::ignore_broken_pipe;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::{NdjsonWriter, SchemaId, write_document};
use crate::output::style::{Style, tier_style, tier_text};
use crate::output::table::{Align, Cell, Table};
use crate::output::{Format, Ui};
use schemars::JsonSchema;
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_engine::now_unix;
use scull_engine::paths::{canon, is_within};
use scull_engine::{ScanRequest, SignalUnavailable, WorktreeProblemReport, WorktreeReport};
use scull_git::{HeadState, LockState, MergeStatus, Observed, WorktreeKind};
use scull_mcp::registry::{Registry, Source, human_duration, parse_ttl};
use serde::Serialize;
use std::io::{self, Write};
use std::path::Path;

/// Schema of `scull worktrees --json` / `--ndjson`.
pub const WORKTREES_SCHEMA: SchemaId = SchemaId::new("worktrees", 1);
/// Schema of `scull worktrees inspect --json` / `--ndjson`.
pub const WORKTREE_SCHEMA: SchemaId = SchemaId::new("worktree", 1);
/// Schema of `scull worktrees register --json` / `--ndjson`.
pub const REGISTRATION_SCHEMA: SchemaId = SchemaId::new("worktree-registration", 1);

/// A worktree TTL registration under the scan roots (shown by the human `scull worktrees`
/// listing; `scull/worktrees@1` is unchanged so the desktop bridge keeps the same shape).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TtlRegistration {
    /// The worktree folder.
    pub path: FsPath,
    /// Time to live, seconds.
    pub ttl_secs: u64,
    /// When it was registered (Unix seconds).
    pub registered_at: i64,
    /// When the TTL runs out (Unix seconds).
    pub expires_at: i64,
    /// Whether the TTL has run out: the worktree is a review suggestion (never removed
    /// automatically).
    pub expired: bool,
    /// Who registered it.
    pub source: Source,
    /// Whether the worktree was found by this scan.
    pub found: bool,
}

/// `scull worktrees register` data.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistrationReport {
    /// The registered worktree folder (canonical).
    pub path: FsPath,
    /// Time to live, seconds.
    pub ttl_secs: u64,
    /// The TTL, human (`7d`).
    pub ttl: String,
    /// When it was registered (Unix seconds).
    pub registered_at: i64,
    /// When the TTL runs out (Unix seconds).
    pub expires_at: i64,
    /// The registry file it was recorded in.
    pub registry: FsPath,
}

/// `scull worktrees` data.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreesReport {
    /// The roots scanned.
    pub roots: Vec<FsPath>,
    /// Whether the user cancelled (partial; worktrees are analysed after the walk).
    pub cancelled: bool,
    /// Every worktree of every repository found.
    pub worktrees: Vec<WorktreeReport>,
    /// Registrations whose metadata could not be trusted (protected).
    pub problems: Vec<WorktreeProblemReport>,
    /// Signals that could not be observed (e.g. `git` missing).
    pub signals_unavailable: Vec<SignalUnavailable>,
}

/// One `scull worktrees --ndjson` record.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorktreesRecord {
    /// A worktree.
    Worktree(Box<WorktreeReport>),
    /// The last line.
    Summary(WorktreesSummary),
}

/// The `summary` record of `scull worktrees --ndjson`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreesSummary {
    /// The roots scanned.
    pub roots: Vec<FsPath>,
    /// Whether the user cancelled.
    pub cancelled: bool,
    /// Number of `worktree` lines.
    pub worktree_count: u64,
    /// Untrusted registrations.
    pub problems: Vec<WorktreeProblemReport>,
    /// Signals that could not be observed.
    pub signals_unavailable: Vec<SignalUnavailable>,
}

/// The one `scull worktrees register --ndjson` record.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RegistrationRecord {
    /// The registration.
    Registration(RegistrationReport),
}

/// The one `scull worktrees inspect --ndjson` record.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorktreeRecord {
    /// The worktree.
    Worktree(Box<WorktreeReport>),
}

/// Run `scull worktrees`.
pub fn run(args: &RootArgs, ui: &Ui) -> Result<(), ScullError> {
    let mut loaded = pipeline::load_config(&args.roots)?;
    loaded.allow_network_roots = args.allow_network_roots;
    let roots = pipeline::roots(&loaded, args.allow_broad_roots)?;
    let engine = pipeline::engine(&loaded, &roots, false, ui)?;
    let bridge = CancelBridge::start();
    let mut req = ScanRequest::full().history(false);
    req.threads = loaded.config.scan.threads as usize;
    let r = engine.scan(&roots, &req, bridge.token(), pipeline::progress(ui))?;
    drop(bridge);
    // Signals that affected worktree decisions (not the artifact candidates of the walk).
    let mut signals: Vec<SignalUnavailable> = r
        .signals_unavailable
        .into_iter()
        .filter(|s| s.signal == scull_engine::GIT_SIGNAL && s.affected == 0)
        .collect();
    for w in &r.worktrees {
        let mut seen = std::collections::BTreeSet::new();
        for ev in &w.decision.evidence {
            if let scull_core::Evidence::SignalUnavailable { signal, reason } = ev
                && seen.insert(signal.clone())
            {
                match signals.iter_mut().find(|s| &s.signal == signal) {
                    Some(s) => s.affected += 1,
                    None => signals.push(SignalUnavailable {
                        signal: signal.clone(),
                        reason: reason.clone(),
                        affected: 1,
                    }),
                }
            }
        }
    }
    let registrations = registrations_under(&r.roots, &r.worktrees, now_unix(), ui);
    let report = WorktreesReport {
        roots: r.roots,
        cancelled: r.cancelled,
        worktrees: r.worktrees,
        problems: r.worktree_problems,
        signals_unavailable: signals,
    };
    let out = io::stdout().lock();
    let res = match ui.format {
        Format::Json => write_document(out, WORKTREES_SCHEMA, &report),
        Format::Ndjson => {
            let mut nd = NdjsonWriter::new(out, WORKTREES_SCHEMA);
            report
                .worktrees
                .iter()
                .try_for_each(|w| nd.record("worktree", w))
                .and_then(|()| {
                    nd.record(
                        "summary",
                        &WorktreesSummary {
                            roots: report.roots.clone(),
                            cancelled: report.cancelled,
                            worktree_count: report.worktrees.len() as u64,
                            problems: report.problems.clone(),
                            signals_unavailable: report.signals_unavailable.clone(),
                        },
                    )
                })
        }
        Format::Human => write_text(out, &render_list(&report, &registrations, ui.color_out)),
    };
    res.or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))?;
    if report.cancelled || cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    Ok(())
}

/// The registrations whose worktree lies under one of `roots` (read-only; a missing registry
/// is empty, an unreadable one is a warning).
fn registrations_under(
    roots: &[FsPath],
    worktrees: &[WorktreeReport],
    now: i64,
    ui: &Ui,
) -> Vec<TtlRegistration> {
    let Ok(dirs) = scull_platform::app_dirs() else {
        return Vec::new();
    };
    let regs = match Registry::in_data_dir(&dirs.data).load() {
        Ok(r) => r,
        Err(e) => {
            pipeline::warn(ui, &format!("cannot read worktree registrations: {e}"));
            return Vec::new();
        }
    };
    regs.into_iter()
        .filter(|g| {
            roots
                .iter()
                .any(|r| is_within(Path::new(&g.path), &r.to_path_buf()))
        })
        .map(|g| TtlRegistration {
            found: worktrees
                .iter()
                .any(|w| w.path.to_path_buf() == Path::new(&g.path)),
            expired: g.is_expired(now),
            path: FsPath::from_path(Path::new(&g.path)),
            ttl_secs: g.ttl_secs,
            registered_at: g.registered_at,
            expires_at: g.expires_at,
            source: g.source,
        })
        .collect()
}

/// Run `scull worktrees register <path> --ttl <duration>`: check that `path` is a linked Git
/// worktree and record its TTL. Never touches the worktree.
pub fn register(path: &Path, ttl: &str, ui: &Ui) -> Result<(), ScullError> {
    let ttl_secs =
        parse_ttl(ttl).map_err(|e| ScullError::new(ErrorKind::Usage, format!("--ttl {e}")))?;
    let loaded = pipeline::load_config(&[])?;
    let engine = pipeline::engine(&loaded, &[], false, ui)?;
    let w = engine.inspect_worktree(&canon(path))?;
    if w.kind != WorktreeKind::Linked || w.state.missing {
        return Err(ScullError::new(
            ErrorKind::Usage,
            format!(
                "{} is not an existing linked Git worktree (main and bare checkouts can't be \
                 registered)",
                path.display()
            ),
        )
        .with_path(FsPath::from_path(path)));
    }
    let dirs = scull_platform::app_dirs()
        .map_err(|e| ScullError::new(ErrorKind::Config, e.to_string()))?;
    let registry = Registry::in_data_dir(&dirs.data);
    let reg = registry
        .record(&w.path.to_path_buf(), ttl_secs, now_unix(), Source::Cli)
        .map_err(|e| ScullError::new(ErrorKind::Io, e.to_string()))?;
    let report = RegistrationReport {
        path: FsPath::from_path(Path::new(&reg.path)),
        ttl_secs,
        ttl: human_duration(ttl_secs),
        registered_at: reg.registered_at,
        expires_at: reg.expires_at,
        registry: FsPath::from_path(registry.file()),
    };
    match ui.format {
        Format::Human => super::emit_text(&format!(
            "Registered {}\n  TTL {}: expires in {} (Unix time {}).\n  When it expires, \
             `scull worktrees` lists it as a review suggestion; it is never removed \
             automatically.\n",
            escape_for_terminal(report.path.as_str()),
            report.ttl,
            report.ttl,
            report.expires_at
        )),
        f => super::emit_machine(f, REGISTRATION_SCHEMA, "registration", &report),
    }
}

/// Run `scull worktrees inspect <path>`.
pub fn inspect(path: &Path, ui: &Ui) -> Result<(), ScullError> {
    let loaded = pipeline::load_config(&[])?;
    let engine = pipeline::engine(&loaded, &[], false, ui)?;
    let report = engine.inspect_worktree(path)?;
    let out = io::stdout().lock();
    let res = match ui.format {
        Format::Json => write_document(out, WORKTREE_SCHEMA, &report),
        Format::Ndjson => NdjsonWriter::new(out, WORKTREE_SCHEMA).record("worktree", &report),
        Format::Human => write_text(out, &render_one(&report, ui.color_out)),
    };
    res.or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))?;
    cancel::check()
}

fn write_text<W: Write>(mut out: W, text: &str) -> io::Result<()> {
    out.write_all(text.as_bytes())?;
    out.flush()
}

fn kind_label(k: WorktreeKind) -> &'static str {
    match k {
        WorktreeKind::Main => "main",
        WorktreeKind::Bare => "bare",
        WorktreeKind::Linked => "linked",
    }
}

fn head_label(w: &WorktreeReport) -> String {
    match w.state.head.value() {
        Some(HeadState::Branch { name, .. }) => {
            name.strip_prefix("refs/heads/").unwrap_or(name).to_owned()
        }
        Some(HeadState::Detached { oid }) => {
            format!("detached {}", oid.get(..12).unwrap_or(oid))
        }
        None if w.state.missing => "(missing)".into(),
        None => "(unknown)".into(),
    }
}

/// A short state summary: missing, locked, dirty/untracked counts, unpushed, merge.
fn state_label(w: &WorktreeReport) -> String {
    let s = &w.state;
    if s.missing {
        return "folder missing (stale registration)".into();
    }
    let mut parts = Vec::new();
    if let LockState::Locked { .. } = s.lock {
        parts.push("locked".to_owned());
    }
    let count = |o: &Observed<scull_git::PathList>, what: &str| match o.value() {
        Some(l) if l.total > 0 => Some(format!("{} {what}", l.total)),
        Some(_) => None,
        None => Some(format!("{what} unknown")),
    };
    parts.extend(count(&s.tracked_changes, "changed"));
    parts.extend(count(&s.untracked, "untracked"));
    if let Some(c) = s.unpushed.value()
        && c.count > 0
    {
        parts.push(format!("{} unpushed", c.count));
    }
    parts.push(
        match &s.merge {
            MergeStatus::AncestorOfDefault { .. } => "merged",
            MergeStatus::ReachableFromPushedRef { .. } => "pushed",
            MergeStatus::PatchIdOnly { .. } => "merge unproven",
            MergeStatus::NoCommits => "no commits",
            MergeStatus::NotMerged { .. } => "not merged",
            MergeStatus::Unknown { .. } => "merge unknown",
        }
        .to_owned(),
    );
    parts.join(", ")
}

fn deciding_reason(w: &WorktreeReport) -> String {
    w.decision
        .reasons
        .iter()
        .find(|r| r.tier == w.decision.tier)
        .map(|r| r.message.clone())
        .unwrap_or_default()
}

/// The `scull worktrees` table, plus the TTL registrations under the roots.
pub fn render_list(r: &WorktreesReport, registrations: &[TtlRegistration], color: bool) -> String {
    let mut out = String::new();
    if r.cancelled {
        out.push_str("CANCELLED: the walk was stopped before worktrees were analysed\n\n");
    }
    if let [one] = r.roots.as_slice() {
        out.push_str(&format!(
            "Git worktrees under {}\n\n",
            escape_for_terminal(one.as_str())
        ));
    }
    if r.worktrees.is_empty() {
        out.push_str("No Git worktrees found.\n");
    } else {
        let base = match r.roots.as_slice() {
            [one] => Some(one.to_path_buf()),
            _ => None,
        };
        let mut t = Table::new([
            ("Tier", Align::Left),
            ("Kind", Align::Left),
            ("Head", Align::Left),
            ("State", Align::Left),
            ("Allocated", Align::Right),
            ("Path", Align::Left),
            ("Why", Align::Left),
        ]);
        for w in &r.worktrees {
            t.push(vec![
                Cell::tier(w.decision.tier),
                Cell::text(kind_label(w.kind)),
                Cell::text(&head_label(w)),
                Cell::text(&state_label(w)),
                match w.allocated {
                    Some(a) => Cell::bytes(a),
                    None => Cell::text("-"),
                },
                Cell::path(&render::display_path(&w.path, base.as_deref())),
                Cell::text(&deciding_reason(w)),
            ]);
        }
        out.push_str(&t.render(color));
        out.push_str("\nMain and bare entries are never removable; branches are never deleted.\n");
    }
    if !r.problems.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            Style::Warn.paint(
                "Untrusted worktree registrations (treated as protected):",
                color
            )
        ));
        for p in &r.problems {
            out.push_str(&format!(
                "  {}: {}\n",
                escape_for_terminal(p.problem.admin_dir.as_str()),
                escape_for_terminal(&p.problem.detail)
            ));
        }
    }
    if !registrations.is_empty() {
        out.push_str("\nRegistered TTLs (never removed automatically):\n");
        let now = now_unix();
        for g in registrations {
            let state = if g.expired {
                Style::Warn.paint(
                    &format!(
                        "expired {} ago: review suggested",
                        human_duration(u64::try_from(now - g.expires_at).unwrap_or(0))
                    ),
                    color,
                )
            } else {
                format!(
                    "expires in {}",
                    human_duration(u64::try_from(g.expires_at - now).unwrap_or(0))
                )
            };
            out.push_str(&format!(
                "  {}  TTL {}, {}{}\n",
                escape_for_terminal(g.path.as_str()),
                human_duration(g.ttl_secs),
                state,
                if g.found { "" } else { " (folder not found)" }
            ));
        }
    }
    out.push_str(&render::signals_section(&r.signals_unavailable, color));
    out
}

/// The `scull worktrees inspect` view.
pub fn render_one(w: &WorktreeReport, color: bool) -> String {
    let e = |s: &str| escape_for_terminal(s).into_owned();
    let s = &w.state;
    let tier = tier_style(w.decision.tier).paint(&tier_text(w.decision.tier), color);
    let mut out = format!("{tier}  {}\n", e(w.path.as_str()));
    out.push_str(&format!("  kind:         {}\n", kind_label(w.kind)));
    out.push_str(&format!("  repository:   {}\n", e(w.common_dir.as_str())));
    out.push_str(&format!("  head:         {}\n", e(&head_label(w))));
    out.push_str(&format!("  state:        {}\n", e(&state_label(w))));
    if let LockState::Locked { reason } = &s.lock {
        out.push_str(&format!(
            "  locked:       {}\n",
            e(reason.as_deref().unwrap_or("(no reason given)"))
        ));
    }
    if let Some(u) = s.upstream.value() {
        match u {
            Some(u) => out.push_str(&format!("  upstream:     {}\n", e(&u.name))),
            None => out.push_str("  upstream:     none\n"),
        }
    }
    if let Some(a) = w.allocated {
        out.push_str(&format!("  size:         {} allocated\n", format_bytes(a)));
    }
    out.push_str(&format!(
        "  action:       {}\n",
        render::action_label(&w.decision.action)
    ));
    out.push_str(&format!(
        "  artifacts:    cleanup {}\n",
        match w.decision.artifact_cleanup {
            scull_git::ArtifactCleanup::Allowed => "allowed",
            scull_git::ArtifactCleanup::BlockedForThisRun => "blocked for this run (in use)",
            scull_git::ArtifactCleanup::NotApplicable => "not applicable",
        }
    ));
    for a in &w.decision.artifacts {
        out.push_str(&format!(
            "                {} ({})\n",
            e(&a.path),
            e(&a.rule)
        ));
    }
    if !w.decision.unique_ignored.is_empty() {
        out.push_str("  unique ignored files (possibly unique data):\n");
        for f in &w.decision.unique_ignored {
            out.push_str(&format!("    {}\n", e(f)));
        }
    }
    out.push_str("  why:\n");
    for r in &w.decision.reasons {
        out.push_str(&format!("    {} {}\n", r.tier, e(&r.message)));
    }
    out
}
