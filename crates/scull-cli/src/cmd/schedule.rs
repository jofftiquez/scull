//! `scull schedule enable|disable|status` (P4-PRO-03, PRO-07): register a periodic, low-priority
//! `scull check` with the OS scheduler (launchd LaunchAgent, systemd user timer, or Task
//! Scheduler), as the current user.
//!
//! **Consent.** `enable` and `disable` first print every file they will write or remove (with
//! its full contents) and every command they will run, then:
//!
//! * `--dry-run`: stop there, nothing changes;
//! * `--yes`: apply;
//! * otherwise ask `[y/N]` on a terminal; without one (and always with `--json`/`--ndjson`)
//!   refuse with a usage error (exit 2) and change nothing.
//!
//! **Never as root.** Applying is refused from an elevated shell (root / an elevated
//! administrator token, or when elevation can't be determined): the job must run as the user
//! it belongs to. `--dry-run` and `status` still work.
//!
//! **Read-only job.** The registered command is `scull check --quiet --no-progress` (see
//! [`scull_proactive::schedule`] for the exact units). It never deletes anything (an
//! auto-policy the user enabled in the config file only moves S0 items into quarantine, D-11).
//!
//! The files, commands and tool paths come from [`scull_proactive::schedule`]; the tools are
//! run through [`scull_platform::scheduler::CommandRunner`], injected so tests use a fake and
//! never register a real schedule. `status` only runs its (read-only) query when scull's unit
//! files are present.
//!
//! Machine output: `scull/schedule@1` ([`ScheduleReport`]).

use super::{emit_machine, emit_text};
use crate::cli::{ConsentArgs, ScheduleAction};
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use crate::output::{Format, Ui};
use schemars::JsonSchema;
use scull_core::{ErrorKind, ScullError};
use scull_platform::Elevation;
use scull_platform::scheduler::{CommandRunner, Scheduler, SystemRunner};
use scull_proactive::schedule::{
    self as plan, CommandSpec, InstallPlan, MAX_INTERVAL_SECS, MIN_INTERVAL_SECS, RemovePlan,
    ScheduleSpec,
};
use serde::Serialize;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Schema of `scull schedule … --json`.
pub const SCHEDULE_SCHEMA: SchemaId = SchemaId::new("schedule", 1);

/// Which subcommand produced a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleOp {
    /// `scull schedule enable`.
    Enable,
    /// `scull schedule disable`.
    Disable,
    /// `scull schedule status`.
    Status,
}

/// What happens (or happened) to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileChange {
    /// Written (created or replaced) by `enable`.
    Write,
    /// Removed by `disable`.
    Remove,
    /// Present (`status`).
    Present,
    /// Missing (`status`).
    Missing,
}

/// One unit file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileReport {
    /// The file.
    pub path: String,
    /// What happens to it.
    pub change: FileChange,
    /// The full contents written (`enable` only).
    pub contents: Option<String>,
    /// Whether a file was already there (`enable`: it is replaced).
    pub existed: bool,
}

/// One scheduler tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandReport {
    /// The command line (absolute tool path, no shell).
    pub command: String,
    /// Whether it was run (false for `--dry-run` and refusals).
    pub ran: bool,
    /// Its exit code, when run.
    pub exit_code: Option<i32>,
    /// Whether a failure stops the operation (false for best-effort steps).
    pub required: bool,
    /// What it printed (trimmed), when run.
    pub output: Option<String>,
}

/// `scull schedule` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleReport {
    /// The subcommand.
    pub operation: ScheduleOp,
    /// `launchd`, `systemd` or `task_scheduler`.
    pub scheduler: String,
    /// The scheduled command line (`scull check --quiet --no-progress`).
    pub job: String,
    /// Seconds between checks (`enable`).
    pub interval_secs: Option<u64>,
    /// `--dry-run`: nothing was changed.
    pub dry_run: bool,
    /// Whether the change was applied.
    pub applied: bool,
    /// Whether scull's schedule is registered (after the operation; for `status`, as found).
    pub registered: bool,
    /// The unit files.
    pub files: Vec<FileReport>,
    /// The tool invocations, in order.
    pub commands: Vec<CommandReport>,
    /// Caveats.
    pub notes: Vec<String>,
}

/// Where and as whom `scull schedule` acts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    /// The OS scheduler.
    pub scheduler: Scheduler,
    /// Where unit files go ([`scull_proactive::schedule::unit_dir`]).
    pub unit_dir: PathBuf,
    /// The `scull` executable the job runs.
    pub exe: PathBuf,
    /// `SCULL_HOME`, when set (passed to the job).
    pub scull_home: Option<PathBuf>,
    /// Whether this process runs elevated.
    pub elevation: Elevation,
}

impl Env {
    /// The real environment: this OS's scheduler, the user's unit folder, this executable.
    pub fn from_system() -> Result<Self, ScullError> {
        let scheduler = Scheduler::native().ok_or_else(|| {
            ScullError::new(
                ErrorKind::UnsupportedPlatform,
                "no supported scheduler on this OS (launchd, systemd user timers or Task \
                 Scheduler)",
            )
        })?;
        let home = scull_safety::protect::home_from_env()
            .ok_or_else(|| ScullError::new(ErrorKind::Config, "cannot locate your home folder"))?;
        let dirs = scull_platform::app_dirs()
            .map_err(|e| ScullError::new(ErrorKind::Config, e.to_string()))?;
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let exe = std::env::current_exe().map_err(|e| {
            ScullError::new(
                ErrorKind::Io,
                format!("cannot tell where the scull executable is: {e}"),
            )
        })?;
        Ok(Self {
            scheduler,
            unit_dir: plan::unit_dir(scheduler, &home, xdg.as_deref(), &dirs.data),
            exe,
            scull_home: std::env::var_os(scull_platform::HOME_OVERRIDE_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            elevation: scull_platform::is_elevated(),
        })
    }

    fn spec(&self, interval_secs: u64) -> ScheduleSpec {
        ScheduleSpec {
            scheduler: self.scheduler,
            exe: self.exe.clone(),
            interval_secs,
            unit_dir: self.unit_dir.clone(),
            scull_home: self.scull_home.clone(),
        }
    }

    fn job(&self) -> String {
        CommandSpec {
            program: self.exe.clone(),
            args: plan::CHECK_ARGS.iter().map(|a| (*a).to_owned()).collect(),
            must_succeed: true,
        }
        .display()
    }

    fn files_present(&self) -> Vec<(PathBuf, bool)> {
        plan::unit_files(self.scheduler, &self.unit_dir)
            .into_iter()
            .map(|p| {
                let present = std::fs::symlink_metadata(&p).is_ok();
                (p, present)
            })
            .collect()
    }
}

/// `--every`: 15 minutes to 7 days.
pub fn parse_interval(s: &str) -> Result<u64, ScullError> {
    let secs = scull_mcp::registry::parse_ttl(s)
        .map_err(|e| ScullError::new(ErrorKind::Usage, format!("--every {e}")))?;
    if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&secs) {
        return Err(ScullError::new(
            ErrorKind::Usage,
            format!("--every {s}: choose between 15m and 7d"),
        ));
    }
    Ok(secs)
}

/// `scull schedule …`.
pub fn run(action: &ScheduleAction, ui: &Ui) -> Result<(), ScullError> {
    let env = Env::from_system()?;
    let runner = SystemRunner;
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    let report = match action {
        ScheduleAction::Status => status(&env, &runner),
        ScheduleAction::Enable { every, consent } => {
            let interval = parse_interval(every)?;
            let (mut report, install) = prepare_enable(&env, interval);
            report.dry_run = consent.dry_run;
            let proceed = review(&report, &env, *consent, interactive, ui)?;
            if proceed {
                apply_enable(report, &install, &runner)?
            } else {
                report
            }
        }
        ScheduleAction::Disable { consent } => {
            let (mut report, remove) = prepare_disable(&env);
            report.dry_run = consent.dry_run;
            let proceed = report.registered && review(&report, &env, *consent, interactive, ui)?;
            if proceed {
                apply_disable(report, &remove, &runner)?
            } else {
                if !report.registered && ui.format == Format::Human {
                    emit_text(&render(&report))?;
                }
                report
            }
        }
    };
    if ui.format.is_machine() {
        return emit_machine(ui.format, SCHEDULE_SCHEMA, "schedule", &report);
    }
    let nothing_to_do = report.operation == ScheduleOp::Disable && !report.registered;
    if report.dry_run && !nothing_to_do {
        emit_text("Dry run: nothing was changed.\n")?;
    }
    if report.applied || report.operation == ScheduleOp::Status {
        emit_text(&escaped(&render_outcome(&report)))?;
    }
    Ok(())
}

/// Show the plan (human mode) and decide whether to apply it (see the module docs). `Ok(false)`
/// means stop without error (`--dry-run`).
fn review(
    report: &ScheduleReport,
    env: &Env,
    consent: ConsentArgs,
    interactive: bool,
    ui: &Ui,
) -> Result<bool, ScullError> {
    let human = ui.format == Format::Human;
    if human {
        emit_text(&escaped(&render(report)))?;
    }
    let pending = report.files.len() + report.commands.len();
    match gate(consent, human, interactive, env.elevation, pending)? {
        Gate::Stop => Ok(false),
        Gate::Apply => Ok(true),
        Gate::Ask => confirm(report.operation),
    }
}

/// What to do after showing the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// `--dry-run`: stop, nothing changes.
    Stop,
    /// `--yes`: apply.
    Apply,
    /// Ask on the terminal.
    Ask,
}

/// The consent and elevation rules (module docs), in order: `--dry-run` stops; without `--yes`
/// a machine format or a missing terminal refuses (usage, exit 2); an elevated process is
/// refused; then `--yes` applies, otherwise ask.
pub fn gate(
    consent: ConsentArgs,
    human: bool,
    interactive: bool,
    elevation: Elevation,
    pending: usize,
) -> Result<Gate, ScullError> {
    if consent.dry_run {
        return Ok(Gate::Stop);
    }
    if !consent.yes {
        if !human {
            return Err(ScullError::new(
                ErrorKind::Usage,
                "--json and --ndjson never prompt: add --dry-run to review, or --yes to apply",
            ));
        }
        if !interactive {
            return Err(ScullError::new(
                ErrorKind::Usage,
                format!(
                    "not changing anything without confirmation: review the files and commands \
                     above, then rerun with --yes (or run in a terminal to answer the prompt); \
                     nothing was changed ({pending} step{} pending)",
                    if pending == 1 { "" } else { "s" }
                ),
            ));
        }
    }
    if elevation != Elevation::NotElevated {
        return Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            "refusing to change the schedule from an elevated shell (root or administrator): \
             the periodic check must belong to, and run as, your own account; rerun without \
             sudo / from a normal shell (nothing was changed)",
        ));
    }
    Ok(if consent.yes { Gate::Apply } else { Gate::Ask })
}

fn confirm(op: ScheduleOp) -> Result<bool, ScullError> {
    let question = match op {
        ScheduleOp::Disable => "Remove the periodic check?",
        _ => "Register the periodic check?",
    };
    let mut err = io::stderr().lock();
    let _ = write!(err, "{question} [y/N] ");
    let _ = err.flush();
    drop(err);
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| ScullError::from_io(&e))?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(true)
    } else {
        Err(ScullError::new(ErrorKind::Cancelled, "nothing was changed"))
    }
}

fn command_report(c: &CommandSpec) -> CommandReport {
    CommandReport {
        command: c.display(),
        ran: false,
        exit_code: None,
        required: c.must_succeed,
        output: None,
    }
}

/// The plan of `enable` (nothing is changed).
pub fn prepare_enable(env: &Env, interval_secs: u64) -> (ScheduleReport, InstallPlan) {
    let install = plan::install_plan(&env.spec(interval_secs));
    let report = ScheduleReport {
        operation: ScheduleOp::Enable,
        scheduler: env.scheduler.as_str().to_owned(),
        job: env.job(),
        interval_secs: Some(interval_secs),
        dry_run: false,
        applied: false,
        registered: false,
        files: install
            .files
            .iter()
            .map(|f| FileReport {
                path: f.path.to_string_lossy().into_owned(),
                change: FileChange::Write,
                contents: Some(f.text.clone()),
                existed: std::fs::symlink_metadata(&f.path).is_ok(),
            })
            .collect(),
        commands: install.commands.iter().map(command_report).collect(),
        notes: install.notes.clone(),
    };
    (report, install)
}

/// Write the unit files and run the registration commands through `runner`.
pub fn apply_enable(
    mut report: ScheduleReport,
    install: &InstallPlan,
    runner: &dyn CommandRunner,
) -> Result<ScheduleReport, ScullError> {
    for f in &install.files {
        write_unit_file(&f.path, &f.bytes)?;
    }
    run_all(&install.commands, &mut report.commands, 0, runner)?;
    report.applied = true;
    report.registered = true;
    Ok(report)
}

/// Write one unit file, refusing to write through a link or over a non-file.
fn write_unit_file(path: &Path, bytes: &[u8]) -> Result<(), ScullError> {
    if let Ok(md) = std::fs::symlink_metadata(path)
        && !md.file_type().is_file()
    {
        return Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            format!(
                "{} is not a regular file (links are never written through); remove it and \
                 rerun",
                path.display()
            ),
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_at(path, &e))?;
    }
    std::fs::write(path, bytes).map_err(|e| io_at(path, &e))
}

fn io_at(path: &Path, e: &io::Error) -> ScullError {
    ScullError::new(
        ScullError::from_io(e).kind,
        format!("{}: {e}", path.display()),
    )
}

/// Run `specs` in order, filling `reports[offset..]`. A required step that fails (or can't be
/// started) stops with an error naming it.
fn run_all(
    specs: &[CommandSpec],
    reports: &mut [CommandReport],
    offset: usize,
    runner: &dyn CommandRunner,
) -> Result<(), ScullError> {
    for (i, c) in specs.iter().enumerate() {
        let out = runner.run(&c.program, &c.args);
        let r = &mut reports[offset + i];
        r.ran = true;
        match out {
            Ok(o) => {
                r.exit_code = o.code;
                let text = format!("{}{}", o.stdout, o.stderr).trim().to_owned();
                r.output = (!text.is_empty()).then_some(text.clone());
                if c.must_succeed && !o.success() {
                    return Err(ScullError::new(
                        ErrorKind::Io,
                        format!(
                            "`{}` failed (exit {}): {}; files written so far stay in place, \
                             `scull schedule disable` removes them",
                            r.command,
                            o.code.map_or("signal".to_owned(), |c| c.to_string()),
                            if text.is_empty() { "no output" } else { &text }
                        ),
                    ));
                }
            }
            Err(e) if c.must_succeed => {
                return Err(ScullError::new(
                    ErrorKind::Io,
                    format!("cannot run `{}`: {e}", r.command),
                ));
            }
            Err(e) => r.output = Some(e.to_string()),
        }
    }
    Ok(())
}

/// The plan of `disable` (nothing is changed). `registered` is false when none of scull's unit
/// files exist (then there is nothing to do and no command is listed).
pub fn prepare_disable(env: &Env) -> (ScheduleReport, RemovePlan) {
    let remove = plan::remove_plan(env.scheduler, &env.unit_dir);
    let present = env.files_present();
    let any = present.iter().any(|(_, p)| *p);
    let report = ScheduleReport {
        operation: ScheduleOp::Disable,
        scheduler: env.scheduler.as_str().to_owned(),
        job: env.job(),
        interval_secs: None,
        dry_run: false,
        applied: false,
        registered: any,
        files: if any {
            present
                .iter()
                .filter(|(_, p)| *p)
                .map(|(path, _)| FileReport {
                    path: path.to_string_lossy().into_owned(),
                    change: FileChange::Remove,
                    contents: None,
                    existed: true,
                })
                .collect()
        } else {
            Vec::new()
        },
        commands: if any {
            remove
                .before
                .iter()
                .chain(&remove.after)
                .map(command_report)
                .collect()
        } else {
            Vec::new()
        },
        notes: Vec::new(),
    };
    (report, remove)
}

/// Unregister, remove scull's unit files (only those), then run the clean-up commands.
pub fn apply_disable(
    mut report: ScheduleReport,
    remove: &RemovePlan,
    runner: &dyn CommandRunner,
) -> Result<ScheduleReport, ScullError> {
    run_all(&remove.before, &mut report.commands, 0, runner)?;
    for p in &remove.remove {
        match std::fs::symlink_metadata(p) {
            Ok(_) => std::fs::remove_file(p).map_err(|e| io_at(p, &e))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_at(p, &e)),
        }
    }
    let offset = remove.before.len();
    run_all(&remove.after, &mut report.commands, offset, runner)?;
    report.applied = true;
    report.registered = false;
    Ok(report)
}

/// `status`: which unit files exist and, when they do, the scheduler's (read-only) answer.
pub fn status(env: &Env, runner: &dyn CommandRunner) -> ScheduleReport {
    let present = env.files_present();
    let all = present.iter().all(|(_, p)| *p);
    let any = present.iter().any(|(_, p)| *p);
    let mut commands = Vec::new();
    let mut registered = all;
    if any {
        let q = plan::status_command(env.scheduler);
        let mut r = vec![command_report(&q)];
        // Not required: a failed query is reported, not fatal.
        let _ = run_all(std::slice::from_ref(&q), &mut r, 0, runner);
        registered = all && r[0].exit_code == Some(0);
        commands = r;
    }
    ScheduleReport {
        operation: ScheduleOp::Status,
        scheduler: env.scheduler.as_str().to_owned(),
        job: env.job(),
        interval_secs: None,
        dry_run: false,
        applied: false,
        registered,
        files: present
            .iter()
            .map(|(p, here)| FileReport {
                path: p.to_string_lossy().into_owned(),
                change: if *here {
                    FileChange::Present
                } else {
                    FileChange::Missing
                },
                contents: None,
                existed: *here,
            })
            .collect(),
        commands,
        notes: if any && !all {
            vec![
                "some of scull's unit files are missing; `scull schedule enable` rewrites them, \
                  `scull schedule disable` removes the rest"
                    .to_owned(),
            ]
        } else {
            Vec::new()
        },
    }
}

fn scheduler_name(s: &str) -> &'static str {
    match s {
        "launchd" => "launchd LaunchAgent",
        "systemd" => "systemd user timer",
        _ => "Task Scheduler task",
    }
}

fn escaped(text: &str) -> String {
    text.lines()
        .map(|l| escape_for_terminal(l) + "\n")
        .collect()
}

/// The plan, as shown before asking.
pub fn render(r: &ScheduleReport) -> String {
    let mut out = String::new();
    let what = scheduler_name(&r.scheduler);
    match r.operation {
        ScheduleOp::Enable => {
            let every = r.interval_secs.map(plan::interval_text).unwrap_or_default();
            out.push_str(&format!(
                "scull schedule enable: run `{}` every {every} at low priority, as you ({what})\n",
                r.job
            ));
            out.push_str("\nFiles to write:\n");
            for f in &r.files {
                out.push_str(&format!(
                    "  {}{}\n",
                    f.path,
                    if f.existed {
                        " (replaces the existing file)"
                    } else {
                        ""
                    }
                ));
                for l in f.contents.as_deref().unwrap_or_default().lines() {
                    out.push_str(&format!("    | {l}\n"));
                }
            }
        }
        ScheduleOp::Disable if !r.registered => {
            out.push_str(&format!(
                "scull schedule disable: no periodic check is registered ({what}); nothing to do\n"
            ));
            return out;
        }
        ScheduleOp::Disable => {
            out.push_str(&format!("scull schedule disable: remove the {what}\n"));
            out.push_str("\nFiles to remove:\n");
            for f in &r.files {
                out.push_str(&format!("  {}\n", f.path));
            }
        }
        ScheduleOp::Status => return render_outcome(r),
    }
    out.push_str("\nCommands to run:\n");
    for c in &r.commands {
        out.push_str(&format!(
            "  {}{}\n",
            c.command,
            if c.required {
                ""
            } else {
                "   (may fail harmlessly)"
            }
        ));
    }
    for n in &r.notes {
        out.push_str(&format!("\nnote: {n}\n"));
    }
    out
}

/// The result line(s) after applying, or the status.
pub fn render_outcome(r: &ScheduleReport) -> String {
    let what = scheduler_name(&r.scheduler);
    match r.operation {
        ScheduleOp::Enable => format!(
            "Registered: `{}` runs every {} ({what}). Undo with `scull schedule disable`.\n",
            r.job,
            r.interval_secs.map(plan::interval_text).unwrap_or_default()
        ),
        ScheduleOp::Disable => {
            format!("Removed: the periodic check is no longer registered ({what}).\n")
        }
        ScheduleOp::Status => {
            let mut out = if r.registered {
                format!("scull schedule: registered ({what}): `{}`\n", r.job)
            } else {
                format!(
                    "scull schedule: not registered ({what}); `scull schedule enable` sets it up\n"
                )
            };
            for f in &r.files {
                let state = match f.change {
                    FileChange::Present => "present",
                    _ => "missing",
                };
                out.push_str(&format!("  {}: {state}\n", f.path));
            }
            for c in &r.commands {
                out.push_str(&format!(
                    "  $ {} -> exit {}\n",
                    c.command,
                    c.exit_code.map_or("?".to_owned(), |c| c.to_string())
                ));
            }
            for n in &r.notes {
                out.push_str(&format!("note: {n}\n"));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_platform::scheduler::CommandOutput;
    use std::cell::RefCell;

    /// Records every invocation; answers with `code` (or an error for `fail_spawn`). Never runs
    /// anything.
    struct Fake {
        calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
        code: i32,
        fail_spawn: bool,
    }

    impl Fake {
        fn ok() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                code: 0,
                fail_spawn: false,
            }
        }
        fn args(&self) -> Vec<Vec<String>> {
            self.calls.borrow().iter().map(|(_, a)| a.clone()).collect()
        }
    }

    impl CommandRunner for Fake {
        fn run(&self, program: &Path, args: &[String]) -> io::Result<CommandOutput> {
            self.calls
                .borrow_mut()
                .push((program.to_path_buf(), args.to_vec()));
            if self.fail_spawn {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such tool"));
            }
            Ok(CommandOutput {
                code: Some(self.code),
                stdout: "fake output".into(),
                stderr: String::new(),
            })
        }
    }

    fn env(scheduler: Scheduler, dir: &Path) -> Env {
        Env {
            scheduler,
            unit_dir: dir.join("units"),
            exe: PathBuf::from("/opt/scull/bin/scull"),
            scull_home: None,
            elevation: Elevation::NotElevated,
        }
    }

    const ALL: [Scheduler; 3] = [
        Scheduler::Launchd,
        Scheduler::SystemdUser,
        Scheduler::TaskScheduler,
    ];

    #[test]
    fn enable_writes_into_the_injected_dir_and_runs_the_fake() {
        for s in ALL {
            let dir = tempfile::tempdir().expect("tempdir");
            let e = env(s, dir.path());
            let (report, install) = prepare_enable(&e, 6 * 3600);
            assert!(!report.applied);
            assert!(report.commands.iter().all(|c| !c.ran));
            assert!(!e.unit_dir.exists(), "prepare changes nothing");
            let fake = Fake::ok();
            let done = apply_enable(report, &install, &fake).expect("apply");
            assert!(done.applied && done.registered);
            for f in &install.files {
                assert!(f.path.starts_with(&e.unit_dir), "{:?}", f.path);
                assert_eq!(std::fs::read(&f.path).expect("written"), f.bytes);
            }
            assert_eq!(fake.calls.borrow().len(), install.commands.len());
            for (call, spec) in fake.calls.borrow().iter().zip(&install.commands) {
                assert_eq!(call.0, spec.program);
                assert_eq!(call.1, spec.args);
            }
            assert!(
                done.commands
                    .iter()
                    .all(|c| c.ran && c.exit_code == Some(0))
            );

            // status: files present, the fake's query says registered.
            let st_fake = Fake::ok();
            let st = status(&e, &st_fake);
            assert!(st.registered, "{s:?}");
            assert_eq!(st_fake.calls.borrow().len(), 1, "one read-only query");
            assert_eq!(
                st_fake.args()[0],
                plan::status_command(s).args,
                "the status query"
            );

            // disable: unregister, remove exactly scull's files.
            let (report, remove) = prepare_disable(&e);
            assert!(report.registered);
            let fake = Fake::ok();
            let done = apply_disable(report, &remove, &fake).expect("disable");
            assert!(done.applied && !done.registered);
            for f in &install.files {
                assert!(!f.path.exists());
            }
            assert!(e.unit_dir.is_dir(), "the folder itself is left alone");
            let st = status(&e, &Fake::ok());
            assert!(!st.registered);
        }
    }

    #[test]
    fn status_and_disable_never_run_tools_when_nothing_is_registered() {
        for s in ALL {
            let dir = tempfile::tempdir().expect("tempdir");
            let e = env(s, dir.path());
            let fake = Fake::ok();
            let st = status(&e, &fake);
            assert!(!st.registered);
            assert!(st.files.iter().all(|f| f.change == FileChange::Missing));
            let (report, _) = prepare_disable(&e);
            assert!(!report.registered);
            assert!(report.commands.is_empty() && report.files.is_empty());
            assert!(fake.calls.borrow().is_empty(), "{s:?}");
            assert!(render(&report).contains("nothing to do"));
        }
    }

    #[test]
    fn a_failed_required_step_is_an_error_and_a_failed_unload_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let e = env(Scheduler::Launchd, dir.path());
        let (report, install) = prepare_enable(&e, 3600);
        let failing = Fake {
            calls: RefCell::new(Vec::new()),
            code: 5,
            fail_spawn: false,
        };
        // `unload` (best effort) fails and is tolerated; `load -w` (required) fails: error.
        let err = apply_enable(report, &install, &failing).expect_err("load fails");
        assert!(err.message.contains("load -w"), "{}", err.message);
        assert!(err.message.contains("scull schedule disable"));
        assert_eq!(failing.calls.borrow().len(), 2);

        let (report, install) = prepare_enable(&env(Scheduler::SystemdUser, dir.path()), 3600);
        let missing = Fake {
            calls: RefCell::new(Vec::new()),
            code: 0,
            fail_spawn: true,
        };
        let err = apply_enable(report, &install, &missing).expect_err("no tool");
        assert!(err.message.contains("cannot run"), "{}", err.message);
    }

    #[test]
    fn links_are_never_written_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let e = env(Scheduler::SystemdUser, dir.path());
        let (report, install) = prepare_enable(&e, 3600);
        // A directory where a unit file should go: refused, nothing run.
        std::fs::create_dir_all(&install.files[0].path).expect("dir in the way");
        let fake = Fake::ok();
        let err = apply_enable(report, &install, &fake).expect_err("refused");
        assert_eq!(err.kind, ErrorKind::SafetyBlocked);
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn consent_and_elevation_rules() {
        let yes = ConsentArgs {
            yes: true,
            dry_run: false,
        };
        let dry = ConsentArgs {
            yes: false,
            dry_run: true,
        };
        let none = ConsentArgs::default();
        let ok = Elevation::NotElevated;
        assert_eq!(
            gate(dry, true, false, Elevation::Elevated, 3).expect("dry"),
            Gate::Stop
        );
        assert_eq!(
            gate(dry, false, false, ok, 3).expect("dry json"),
            Gate::Stop
        );
        assert_eq!(gate(yes, true, false, ok, 3).expect("yes"), Gate::Apply);
        assert_eq!(
            gate(yes, false, false, ok, 3).expect("yes json"),
            Gate::Apply
        );
        assert_eq!(gate(none, true, true, ok, 3).expect("ask"), Gate::Ask);
        let e = gate(none, true, false, ok, 3).expect_err("no terminal");
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(e.message.contains("--yes"));
        assert_eq!(
            gate(none, false, true, ok, 3).expect_err("json").kind,
            ErrorKind::Usage
        );
        for elevated in [Elevation::Elevated, Elevation::Unknown] {
            let e = gate(yes, true, true, elevated, 3).expect_err("root");
            assert_eq!(e.kind, ErrorKind::SafetyBlocked);
            assert!(e.message.contains("elevated"));
        }
    }

    #[test]
    fn render_shows_every_file_line_and_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let e = env(Scheduler::SystemdUser, dir.path());
        let (report, install) = prepare_enable(&e, 6 * 3600);
        let text = render(&report);
        assert!(text.contains("every 6 hours at low priority, as you (systemd user timer)"));
        for f in &install.files {
            assert!(text.contains(&f.path.to_string_lossy().into_owned()));
            for l in f.text.lines() {
                assert!(text.contains(&format!("    | {l}")), "{l}");
            }
        }
        for c in &install.commands {
            assert!(text.contains(&c.display()));
        }
        assert!(parse_interval("10m").is_err());
        assert!(parse_interval("8d").is_err());
        assert_eq!(parse_interval("6h").expect("6h"), 21_600);
    }
}
