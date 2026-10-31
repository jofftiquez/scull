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

    /// Show past cleanup operations, or one operation in detail (read-only).
    History(HistoryArgs),

    /// Restore quarantined items of a past operation to where they were (never overwriting).
    Restore(RestoreArgs),

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

    /// Local trends and insights.
    Insights {
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
/// default); executing requires `--plan` plus `--confirm` (CLI-03), or answering the prompt
/// on a terminal.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
#[command(group(clap::ArgGroup::new("confirm_target").args(["plan", "purge_quarantine"])))]
pub struct CleanArgs {
    /// Optional subcommand.
    #[command(subcommand)]
    pub action: Option<CleanAction>,

    /// Scan root; repeat for several (default: `scan.roots` from the config file; with
    /// --plan, the roots the plan was made from).
    #[arg(long = "root", value_name = "PATH")]
    pub roots: Vec<PathBuf>,

    /// Allow scanning a filesystem root, your home folder, or a folder that contains it
    /// (refused by default, CFG-04).
    #[arg(long)]
    pub allow_broad_roots: bool,

    /// Allow scan roots on network or virtual volumes such as NFS, SMB, FUSE, tmpfs or
    /// overlay (skipped by default, D-22).
    #[arg(long)]
    pub allow_network_roots: bool,

    /// Also plan S1 (costly to recreate) items; they are quarantined, not deleted.
    #[arg(long, conflicts_with = "plan")]
    pub include_s1: bool,

    /// Plan exactly these items (item ID or path; repeat for several). The only way to plan
    /// an S2 (uncertain) item.
    #[arg(long = "item", value_name = "ID|PATH", conflicts_with_all = ["plan", "include_s1"])]
    pub items: Vec<String>,

    /// Only plan or review; never execute or ask (the default without a terminal).
    #[arg(long, conflicts_with = "confirm")]
    pub dry_run: bool,

    /// Write the plan to this file (owner-only).
    #[arg(long, value_name = "FILE", conflicts_with = "plan")]
    pub output: Option<PathBuf>,

    /// Review or execute this plan file (made by `scull clean --output`).
    #[arg(long, value_name = "FILE")]
    pub plan: Option<PathBuf>,

    /// Execute --plan (or --purge-quarantine) without asking.
    #[arg(long, requires = "confirm_target")]
    pub confirm: bool,

    /// Confirm items that can't be undone at S1/S2 without typing their count (elevated
    /// confirmation, SAFE-07; required for them without a terminal).
    #[arg(long, requires = "confirm")]
    pub allow_permanent: bool,

    /// Allow running as root/administrator (refused by default, SAFE-14).
    #[arg(long)]
    pub allow_root: bool,

    /// Permanently delete quarantined items whose retention has expired (or, with
    /// --older-than, that were quarantined more than AGE ago). Not undoable: needs the
    /// elevated confirmation (typed count, or --allow-permanent with --confirm).
    #[arg(long, conflicts_with_all = ["plan", "output", "items", "include_s1", "roots"])]
    pub purge_quarantine: bool,

    /// With --purge-quarantine: also purge items quarantined more than AGE ago (90m, 12h,
    /// 30d, 2w), even before their retention ends.
    #[arg(long, value_name = "AGE", requires = "purge_quarantine",
          value_parser = crate::cmd::data::parse_age)]
    pub older_than: Option<u64>,

    /// With --purge-quarantine: purge every held item, also those whose retention hasn't
    /// ended (empties the quarantine).
    #[arg(long, requires = "purge_quarantine", conflicts_with = "older_than")]
    pub include_unexpired: bool,
}

/// Arguments for `scull history`.
#[derive(Debug, Clone, Args)]
pub struct HistoryArgs {
    /// Show this operation in detail (items, audit chain, quarantine entries).
    pub operation_id: Option<String>,
    /// Only operations started within AGE (90m, 12h, 30d, 2w).
    #[arg(long, value_name = "AGE", value_parser = crate::cmd::data::parse_age, conflicts_with = "operation_id")]
    pub since: Option<u64>,
}

/// Arguments for `scull restore`.
#[derive(Debug, Clone, Args)]
pub struct RestoreArgs {
    /// Operation ID from `scull history`.
    pub operation_id: String,
    /// Restore only this item (item ID, original path, or quarantine entry number); repeat
    /// for several.
    #[arg(long = "item", value_name = "ITEM")]
    pub items: Vec<String>,
    /// Restore without asking (required without a terminal).
    #[arg(long)]
    pub confirm: bool,
    /// Only show what would be restored.
    #[arg(long, conflicts_with = "confirm")]
    pub dry_run: bool,
    /// Allow running as root/administrator (refused by default, SAFE-14).
    #[arg(long)]
    pub allow_root: bool,
}

/// `scull clean` subcommands.
#[derive(Debug, Subcommand)]
pub enum CleanAction {
    /// Quick Clean: prune package-manager caches with their native commands.
    Caches(CleanCachesArgs),
}

/// Arguments for `scull clean caches` (QC-04). Without `--confirm` (and without a terminal
/// to ask on) it only shows what would run.
#[derive(Debug, Clone, Args)]
pub struct CleanCachesArgs {
    /// Run the commands without asking (otherwise only show them, or ask on a terminal).
    #[arg(long)]
    pub confirm: bool,
    /// Wipe caches entirely instead of pruning (where the tool has a prune; pnpm and conda
    /// are never wiped).
    #[arg(long)]
    pub wipe: bool,
    /// Leave this tool's cache alone; repeat for several (npm, pnpm, bun, yarn,
    /// yarn-classic, yarn-berry, pip, uv, conda).
    #[arg(long, value_name = "TOOL", value_parser = crate::cmd::clean_caches::parse_tool)]
    pub untick: Vec<crate::cmd::clean_caches::Untick>,
    /// Also remove this uninstalled tool's orphaned cache folder, when the preview lists it as
    /// removable (D-36): scull deletes it itself, which is not undoable. Repeat for several.
    /// This is the per-tool confirmation; on a terminal, each orphaned cache not named here
    /// is asked about separately. conda's `pkgs/` is never removed this way (environments
    /// hard-link into it).
    #[arg(long, value_name = "TOOL", value_parser = crate::cmd::clean_caches::parse_tool)]
    pub remove_orphaned: Vec<crate::cmd::clean_caches::Untick>,
    /// Only show what would run; never ask or run.
    #[arg(long, conflicts_with = "confirm")]
    pub dry_run: bool,
    /// Allow running as root/administrator (refused by default, SAFE-14).
    #[arg(long)]
    pub allow_root: bool,
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
    /// Export recorded trends.
    Export,
    /// Delete recorded trends.
    Purge,
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
    /// Show what scull has recorded and where (read-only).
    Show,
    /// Export everything scull has recorded (JSON or CSV; Parquet is not built yet).
    Export {
        /// File format.
        #[arg(long, value_enum, default_value_t = crate::cmd::data::ExportFormat::Json)]
        format: crate::cmd::data::ExportFormat,
        /// Folder to write to, created owner-only if missing (default:
        /// `./scull-export-<unix time>`). Existing files are never overwritten.
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
    },
    /// Delete scull's own files: the database, its backups and damaged copies, audit mirrors
    /// and the MCP salt. Never anything else; the configuration and quarantine are kept.
    Purge {
        /// Delete every file scull has recorded (FX-DATA-04).
        #[arg(
            long,
            conflicts_with = "older_than",
            required_unless_present = "older_than"
        )]
        all: bool,
        /// Delete only migration backups and damaged copies older than AGE (90m, 12h, 30d,
        /// 2w).
        #[arg(long, value_name = "AGE", value_parser = crate::cmd::data::parse_age)]
        older_than: Option<u64>,
        /// Delete without asking (required without a terminal or with --json/--ndjson).
        #[arg(long)]
        confirm: bool,
        /// Also purge quarantine contents (refused: use `scull clean --purge-quarantine`,
        /// which goes through the Executor's purge, #71).
        #[arg(long)]
        include_quarantine: bool,
    },
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
        let cli = Cli::try_parse_from([
            "scull",
            "clean",
            "caches",
            "--confirm",
            "--wipe",
            "--untick",
            "pnpm",
            "--untick",
            "yarn",
        ])
        .expect("parses");
        let Command::Clean(CleanArgs {
            action: Some(CleanAction::Caches(a)),
            ..
        }) = cli.command
        else {
            panic!("not clean caches");
        };
        assert!(a.confirm && a.wipe);
        assert_eq!(a.untick.len(), 2);
        assert_eq!(a.untick[1].0.len(), 2, "yarn = classic + berry");
        // A dry-run preview of the wipe is allowed; an unknown tool is not.
        assert!(Cli::try_parse_from(["scull", "clean", "caches", "--wipe"]).is_ok());
        assert!(Cli::try_parse_from(["scull", "clean", "caches", "--untick", "cargo"]).is_err());
        assert!(
            Cli::try_parse_from(["scull", "clean", "caches", "--dry-run", "--confirm"]).is_err()
        );
    }
}
