//! Command dispatch.
//!
//! Commands that are not implemented yet fail with exit code 1 (`ExitCode::General`) and a
//! `unsupported_platform` error whose message names the command and its tracking issue, e.g.
//! `` `scull scan` is not implemented yet (tracked in #56) ``.

use crate::cancel;
use crate::cli::{
    AgentAction, CleanAction, Cli, Command, DataAction, InsightsAction, McpAction, ProjectsAction,
    RulesAction, ScheduleAction, WorktreesAction,
};
use crate::output::Ui;
use clap::CommandFactory;
use scull_core::{ErrorKind, ScullError};
use std::io::{self, Write};

/// Where a command is implemented: its full name and tracking issue (if one exists yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Planned {
    /// Full command, e.g. `clean caches`.
    pub name: &'static str,
    /// GitHub issue that implements it.
    pub issue: Option<u32>,
}

const fn planned(name: &'static str, issue: u32) -> Option<Planned> {
    Some(Planned {
        name,
        issue: Some(issue),
    })
}

/// The tracking issue for a command that is not implemented yet, or `None` if the command is
/// implemented in this build.
pub fn planned_for(cmd: &Command) -> Option<Planned> {
    match cmd {
        Command::Scan(_) | Command::Projects { .. } | Command::Caches => None,
        Command::Worktrees {
            action: None | Some(WorktreesAction::Inspect { .. }),
            ..
        } => None,
        Command::Worktrees {
            action: Some(WorktreesAction::Register { .. }),
            ..
        } => planned("worktrees register", 111),
        Command::Clean(args) => match args.action {
            None => None,
            Some(CleanAction::Caches { .. }) => planned("clean caches", 80),
        },
        Command::LargeFiles { .. } => None,
        Command::History => planned("history", 81),
        Command::Restore { .. } => planned("restore", 81),
        Command::Approve { .. } => planned("approve", 108),
        Command::Rules { .. } => None,
        Command::Init(_) => None,
        Command::Status { .. } => None,
        Command::Check => planned("check", 114),
        Command::Insights { action } => match action {
            None => planned("insights", 116),
            Some(InsightsAction::Export) => planned("insights export", 116),
            Some(InsightsAction::Purge) => planned("insights purge", 116),
        },
        Command::Agent {
            action: AgentAction::Install { .. },
        } => planned("agent install", 110),
        Command::Schedule { action } => match action {
            ScheduleAction::Enable => planned("schedule enable", 114),
            ScheduleAction::Disable => planned("schedule disable", 114),
        },
        Command::Data { .. } => None,
        Command::Doctor => None,
        Command::Mcp {
            action: McpAction::Serve { .. },
        } => None,
        Command::Completions { .. } => None,
    }
}

/// The error for a command that is not implemented in this build.
pub fn not_implemented(p: Planned) -> ScullError {
    let tracking = match p.issue {
        Some(n) => format!("tracked in #{n}"),
        None => "not tracked yet".to_owned(),
    };
    ScullError::new(
        ErrorKind::UnsupportedPlatform,
        format!("`scull {}` is not implemented yet ({tracking})", p.name),
    )
}

/// Run the parsed command.
pub fn run(cli: Cli, ui: &Ui) -> Result<(), ScullError> {
    if let Command::Completions { shell } = cli.command {
        return completions(shell.into());
    }
    // Every command that does work honors Ctrl-C (CLI-02). `--help`, `--version` and
    // `completions` never get here, so they stay fast.
    if let Err(e) = cancel::install() {
        ui.debug(&format!("could not install Ctrl-C handler: {e}"));
    }
    ui.debug(&format!("command: {:?}", cli.command));
    if let Some(p) = planned_for(&cli.command) {
        return Err(not_implemented(p));
    }
    cancel::check()?;
    match &cli.command {
        Command::Doctor => crate::cmd::doctor::run(ui)?,
        Command::Scan(args) => crate::cmd::scan::run(args, ui)?,
        Command::Projects {
            roots,
            action: None,
        } => crate::cmd::projects::run(roots, ui)?,
        Command::Projects {
            roots,
            action: Some(ProjectsAction::Inspect { path }),
        } => crate::cmd::projects::inspect(path, roots, ui)?,
        Command::Worktrees {
            roots,
            action: None,
        } => crate::cmd::worktrees::run(roots, ui)?,
        Command::Worktrees {
            action: Some(WorktreesAction::Inspect { path }),
            ..
        } => crate::cmd::worktrees::inspect(path, ui)?,
        Command::Caches => crate::cmd::caches::run(ui)?,
        Command::Clean(args) if args.action.is_none() => crate::cmd::clean::run(args, ui)?,
        Command::LargeFiles {
            path,
            min_size,
            top,
        } => crate::large_files::run(path, *min_size, *top, ui)?,
        Command::Status { short } => crate::cmd::status::run(ui, *short)?,
        Command::Init(args) => crate::cmd::init::run(ui, args)?,
        Command::Rules {
            action: RulesAction::List,
        } => crate::cmd::rules::run_list(ui)?,
        Command::Rules {
            action: RulesAction::Validate { paths },
        } => crate::cmd::rules::run_validate(ui, paths)?,
        Command::Mcp {
            action: McpAction::Serve { allow_destructive },
        } => crate::cmd::mcp::serve(*allow_destructive, ui)?,
        Command::Data {
            action: DataAction::Show,
        } => crate::cmd::data::run_show(ui)?,
        Command::Data {
            action: DataAction::Export { format, out },
        } => crate::cmd::data::run_export(ui, *format, out.as_deref())?,
        Command::Data {
            action:
                DataAction::Purge {
                    older_than,
                    confirm,
                    include_quarantine,
                    ..
                },
        } => {
            let scope = match older_than {
                Some(secs) => crate::cmd::data::PurgeScope::OlderThan(*secs),
                None => crate::cmd::data::PurgeScope::All,
            };
            let opts = crate::cmd::data::PurgeOptions {
                scope,
                include_quarantine: *include_quarantine,
            };
            crate::cmd::data::run_purge(ui, &opts, *confirm)?;
        }
        _ => {}
    }
    cancel::check()
}

fn completions(shell: clap_complete::Shell) -> Result<(), ScullError> {
    let mut cmd = Cli::command();
    let mut buf = Vec::new();
    clap_complete::generate(shell, &mut cmd, "scull", &mut buf);
    let mut out = io::stdout().lock();
    out.write_all(&buf)
        .and_then(|()| out.flush())
        .or_else(ignore_broken_pipe)
        .map_err(|e| ScullError::from_io(&e))
}

/// A closed stdout (e.g. `scull completions zsh | head`) is not an error.
pub fn ignore_broken_pipe(e: io::Error) -> io::Result<()> {
    if e.kind() == io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use scull_core::ExitCode;

    fn planned_of(args: &[&str]) -> Option<Planned> {
        let cli = Cli::try_parse_from(std::iter::once("scull").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("{args:?}: {e}"));
        planned_for(&cli.command)
    }

    #[test]
    fn every_planned_command_names_its_issue() {
        let table: &[(&[&str], &str, Option<u32>)] = &[
            (
                &["worktrees", "register", ".", "--ttl", "7d"],
                "worktrees register",
                Some(111),
            ),
            (&["clean", "caches"], "clean caches", Some(80)),
            (&["history"], "history", Some(81)),
            (&["restore", "op-1"], "restore", Some(81)),
            (&["approve", "plan-1"], "approve", Some(108)),
            (&["check"], "check", Some(114)),
            (&["insights"], "insights", Some(116)),
            (&["insights", "export"], "insights export", Some(116)),
            (&["insights", "purge"], "insights purge", Some(116)),
            (&["agent", "install", "codex"], "agent install", Some(110)),
            (&["schedule", "enable"], "schedule enable", Some(114)),
            (&["schedule", "disable"], "schedule disable", Some(114)),
        ];
        for (args, name, issue) in table {
            assert_eq!(
                planned_of(args),
                Some(Planned {
                    name,
                    issue: *issue
                }),
                "{args:?}"
            );
        }
        assert_eq!(planned_of(&["completions", "bash"]), None);
        assert_eq!(planned_of(&["doctor"]), None);
        for implemented in [
            &["scan", "."][..],
            &["projects"],
            &["projects", "inspect", "."],
            &["worktrees"],
            &["worktrees", "inspect", "."],
            &["caches"],
            &["data", "show"],
            &["data", "export"],
            &["data", "purge", "--all"],
            &["data", "purge", "--older-than", "30d"],
            &["clean"],
            &["clean", "--dry-run", "--output", "p.json"],
            &["clean", "--plan", "p.json", "--confirm"],
        ] {
            assert_eq!(planned_of(implemented), None, "{implemented:?}");
        }
        assert_eq!(planned_of(&["large-files", "."]), None);
        assert_eq!(planned_of(&["status", "--short"]), None);
        assert_eq!(planned_of(&["rules", "list"]), None);
        assert_eq!(planned_of(&["init", "--yes"]), None);
        assert_eq!(planned_of(&["rules", "validate", "x.toml"]), None);
        assert_eq!(planned_of(&["mcp", "serve"]), None);
    }

    #[test]
    fn not_implemented_error_is_clear_and_exits_1() {
        let e = not_implemented(Planned {
            name: "history",
            issue: Some(81),
        });
        assert_eq!(
            e.message,
            "`scull history` is not implemented yet (tracked in #81)"
        );
        assert_eq!(e.exit_code(), ExitCode::General);
        let e = not_implemented(Planned {
            name: "rules list",
            issue: None,
        });
        assert!(e.message.ends_with("(not tracked yet)"));
    }
}
