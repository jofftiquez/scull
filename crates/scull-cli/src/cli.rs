//! The command-line surface (PRD §12), declared with clap's derive API.

use crate::output::{ColorWhen, Format};
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// `scull --version`: version plus target triple.
pub const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("SCULL_TARGET"), ")");

const AFTER_HELP: &str = "\
Output:
  Data goes to stdout; progress and errors go to stderr. With --json or --ndjson, stdout carries
  only JSON with a versioned envelope {\"schema\": \"scull/<name>@<major>\", \"version\", ...} and
  errors are written to stderr as one JSON line {\"schema\": \"scull/error@1\", \"version\",
  \"error\": {\"kind\", \"message\", ...}}. Terminal output escapes control and bidirectional
  characters in names and flags suspicious names.

Exit codes:
  0    success
  1    general error (including commands not implemented yet in this build)
  2    usage or configuration error
  3    completed with partial failures (see report)
  4    plan rejected (expired, tampered, foreign, or invalidated)
  5    blocked by safety policy
  6    execution lock held by another operation
  130  cancelled by user (Ctrl-C)";

/// scull: a safe developer storage analyzer and cleanup tool.
///
/// Finds regenerable build outputs, dependency folders, package caches and stale Git
/// worktrees, explains what each one is and how risky it is to remove, and never deletes
/// anything without a reviewed plan and explicit confirmation.
#[derive(Debug, Parser)]
#[command(
    name = "scull",
    version = LONG_VERSION,
    propagate_version = true,
    subcommand_required = true,
    arg_required_else_help = true,
    after_help = AFTER_HELP,
    max_term_width = 100
)]
pub struct Cli {
    /// Flags accepted by every command.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// The command to run.
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Parse like [`Parser::try_parse_from`], plus the global-flag conflict checks.
    pub fn try_parse_checked<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let cli = Self::try_parse_from(args)?;
        cli.check_global_conflicts()?;
        Ok(cli)
    }

    /// clap checks `conflicts_with` per command level, so `scull --json caches --ndjson`
    /// slips through; check the merged global flags again.
    pub fn check_global_conflicts(&self) -> Result<(), clap::Error> {
        use clap::CommandFactory;
        let g = &self.global;
        let conflict = if g.json && g.ndjson {
            Some(("--json", "--ndjson"))
        } else if g.quiet && g.verbose > 0 {
            Some(("--quiet", "--verbose"))
        } else {
            None
        };
        match conflict {
            None => Ok(()),
            Some((a, b)) => Err(Self::command().error(
                clap::error::ErrorKind::ArgumentConflict,
                format!("the argument '{a}' cannot be used with '{b}'"),
            )),
        }
    }
}

/// Flags accepted by every command.
#[derive(Debug, Args)]
pub struct GlobalArgs {
    /// Write one JSON document to stdout.
    #[arg(long, global = true, conflicts_with = "ndjson")]
    pub json: bool,

    /// Write newline-delimited JSON records to stdout (one object per line).
    #[arg(long, global = true)]
    pub ndjson: bool,

    /// Suppress progress and informational messages; errors are still shown.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// When to use color. `auto` honors NO_COLOR and TERM=dumb.
    #[arg(long, global = true, value_enum, value_name = "WHEN", default_value_t = ColorWhen::Auto)]
    pub color: ColorWhen,

    /// Do not show progress on stderr.
    #[arg(long, global = true)]
    pub no_progress: bool,

    /// More diagnostic output on stderr (repeat for more).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,
}

impl GlobalArgs {
    /// The stdout format selected by `--json` / `--ndjson`.
    pub fn format(&self) -> Format {
        match (self.json, self.ndjson) {
            (true, _) => Format::Json,
            (_, true) => Format::Ndjson,
            _ => Format::Human,
        }
    }
}

/// Top-level commands (PRD §12).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scan directories for reclaimable developer storage (read-only).
    Scan(ScanArgs),

    /// List detected projects, or inspect one.
    Projects {
        /// Scan roots (default: `scan.roots` from the config).
        #[command(flatten)]
        roots: RootArgs,
        /// Optional subcommand.
        #[command(subcommand)]
        action: Option<ProjectsAction>,
    },

    /// List Git worktrees and their state, inspect one, or register a TTL.
    Worktrees {
        /// Scan roots (default: `scan.roots` from the config).
        #[command(flatten)]
        roots: RootArgs,
        /// Optional subcommand.
        #[command(subcommand)]
        action: Option<WorktreesAction>,
    },

    /// List package-manager caches and their sizes (read-only).
    Caches,

    /// Plan a cleanup (dry run by default), or execute a reviewed plan.
    Clean(CleanArgs),

    /// List large files under a path (inspect only; never cleaned).
    #[command(name = "large-files")]
    LargeFiles {
        /// Directory to inspect.
        path: PathBuf,

        /// Smallest allocated size to list (binary units: 500M, 1.5G, 2GiB).
        #[arg(long, value_name = "SIZE", default_value = crate::large_files::DEFAULT_MIN_SIZE,
              value_parser = crate::large_files::parse_size)]
        min_size: u64,

        /// List at most this many files (the largest).
        #[arg(long, value_name = "N", default_value_t = crate::large_files::DEFAULT_TOP_ARG,
              value_parser = crate::large_files::parse_top)]
        top: usize,
    },

    /// Show past cleanup operations.
    History,

    /// Restore items from a past operation (Trash or quarantine).
    Restore {
        /// Operation ID from `scull history`.
        operation_id: String,
    },

    /// Approve a plan prepared by an MCP client (interactive terminal only).
    Approve {
        /// Plan ID to approve.
        plan_id: String,
    },

    /// List or validate detection rules.
    Rules {
        /// Subcommand.
        #[command(subcommand)]
        action: RulesAction,
    },

    /// First-run onboarding: detect toolchains, confirm roots and profile.
    Init(InitArgs),

    /// Cached one-line status, e.g. for shell prompts.
    Status {
        /// Print a short single-line summary.
        #[arg(long)]
        short: bool,
    },

    /// Light scan plus forecast and alerts (run by the OS scheduler).
    Check,

    /// Local trends and insights from scull's own history (read-only).
    Insights {
        /// Start of the report window: a duration back from now (7d, 30d, 12w, 48h) or a UTC
        /// date (2026-10-01). Default: 30 days.
        #[arg(long, value_name = "WHEN", value_parser = crate::cmd::insights::parse_when)]
        since: Option<crate::cmd::insights::When>,
        /// Optional subcommand.
        #[command(subcommand)]
        action: Option<InsightsAction>,
    },

    /// Set up an AI client (MCP config and guidance), with consent.
    Agent {
        /// Subcommand.
        #[command(subcommand)]
        action: AgentAction,
    },

    /// Register or remove periodic checks with launchd, systemd or Task Scheduler.
    Schedule {
        /// Subcommand.
        #[command(subcommand)]
        action: ScheduleAction,
    },

    /// Inspect, export or delete everything scull has recorded locally.
    Data {
        /// Subcommand.
        #[command(subcommand)]
        action: DataAction,
    },

    /// Report platform capabilities, permissions and available signals.
    Doctor,

    /// Model Context Protocol server for AI agents.
    Mcp {
        /// Subcommand.
        #[command(subcommand)]
        action: McpAction,
    },

    /// Generate a shell completion script on stdout.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: CompletionShell,
    },
}

/// Arguments for `scull scan`.
#[derive(Debug, Args)]
pub struct ScanArgs {
    /// Directories to scan (default: `scan.roots` from the config file).
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,

    /// Allow scanning a filesystem root, your home folder, or a folder that contains it
    /// (refused by default, CFG-04).
    #[arg(long)]
    pub allow_broad_roots: bool,

    /// Do not record this scan in the local history database.
    #[arg(long)]
    pub no_history: bool,

    /// Allow scan roots on network or virtual volumes such as NFS, SMB, FUSE, tmpfs or
    /// overlay (skipped by default, D-22; backup volumes are never scanned).
    #[arg(long)]
    pub allow_network_roots: bool,
}

/// Scan roots for `scull projects` and `scull worktrees`.
#[derive(Debug, Clone, Default, Args)]
pub struct RootArgs {
    /// Directory to scan; repeat for several (default: `scan.roots` from the config file).
    #[arg(long = "root", value_name = "PATH", global = true)]
    pub roots: Vec<PathBuf>,

    /// Allow scanning a filesystem root, your home folder, or a folder that contains it
    /// (refused by default, CFG-04).
    #[arg(long, global = true)]
    pub allow_broad_roots: bool,

    /// Allow scan roots on network or virtual volumes such as NFS, SMB, FUSE, tmpfs or
    /// overlay (skipped by default, D-22; backup volumes are never scanned).
    #[arg(long, global = true)]
    pub allow_network_roots: bool,
}

/// Arguments for `scull init` (ONB-01…04). Interactive on a terminal; with `--yes`, or when
/// stdin is not a terminal, every answer comes from these flags or the detected defaults.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Accept the detected defaults for every question not given by a flag (no prompts).
    #[arg(short, long)]
    pub yes: bool,

    /// Scan roots (repeat or comma-separate); replaces the detected project folders.
    #[arg(long, value_name = "PATH", value_delimiter = ',', num_args = 1..)]
    pub roots: Vec<PathBuf>,

    /// Explicitly confirm roots that are a filesystem root, the home directory or one of its
    /// parents (CFG-04). Without it such roots are refused.
    #[arg(long)]
    pub allow_broad_roots: bool,

    /// What you build (repeat or comma-separate): web, rust, python, ml, mobile, game,
    /// native, infra, other.
    #[arg(long, value_name = "KIND", value_delimiter = ',', num_args = 1..)]
    pub builds: Vec<String>,

    /// AI coding agents: none, sometimes or daily.
    #[arg(long, value_name = "USE")]
    pub agents: Option<String>,

    /// What matters most: free-space, tidy or understand.
    #[arg(long, value_name = "GOAL")]
    pub priority: Option<String>,

    /// How cautious scull should be: cautious, balanced or expert (never changes tiers or
    /// safety checks, ONB-07).
    #[arg(long, value_name = "LEVEL")]
    pub caution: Option<String>,

    /// Replace an existing profile and the config's scan roots (other config settings are
    /// kept).
    #[arg(long)]
    pub force: bool,
}

/// `scull projects` subcommands.
#[derive(Debug, Subcommand)]
pub enum ProjectsAction {
    /// Show details for one project.
    Inspect {
        /// Project directory.
        path: PathBuf,
    },
}

/// `scull worktrees` subcommands.
#[derive(Debug, Subcommand)]
pub enum WorktreesAction {
    /// Show details for one worktree.
    Inspect {
        /// Worktree directory.
        path: PathBuf,
    },
    /// Register an agent or script worktree with a time-to-live.
    Register {
        /// Worktree directory.
        path: PathBuf,
        /// Time to live, e.g. `7d`, `12h`.
        #[arg(long, value_name = "DURATION")]
        ttl: String,
    },
}

/// Arguments for `scull clean`. Without a subcommand this plans a cleanup (dry run by
/// default); executing requires `--plan` plus `--confirm` (CLI-03).
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct CleanArgs {
    /// Optional subcommand.
    #[command(subcommand)]
    pub action: Option<CleanAction>,

    /// Only plan; never delete (the default).
    #[arg(long, conflicts_with = "confirm")]
    pub dry_run: bool,

    /// Write the plan to this file.
    #[arg(long, value_name = "FILE", conflicts_with = "plan")]
    pub output: Option<PathBuf>,

    /// Execute this reviewed plan file (requires --confirm).
    #[arg(long, value_name = "FILE")]
    pub plan: Option<PathBuf>,

    /// Confirm execution of --plan.
    #[arg(long, requires = "plan")]
    pub confirm: bool,

    /// Allow permanent deletion of costly or uncertain items (elevated confirmation).
    #[arg(long, requires = "confirm")]
    pub allow_permanent: bool,

    /// Allow running as root/administrator (refused by default).
    #[arg(long)]
    pub allow_root: bool,
}

/// `scull clean` subcommands.
#[derive(Debug, Subcommand)]
pub enum CleanAction {
    /// Quick Clean: prune package-manager caches with their native commands.
    Caches {
        /// Run the commands (otherwise only show them).
        #[arg(long)]
        confirm: bool,
        /// Wipe caches entirely instead of pruning.
        #[arg(long, requires = "confirm")]
        wipe: bool,
    },
}

/// `scull rules` subcommands.
#[derive(Debug, Subcommand)]
pub enum RulesAction {
    /// List the bundled detection rules.
    List,
    /// Validate rule files.
    Validate {
        /// Rule files to validate (default: the bundled rules).
        paths: Vec<PathBuf>,
    },
}

/// `scull insights` subcommands.
#[derive(Debug, Subcommand)]
pub enum InsightsAction {
    /// Write the insights report to stdout as JSON or CSV (redirect it to a file to keep it).
    Export {
        /// Output format.
        #[arg(long, value_enum, default_value_t = ExportFormat::Json)]
        format: ExportFormat,
        /// Start of the report window (as for `scull insights --since`).
        #[arg(long, value_name = "WHEN", value_parser = crate::cmd::insights::parse_when)]
        since: Option<crate::cmd::insights::When>,
    },
    /// Forget recorded trends (history rows only; scans, plans, audit and files are kept).
    /// Refused in the inspect-only alpha (#266).
    Purge {
        /// Only delete history older than this: a duration (90d, 12w) or a UTC date.
        #[arg(long, value_name = "WHEN", value_parser = crate::cmd::insights::parse_when)]
        older_than: Option<crate::cmd::insights::When>,
        /// Delete without asking (required without a terminal or with --json/--ndjson).
        #[arg(long)]
        confirm: bool,
    },
}

/// Formats for `scull insights export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    /// The `scull/insights@1` JSON document.
    Json,
    /// One tidy CSV table: section, period, subject, metric, value.
    Csv,
}

/// `scull agent` subcommands.
#[derive(Debug, Subcommand)]
pub enum AgentAction {
    /// Write MCP config and guidance for an AI client.
    Install {
        /// Client name (e.g. claude-code, codex, cursor, vscode, gemini-cli).
        client: String,
    },
}

/// `scull schedule` subcommands.
#[derive(Debug, Subcommand)]
pub enum ScheduleAction {
    /// Register periodic `scull check` runs.
    Enable,
    /// Remove periodic checks.
    Disable,
}

/// `scull data` subcommands.
#[derive(Debug, Subcommand)]
pub enum DataAction {
    /// Show what scull has recorded and where.
    Show,
    /// Export everything scull has recorded.
    Export,
    /// Delete everything scull has recorded.
    Purge,
}

/// `scull mcp` subcommands.
#[derive(Debug, Subcommand)]
pub enum McpAction {
    /// Serve MCP over stdio (read-only unless this flag and the config both allow more).
    Serve {
        /// Allow destructive tools (also requires the config setting).
        #[arg(long)]
        allow_destructive: bool,
    },
}

/// Shells supported by `scull completions` (CLI-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    /// Bash.
    Bash,
    /// Zsh.
    Zsh,
    /// Fish.
    Fish,
    /// PowerShell.
    Powershell,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(s: CompletionShell) -> Self {
        match s {
            CompletionShell::Bash => clap_complete::Shell::Bash,
            CompletionShell::Zsh => clap_complete::Shell::Zsh,
            CompletionShell::Fish => clap_complete::Shell::Fish,
            CompletionShell::Powershell => clap_complete::Shell::PowerShell,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_parse_after_subcommand() {
        let cli = Cli::try_parse_from(["scull", "caches", "--json", "-vv", "--color", "never"])
            .expect("parses");
        assert_eq!(cli.global.format(), Format::Json);
        assert_eq!(cli.global.verbose, 2);
        assert_eq!(cli.global.color, ColorWhen::Never);
    }

    #[test]
    fn json_and_ndjson_conflict() {
        let err = Cli::try_parse_checked(["scull", "--json", "caches", "--ndjson"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn clean_execution_requires_plan_and_confirm() {
        // CLI-03: --confirm without --plan is a usage error.
        assert!(Cli::try_parse_from(["scull", "clean", "--confirm"]).is_err());
        assert!(Cli::try_parse_from(["scull", "clean", "--plan", "p.json"]).is_ok());
        assert!(
            Cli::try_parse_from(["scull", "clean", "--plan", "p.json", "--allow-permanent"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["scull", "clean", "--dry-run", "--confirm"]).is_err());
        let cli = Cli::try_parse_from(["scull", "clean", "caches", "--confirm", "--wipe"])
            .expect("parses");
        assert!(matches!(
            cli.command,
            Command::Clean(CleanArgs {
                action: Some(CleanAction::Caches {
                    confirm: true,
                    wipe: true
                }),
                ..
            })
        ));
        assert!(Cli::try_parse_from(["scull", "clean", "caches", "--wipe"]).is_err());
    }
}
