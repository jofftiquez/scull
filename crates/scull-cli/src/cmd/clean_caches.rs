//! `scull clean caches` (P2-CLI-02; PRD §10.8 QC-02…06, QC-04): Quick Clean from the CLI.
//!
//! Package caches hold no unique data, so they are cleaned with each tool's **own** command
//! (prune by default, `--wipe` for the full wipe where the tool has one), never quarantined
//! and never deleted by scull ([`scull_cleaner::quickclean`]).
//!
//! * **Preview** (the default; `--dry-run` never asks): every located cache as one line per
//!   tool with its size, folder, the exact command with the **resolved binary** (SAFE-12) and
//!   the mode, the not-undoable note ("Not undoable. Packages re-download on next install;
//!   offline installs may fail until then."), and every tool that won't run with the reason
//!   (unticked, not installed, in use or unknown, link / other volume, refused location, tier,
//!   version, shell shim only). `--untick TOOL` leaves a tool alone (FX-QC-08).
//! * **Run**: `--confirm`, or `y` at the single prompt on a terminal (QC-04: one confirmation,
//!   no elevated one; without a terminal nothing runs unless `--confirm`, CLI-03). Elevated
//!   runs are refused unless `--allow-root` (SAFE-14). Under the per-user execution lock
//!   (SAFE-09, exit 6), with a **fresh** activity probe, Quick Clean revalidates each tool
//!   right before starting it (same cache identity, no link, same volume, not in use, same
//!   binary) and runs it with a sanitized environment in a private sandbox created in an
//!   owner-only folder under scull's state directory (not a shared `/tmp`). The run is one
//!   recorded operation (SAFE-08, P2-EXEC-12): the confirmed preview is stored as a plan
//!   (resolved binary and exact arguments per tool) and every tool gets one audit row.
//! * **Result** per tool (QC-06): exit status, cache size before / after, the volume's measured
//!   free-space delta, and for pnpm why a prune may free less than expected. A tool that was
//!   planned but did not complete makes the exit code 3.
//!
//! An orphaned cache whose tool is not installed is listed with "left in place for manual
//! removal": the Quick Clean runner does not remove it (D-36 is provisional and not
//! implemented by the runner yet).
//!
//! Machine output: `scull/clean-caches@1`.

use super::exec;
use super::pipeline::{self, CancelBridge};
use super::{emit_machine, emit_text};
use crate::cancel;
use crate::cli::CleanCachesArgs;
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use crate::output::style::Style;
use crate::output::table::{Align, Cell, Table};
use schemars::JsonSchema;
use scull_cleaner::quickclean::{
    CleanMode, Ended, NOT_UNDOABLE_NOTE, QuickClean, QuickCleanOptions, QuickCleanPlan,
    QuickCleanReport, QuickCleanRequest, SkipReason, SkippedTool, ToolResult,
};
use scull_core::{ErrorKind, FsPath, PlanOrigin, PlanParams, ScanId, ScullError};
use scull_detectors::caches::CacheTool;
use scull_detectors::rules::Platform as RulesOs;
use scull_detectors::{CacheEnv, DetectContext, locate_caches};
use scull_engine::{FsConfigReader, SignalUnavailable, now_unix};
use scull_platform::Native;
use scull_platform::activity::Limits;
use scull_safety::NativeActivityProbe;
use scull_storage::{ApprovalMethod, OperationEnd, OperationStart};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Schema of `scull clean caches --json`.
pub const CLEAN_CACHES_SCHEMA: SchemaId = SchemaId::new("clean-caches", 1);

/// Folder under scull's state directory that holds Quick Clean's private sandboxes.
pub const SANDBOX_DIR: &str = "quickclean";

/// One `--untick` value: the tools it names (`yarn` names both Yarn caches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Untick(pub Vec<CacheTool>);

/// Parse a `--untick` value.
pub fn parse_tool(s: &str) -> Result<Untick, String> {
    let tools = match s.trim().to_ascii_lowercase().as_str() {
        "npm" => vec![CacheTool::Npm],
        "pnpm" => vec![CacheTool::Pnpm],
        "bun" => vec![CacheTool::Bun],
        "yarn" => vec![CacheTool::YarnClassic, CacheTool::YarnBerry],
        "yarn-classic" => vec![CacheTool::YarnClassic],
        "yarn-berry" => vec![CacheTool::YarnBerry],
        _ => {
            return Err("expected one of npm, pnpm, bun, yarn, yarn-classic, yarn-berry".into());
        }
    };
    Ok(Untick(tools))
}

/// Stable short name of a tool (as `--untick` takes it).
pub fn tool_name(t: CacheTool) -> &'static str {
    match t {
        CacheTool::Npm => "npm",
        CacheTool::Pnpm => "pnpm",
        CacheTool::Bun => "bun",
        CacheTool::YarnClassic => "yarn-classic",
        CacheTool::YarnBerry => "yarn-berry",
    }
}

// ---------------------------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------------------------

/// Preview or run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CachesMode {
    /// Nothing ran.
    Preview,
    /// The planned tools were run.
    Run,
}

/// `scull clean caches` (`scull/clean-caches@1`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanCachesReport {
    /// Preview or run.
    pub mode: CachesMode,
    /// The tools that will run (preview) or were planned (run), one line each.
    pub tools: Vec<ToolLine>,
    /// Tools that won't run (or didn't), and why.
    pub skipped: Vec<SkippedLine>,
    /// Run only: what each started tool did.
    pub results: Vec<ResultLine>,
    /// Estimated reclaimable bytes of the planned tools.
    pub total_reclaimable_bytes: u64,
    /// "Not undoable. Packages re-download on next install; offline installs may fail until
    /// then." (QC-02).
    pub not_undoable: String,
    /// Signals the cache scan could not observe.
    pub signals_unavailable: Vec<SignalUnavailable>,
    /// What to run next, if anything.
    pub next_step: Option<String>,
}

/// One planned tool.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolLine {
    /// Short name (`npm`, `pnpm`, `bun`, `yarn-classic`, `yarn-berry`).
    pub tool: String,
    /// What it is ("npm cache").
    pub artifact: String,
    /// The cache folder.
    pub cache: FsPath,
    /// `prune` or `wipe`.
    pub mode: String,
    /// Whether the wipe was asked for (`--wipe`); Bun and Yarn only have a wipe.
    pub wipe_requested: bool,
    /// The resolved binary (SAFE-12).
    pub binary: FsPath,
    /// The exact arguments.
    pub args: Vec<String>,
    /// Location variables added to the sanitized environment.
    pub env: Vec<(String, String)>,
    /// The command as shown (display only; never run through a shell).
    pub command_line: String,
    /// Estimated reclaimable bytes.
    pub reclaimable_bytes: u64,
    /// Extra facts (e.g. "pnpm has no wipe").
    pub notes: Vec<String>,
}

/// One tool that won't run.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkippedLine {
    /// Short name.
    pub tool: String,
    /// Its cache folder, if known.
    pub path: Option<FsPath>,
    /// Stable reason code.
    pub reason: String,
    /// Why, in words.
    pub message: String,
    /// The folder left for manual removal when the tool isn't installed.
    pub manual_removal: Option<FsPath>,
}

/// What one started tool did.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultLine {
    /// Short name.
    pub tool: String,
    /// The cache folder.
    pub cache: FsPath,
    /// The binary that ran.
    pub binary: FsPath,
    /// Its arguments.
    pub args: Vec<String>,
    /// `prune` or `wipe`.
    pub mode: String,
    /// `exited`, `timed_out`, `cancelled` or `spawn_failed`.
    pub ended: String,
    /// Exit code, when it exited.
    pub exit_code: Option<i32>,
    /// Whether it exited with status 0.
    pub succeeded: bool,
    /// Wall-clock milliseconds.
    pub elapsed_ms: u64,
    /// Cache allocated bytes before.
    pub cache_bytes_before: Option<u64>,
    /// Cache allocated bytes after.
    pub cache_bytes_after: Option<u64>,
    /// Allocated bytes the cache shrank by (negative: it grew).
    pub cache_bytes_freed: Option<i64>,
    /// Measured change of the volume's free space (positive: freed; other activity shows too).
    pub free_space_delta: Option<i64>,
    /// Why less was freed than one might expect (pnpm, QC-06).
    pub explanation: Option<String>,
    /// Captured stdout (escaped, capped).
    pub stdout: Vec<String>,
    /// Captured stderr (escaped, capped).
    pub stderr: Vec<String>,
}

fn skip_code(r: &SkipReason) -> &'static str {
    match r {
        SkipReason::Refused(_) => "refused",
        SkipReason::Unticked => "unticked",
        SkipReason::Problem(_) => "location_problem",
        SkipReason::VolumeUnverified => "volume_unverified",
        SkipReason::Unrecognized => "unrecognized",
        SkipReason::NotClassified => "not_classified",
        SkipReason::NotEligible { .. } => "not_eligible",
        SkipReason::UnexpectedLayout(_) => "unexpected_layout",
        SkipReason::InUse(_) => "in_use",
        SkipReason::ActivityUnknown(_) => "activity_unknown",
        SkipReason::NotInstalled { .. } => "not_installed",
        SkipReason::ShellShimOnly(_) => "shell_shim_only",
        SkipReason::WrongVersion { .. } => "wrong_version",
        SkipReason::Changed(_) => "changed",
        SkipReason::Elevated(_) => "elevated",
        SkipReason::Cancelled => "cancelled",
        SkipReason::Sandbox(_) => "sandbox",
        SkipReason::NotApproved => "not_approved",
    }
}

fn skipped_line(s: &SkippedTool) -> SkippedLine {
    SkippedLine {
        tool: tool_name(s.tool).to_owned(),
        path: s.path.as_deref().map(FsPath::from_path),
        reason: skip_code(&s.reason).to_owned(),
        message: s.reason.to_string(),
        manual_removal: match &s.reason {
            SkipReason::NotInstalled { manual_removal } => Some(FsPath::from_path(manual_removal)),
            _ => None,
        },
    }
}

fn mode_name(m: CleanMode) -> String {
    m.to_string()
}

fn tool_lines(plan: &QuickCleanPlan) -> Vec<ToolLine> {
    plan.tools
        .iter()
        .map(|t| ToolLine {
            tool: tool_name(t.tool).to_owned(),
            artifact: t.tool.artifact().to_owned(),
            cache: FsPath::from_path(&t.cache),
            mode: mode_name(t.mode),
            wipe_requested: t.wipe_requested,
            binary: FsPath::from_path(&t.binary),
            args: t
                .args
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            env: t
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.to_string_lossy().into_owned()))
                .collect(),
            command_line: t.command_line(),
            reclaimable_bytes: t.reclaimable_bytes,
            notes: t.notes.clone(),
        })
        .collect()
}

fn clamp(v: i128) -> i64 {
    i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
}

fn result_line(r: &ToolResult) -> ResultLine {
    let (ended, exit_code) = match &r.ended {
        Ended::Exited(code) => ("exited", *code),
        Ended::TimedOut(_) => ("timed_out", None),
        Ended::Cancelled => ("cancelled", None),
        Ended::SpawnFailed(_) => ("spawn_failed", None),
        #[allow(unreachable_patterns)]
        _ => ("other", None),
    };
    ResultLine {
        tool: tool_name(r.tool).to_owned(),
        cache: FsPath::from_path(&r.cache),
        binary: FsPath::from_path(&r.binary),
        args: r
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        mode: mode_name(r.mode),
        ended: ended.to_owned(),
        exit_code,
        succeeded: r.succeeded(),
        elapsed_ms: u64::try_from(r.elapsed.as_millis()).unwrap_or(u64::MAX),
        cache_bytes_before: r.size_before.map(|s| s.allocated),
        cache_bytes_after: r.size_after.map(|s| s.allocated),
        cache_bytes_freed: r.cache_bytes_freed().map(clamp),
        free_space_delta: r.free_space_delta().map(clamp),
        explanation: r.explanation.clone(),
        stdout: r.stdout.lines.clone(),
        stderr: r.stderr.lines.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------------------------

/// Run `scull clean caches`.
pub fn run(args: &CleanCachesArgs, ui: &Ui) -> Result<(), ScullError> {
    let wants_run = args.confirm || (!args.dry_run && exec::interactive(ui));
    let elevation = exec::elevation();
    if args.confirm {
        exec::check_not_elevated(elevation, args.allow_root, "scull clean caches")?;
    }
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
    let caches = engine.caches(
        &env,
        loaded.config.scan.threads as usize,
        bridge.token(),
        pipeline::progress(ui),
    );
    drop(bridge);
    if caches.cancelled || cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    let reader = FsConfigReader;
    let ctx = DetectContext::new().with_config_reader(&reader);
    let located = locate_caches(&env, &Native, &ctx);

    let mut opts = QuickCleanOptions::new(std::env::var_os("PATH").unwrap_or_default());
    opts.cwd = std::env::current_dir().ok();
    opts.excluded_roots = loaded.config.scan.roots.clone();
    opts.sandbox_base = Some(sandbox_base()?);
    opts.allow_elevated = args.allow_root;
    let qc = QuickClean::new(opts, Arc::new(Native)).with_elevation(elevation);
    let mut request = QuickCleanRequest::default();
    for u in &args.untick {
        request.unticked.extend(u.0.iter().copied());
    }
    if args.wipe {
        request.wipe = CacheTool::ALL.into_iter().collect::<BTreeSet<_>>();
    }
    let probe = NativeActivityProbe::capture(Limits::default());
    let plan = qc.plan(&located, &caches.candidates, &request, &probe);

    let mut report = preview(&plan, caches.signals_unavailable.clone());

    if plan.tools.is_empty() || !wants_run {
        if !plan.tools.is_empty() {
            report.next_step = Some(format!(
                "Nothing was run. To run {}: scull clean caches --confirm{}",
                if plan.tools.len() == 1 { "it" } else { "them" },
                if args.wipe { " --wipe" } else { "" }
            ));
        }
        return emit(&report, ui);
    }
    if !args.confirm {
        // Interactive: show the preview, then one confirmation (QC-04).
        exec::check_not_elevated(elevation, args.allow_root, "scull clean caches")?;
        emit(&report, ui)?;
        let n = plan.tools.len();
        if !exec::prompt_yes_no(&format!(
            "Run {n} cleanup command{}? This is not undoable.",
            if n == 1 { "" } else { "s" }
        ))? {
            info(ui, "Nothing was run.");
            return Err(cancel::cancelled_error());
        }
    }

    // SAFE-09: one executor at a time; reconcile interrupted operations first (SAFE-19).
    let mut store = exec::open_store()?;
    let executor = exec::lock_and_recover(&mut store, ui)?;
    // SAFE-08 (P2-EXEC-12): the confirmed preview is recorded as a plan (resolved binary and
    // exact arguments per tool) and run as one audited operation.
    let who = exec::who()?;
    let now = now_unix();
    let audit_plan = plan.audit_plan(
        &BTreeSet::new(),
        PlanParams {
            id: exec::new_plan_id(now)?,
            scan_id: ScanId::from_suffix(&exec::new_id_suffix(now))
                .map_err(|e| ScullError::new(ErrorKind::Internal, e))?,
            created_unix: now,
            ttl_secs: None,
            machine_id: who.machine_id.clone(),
            user: who.user.clone(),
            origin: PlanOrigin::Cli,
        },
    )?;
    store.insert_plan(&audit_plan)?;
    let mut op = executor.begin_operation(
        &mut store,
        &audit_plan,
        &OperationStart {
            operation_id: exec::new_operation_id(now)?,
            plan_id: audit_plan.id().clone(),
            content_hash: audit_plan.content_hash().clone(),
            approver: who.user.clone(),
            method: ApprovalMethod::CliInteractive,
            started_at: now,
        },
    )?;
    if ui.progress {
        ui.progress(&format!("running {} tool(s)...", plan.tools.len()));
    }
    let fresh = NativeActivityProbe::capture(Limits::default());
    let bridge = CancelBridge::start();
    let ran: QuickCleanReport = qc.run(&mut op, &plan, &fresh, bridge.token())?;
    drop(bridge);
    let end = if ran.cancelled {
        OperationEnd::Cancelled
    } else {
        OperationEnd::Normal
    };
    op.finish(end, now_unix())?;
    let failed = apply_run(&mut report, &plan, &ran);
    emit(&report, ui)?;

    if cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    if failed > 0 {
        return Err(ScullError::new(
            ErrorKind::PartialFailure,
            format!(
                "{failed} of {} planned tool(s) did not complete; each is listed with the reason",
                plan.tools.len()
            ),
        ));
    }
    Ok(())
}

/// The preview report of `plan`.
pub fn preview(plan: &QuickCleanPlan, signals: Vec<SignalUnavailable>) -> CleanCachesReport {
    CleanCachesReport {
        mode: CachesMode::Preview,
        tools: tool_lines(plan),
        skipped: plan.skipped.iter().map(skipped_line).collect(),
        results: Vec::new(),
        total_reclaimable_bytes: plan.total_reclaimable(),
        not_undoable: NOT_UNDOABLE_NOTE.to_owned(),
        signals_unavailable: signals,
        next_step: None,
    }
}

/// Turn `report` into the run report of `ran`; returns how many planned tools did not
/// complete (failed, or skipped at run time for any reason but unticking).
pub fn apply_run(
    report: &mut CleanCachesReport,
    plan: &QuickCleanPlan,
    ran: &QuickCleanReport,
) -> usize {
    report.mode = CachesMode::Run;
    report.next_step = None;
    report.skipped = ran.skipped.iter().map(skipped_line).collect();
    report.results = ran.results.iter().map(result_line).collect();
    let planned: BTreeSet<CacheTool> = plan.tools.iter().map(|t| t.tool).collect();
    ran.results.iter().filter(|r| !r.succeeded()).count()
        + ran
            .skipped
            .iter()
            .filter(|s| planned.contains(&s.tool) && !matches!(s.reason, SkipReason::Unticked))
            .count()
}

/// The owner-only folder under scull's state directory that holds Quick Clean's private
/// sandboxes (never the shared system temp folder).
fn sandbox_base() -> Result<PathBuf, ScullError> {
    let dir = exec::dirs()?.state.join(SANDBOX_DIR);
    scull_platform::create_dir_owner_only(&dir).map_err(|e| {
        ScullError::new(
            ErrorKind::Io,
            format!("cannot create the Quick Clean sandbox folder: {e}"),
        )
    })?;
    scull_platform::ensure_owner_only(&dir).map_err(|e| {
        ScullError::new(
            ErrorKind::SafetyBlocked,
            format!("the Quick Clean sandbox folder is not private: {e}"),
        )
        .with_path(FsPath::from_path(&dir))
    })?;
    Ok(dir)
}

fn info(ui: &Ui, msg: &str) {
    if !ui.quiet {
        eprintln!("{}", escape_for_terminal(msg));
    }
}

fn emit(r: &CleanCachesReport, ui: &Ui) -> Result<(), ScullError> {
    if ui.format.is_machine() {
        emit_machine(ui.format, CLEAN_CACHES_SCHEMA, "caches", r)
    } else {
        emit_text(&render(r, ui.color_out))
    }
}

fn signed_bytes(v: Option<i64>) -> String {
    match v {
        None => "unknown".into(),
        Some(b) if b < 0 => format!("-{}", format_bytes(b.unsigned_abs())),
        Some(b) => format!("+{}", format_bytes(b.unsigned_abs())),
    }
}

fn short(p: &Path) -> String {
    escape_for_terminal(&p.to_string_lossy()).into_owned()
}

/// The human output.
pub fn render(r: &CleanCachesReport, color: bool) -> String {
    let mut out = String::new();
    match r.mode {
        CachesMode::Preview => out.push_str(&format!(
            "{}\n\n",
            Style::Bold.paint("Quick Clean: package caches (nothing has been run)", color)
        )),
        CachesMode::Run => out.push_str(&format!(
            "{}\n\n",
            Style::Bold.paint("Quick Clean: package caches", color)
        )),
    }
    if r.tools.is_empty() {
        out.push_str("No package cache can be cleaned right now.\n");
    } else if r.mode == CachesMode::Preview {
        let mut t = Table::new([
            ("Tool", Align::Left),
            ("Mode", Align::Left),
            ("Reclaimable", Align::Right),
            ("Cache", Align::Left),
        ]);
        for l in &r.tools {
            t.push(vec![
                Cell::text(&l.tool),
                Cell::text(&l.mode),
                Cell::bytes(l.reclaimable_bytes),
                Cell::path(l.cache.as_str()),
            ]);
        }
        out.push_str(&t.render(color));
        out.push_str("\nCommands (no shell; sanitized environment; private working directory):\n");
        for l in &r.tools {
            out.push_str(&format!(
                "  {:<13} {}\n",
                l.tool,
                escape_for_terminal(&l.command_line)
            ));
            for n in &l.notes {
                out.push_str(&format!("  {:<13} note: {}\n", "", escape_for_terminal(n)));
            }
        }
        out.push_str(&format!(
            "\nTotal: {} reclaimable from {} tool(s).\n",
            format_bytes(r.total_reclaimable_bytes),
            r.tools.len()
        ));
        out.push_str(&format!(
            "{}\n",
            Style::Warn.paint(&escape_for_terminal(&r.not_undoable), color)
        ));
    }
    if !r.results.is_empty() {
        let mut t = Table::new([
            ("Tool", Align::Left),
            ("Mode", Align::Left),
            ("Result", Align::Left),
            ("Cache before", Align::Right),
            ("Cache after", Align::Right),
            ("Free space", Align::Right),
        ]);
        for x in &r.results {
            let result = match (x.ended.as_str(), x.exit_code) {
                ("exited", Some(0)) => "ok".to_owned(),
                ("exited", Some(c)) => format!("FAILED (exit {c})"),
                ("exited", None) => "killed by a signal".to_owned(),
                (other, _) => other.replace('_', " ").to_uppercase(),
            };
            t.push(vec![
                Cell::text(&x.tool),
                Cell::text(&x.mode),
                Cell::text(&result).styled(if x.succeeded {
                    Style::Plain
                } else {
                    Style::Warn
                }),
                Cell::text(&x.cache_bytes_before.map_or("unknown".into(), format_bytes)),
                Cell::text(&x.cache_bytes_after.map_or("unknown".into(), format_bytes)),
                Cell::text(&signed_bytes(x.free_space_delta)),
            ]);
        }
        out.push_str(&t.render(color));
        for x in &r.results {
            out.push_str(&format!(
                "  {}: ran {} ({} ms); cache shrank by {}\n",
                x.tool,
                escape_for_terminal(x.binary.as_str()),
                x.elapsed_ms,
                signed_bytes(x.cache_bytes_freed)
            ));
            if let Some(e) = &x.explanation {
                out.push_str(&format!("    {}\n", escape_for_terminal(e)));
            }
            if !x.succeeded {
                for l in x.stderr.iter().take(5) {
                    out.push_str(&format!("    stderr: {l}\n"));
                }
            }
        }
    }
    if !r.skipped.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            Style::Bold.paint(&format!("Not run ({}):", r.skipped.len()), color)
        ));
        for s in &r.skipped {
            // A tool that isn't installed names its folder in the message already.
            let path = s
                .path
                .as_ref()
                .filter(|_| s.manual_removal.is_none())
                .map(|p| format!("{}: ", short(&p.to_path_buf())))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {:<13} {path}{}\n",
                s.tool,
                escape_for_terminal(&s.message)
            ));
        }
    }
    if !r.signals_unavailable.is_empty() {
        out.push_str(&super::render::signals_section(
            &r.signals_unavailable,
            color,
        ));
    }
    if let Some(n) = &r.next_step {
        out.push_str(&format!("\n{}\n", escape_for_terminal(n)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untick_names_parse() {
        assert_eq!(parse_tool("npm").unwrap().0, vec![CacheTool::Npm]);
        assert_eq!(parse_tool(" PNPM ").unwrap().0, vec![CacheTool::Pnpm]);
        assert_eq!(parse_tool("yarn").unwrap().0.len(), 2);
        assert_eq!(
            parse_tool("yarn-berry").unwrap().0,
            vec![CacheTool::YarnBerry]
        );
        assert!(parse_tool("cargo").is_err());
        for t in CacheTool::ALL {
            assert_eq!(parse_tool(tool_name(t)).unwrap().0, vec![t]);
        }
    }
}
