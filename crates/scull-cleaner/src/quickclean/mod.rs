//! Quick Clean: package-manager caches cleaned with each tool's own command (P2-QC-01; PRD
//! §10.8 QC-02…06, SAFE-06, SAFE-12; threats T-22, T-23).
//!
//! Package caches hold no unique data. They are S1 but bypass quarantine (QC-02): scull runs
//! the tool's **own** cleanup, a prune where the tool has one and a full wipe only when the
//! user asks for it (QC-03). Nothing here deletes a file itself.
//!
//! # Flow
//!
//! 1. [`QuickClean::plan`] takes the located caches ([`CacheScan`], from
//!    `scull_detectors::locate_caches`) and their classified [`Candidate`]s, and decides per
//!    tool: the resolved binary, the exact arguments and environment, the folder the tool
//!    will act on, the mode and the not-undoable note; or why the tool is skipped
//!    ([`SkipReason`], QC-05). Nothing is changed (a Yarn version probe is the only process
//!    started).
//! 2. The preview shows [`PlannedTool::command_line`] (the **resolved binary path**, SAFE-12),
//!    [`NOT_UNDOABLE_NOTE`] and every skipped tool. One confirmation (QC-04).
//! 3. The confirmed Quick Clean becomes a recorded plan ([`QuickCleanPlan::audit_plan`]) and
//!    an [`Operation`] under the execution lock (see [Audit](#audit-safe-08-safe-19)).
//! 4. [`QuickClean::run`] revalidates each tool right before starting it (same cache identity,
//!    no link, same volume, not in use, same binary, still the approved command), runs it,
//!    audits it, and reports the exit status, the escaped and capped output, the cache size and
//!    the volume's free space before and after (QC-06), and for pnpm why a prune may free
//!    little.
//!
//! # Which caches are planned
//!
//! A located cache is planned only when **all** of these hold, otherwise it is skipped with
//! the reason:
//!
//! * the user did not untick it ([`QuickCleanRequest::unticked`], FX-QC-08);
//! * the locator found no problem: not reached through a symlink or junction, not on another
//!   volume than its anchor, and the volume check could run (QC-05, FX-QC-04);
//! * its layout was recognized and it has a file identity;
//! * a candidate for the same path and rule exists at **S0/S1** with a tool command (D-28: an
//!   S2 or S3 cache gets no tool action);
//! * nothing holds it: no process has its working directory or an open file inside the
//!   folder the tool acts on, and the probe could see everything ("unknown" skips too, PRD
//!   §8.4) (QC-05, FX-QC-03);
//! * a trusted binary exists (see [Binary](#binary-safe-12-t-22)) and, for Yarn, it is the
//!   right major version for that cache (1.x for the classic cache, 2+ for the Berry cache).
//!
//! Locations the locator **refused** (T-23, e.g. a user `.npmrc` with `cache=~`) are skipped
//! with [`SkipReason::Refused`] (FX-QC-07).
//!
//! # Binary (SAFE-12, T-22)
//!
//! Resolved from the injected search list ([`QuickCleanOptions::search_path`], the CLI passes
//! `PATH`) by [`TrustedDirs`]: never an empty or relative entry, the current directory, a
//! `node_modules` folder or a scan root, and a symlink is checked after resolving it. Only
//! real executables are started, never through a shell.
//!
//! **Windows `.cmd` shims (P2-QC-02).** npm, pnpm and Yarn usually install `.cmd` shims, which
//! would need `cmd.exe`. Quick Clean never runs them: it **reads** an npm-style shim
//! (cmd-shim, Node's bundled `npm.cmd`, Yarn's installer, pnpm's own), takes the Node program
//! and the JavaScript entry point it names, checks both with the trusted-locations rules (Node:
//! the shim folder's `node.exe` or a trusted `node` executable; the script: a regular file
//! inside the shim's trusted directory, not the current directory or a scan root) and starts
//! `node.exe <script> <args>` itself with structured arguments ([`Lookup::NodeScript`],
//! [`PlannedTool::script`]). Any other shim, and `.bat`/`.ps1`, is still skipped with
//! [`SkipReason::ShellShimOnly`]. `cmd.exe` is never started.
//!
//! This is a local copy of `scull-git`'s resolver (generalized to any tool name); a shared
//! helper would have to live in another lane's crate.
//!
//! # Process (SAFE-12)
//!
//! Spawned by absolute path with [`std::process::Command`] (no shell), structured arguments,
//! null stdin, a timeout ([`QuickCleanOptions::timeout`]; the process is killed when it
//! expires or the operation is cancelled) and capped output. Working directory: an empty
//! private folder, so no project configuration (`.npmrc`, `.yarnrc.yml`, `package.json`) in
//! the user's current directory applies (T-23, FX-QC-07). There are no positional operands,
//! only fixed words and option values that are absolute paths, so no `--` separator is
//! needed (an option value can't be mistaken for an option).
//!
//! **Environment.** Cleared, then only: `PATH` (the trusted directories, so `#!/usr/bin/env
//! node` finds a trusted `node`), `HOME`, `USERPROFILE`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`,
//! `XDG_DATA_HOME`, `XDG_STATE_HOME`, `APPDATA`, `LOCALAPPDATA`, `TMPDIR`, `TEMP`, `TMP`
//! (all empty private folders, so no user-level tool configuration is read either),
//! `LC_ALL=C`, `LANG=C`, `NO_COLOR=1`, `npm_config_update_notifier=false`,
//! `npm_config_offline=true`, `COREPACK_ENABLE_NETWORK=0`,
//! `COREPACK_ENABLE_DOWNLOAD_PROMPT=0`, `COREPACK_ENABLE_AUTO_PIN=0`, `YARN_IGNORE_PATH=1`
//! (never exec a `yarnPath` from some rc file), `YARN_ENABLE_TELEMETRY=0`,
//! `YARN_ENABLE_NETWORK=0`, plus [`QuickCleanOptions::passthrough_env`] (`SystemRoot` on
//! Windows, which processes need to load system DLLs) and the tool's location (below).
//!
//! Residual risk: npm walks up from the working directory looking for a project root, and
//! Yarn Berry merges `.yarnrc.yml` files from every ancestor. The sandbox lives in
//! [`QuickCleanOptions::sandbox_base`] (default: the system temp folder, which is per-user on
//! macOS and Windows but shared `/tmp` on Linux); pass an owner-only folder to remove the
//! shared ancestors. The cache location itself is always passed explicitly and takes
//! precedence over any rc file, and `YARN_IGNORE_PATH` blocks the one rc setting that runs code.
//!
//! # The same path (T-23)
//!
//! The tool runs with a sanitized environment, so its own notion of where the cache is would
//! differ from what the locator found. Each tool is told the location explicitly through its
//! own option, which takes precedence over environment and every config file (Bun, Yarn and
//! conda have no such option: their location variable, which outranks their config files):
//!
//! | Tool | Prune (default) | Wipe (only when requested) | Location passed as |
//! |---|---|---|---|
//! | npm | `npm cache verify` | `npm cache clean --force` | `--cache <dir>` (the folder holding `_cacache`; npm's `cache` config) |
//! | pnpm | `pnpm store prune` | none: never wipe the store | `--store-dir <dir>` (pnpm's `store-dir` config) |
//! | Bun | — | `bun pm cache rm` | `BUN_INSTALL_CACHE_DIR=<dir>` |
//! | Yarn classic | — | `yarn cache clean` | `YARN_CACHE_FOLDER=<dir>` |
//! | Yarn Berry | — | `yarn cache clean --mirror` | `YARN_GLOBAL_FOLDER=<dir>` (the folder holding `cache/`) |
//! | pip | — | `pip cache purge` | `--cache-dir <dir>`, plus `PIP_DISABLE_PIP_VERSION_CHECK=1`, `PIP_NO_INPUT=1` |
//! | uv | `uv cache prune` | `uv cache clean` | `--cache-dir <dir>` (outranks `UV_CACHE_DIR`), plus `UV_NO_CONFIG=1`, `UV_OFFLINE=1` |
//! | conda | `conda clean --packages --tarballs --yes` | none: never wipe `pkgs/` | `CONDA_PKGS_DIRS=<dir>`, plus `CONDA_OFFLINE=true` |
//!
//! conda has no cache-location option, so `CONDA_PKGS_DIRS` names the folder. Residual risk:
//! conda merges `pkgs_dirs` from every source, so a **system** condarc (`/etc/conda/condarc`,
//! the install's own `.condarc`) listing other package caches adds them to the run; the user
//! condarc files are not read (`HOME` and the XDG folders are empty private folders).
//! `conda clean --packages` only removes extracted packages no environment links to, and
//! `--tarballs` only downloaded archives, in each of them.
//!
//! Bun, Yarn and pip have no prune: their only cleanup removes the whole cache (`pip cache
//! purge` empties pip's HTTP and wheel caches), and it is what Quick Clean runs for them
//! ([`CleanMode::Wipe`] with [`PlannedTool::wipe_requested`] false). Asking pnpm or conda for a
//! wipe still runs the prune.
//!
//! # Tool not installed, cache left behind (FX-QC-05)
//!
//! QC-03 says such a cache is removed "through the safe executor", but the Executor's only
//! removal is `Action::Delete`, which D-28 allows at S0 only, and a package cache is S1
//! (QC-02); quarantine would free nothing (QC-02). **D-36** (provisional, #217) resolves this
//! with one narrow path ([`crate::orphaned`]), not a core rule change:
//!
//! * the tool runner still skips the tool with [`SkipReason::NotInstalled`] (manual removal);
//! * when the cache also qualifies (S1 package cache, validated location, not a link, same
//!   volume, not in use, and not conda's `pkgs/`, which environments hard-link into:
//!   [`crate::orphaned::not_orphan_removable`]), it is listed in [`QuickCleanPlan::orphaned`] for the preview, with
//!   the not-undoable note;
//! * only if the user confirms **that tool** does [`QuickClean::remove_orphaned`] gather fresh
//!   [`OrphanedCacheEvidence`] (every check again, right before) and remove the folder with
//!   [`Executor::delete_orphaned_cache`] (the fd-relative, no-follow walker), under the
//!   execution lock.
//!
//! Without that confirmation nothing is removed.
//!
//! # Audit (SAFE-08, SAFE-19)
//!
//! Every Quick Clean runs inside an [`Operation`] (P2-EXEC-12), like a cleanup plan:
//!
//! 1. [`QuickCleanPlan::audit_plan`] turns the confirmed preview into an immutable
//!    [`Plan`]: one item per planned tool, whose action is the **resolved binary path and the
//!    exact arguments** ([`Action::ToolCommand`], escaped and capped: [`MAX_AUDIT_ARG_CHARS`],
//!    [`MAX_AUDIT_ARGS`]), with the candidate's predicted bytes; plus one item per orphaned
//!    cache whose tool the user confirmed. The caller records it (`Store::insert_plan`) and
//!    starts the operation ([`Executor::begin_operation`]: approver, method, plan hash).
//! 2. [`QuickClean::run`] and [`QuickClean::remove_orphaned`] take that operation. They act only
//!    on a tool or cache whose item is exactly one of the operation's plan items (what was
//!    approved); anything else is skipped with [`SkipReason::NotApproved`] and not started.
//!    Each item gets exactly one audit row:
//!    * a tool that ran: `tool_run` (exit status 0) with the bytes the cache shrank by as
//!      `freed_bytes`; otherwise `failed` (cache provably unchanged) or `partial`, with
//!      `tool_exit:<code>`, `tool_killed`, `tool_timeout`, `cancelled` or `spawn_failed`;
//!    * a tool or cache not started: `skipped` with [`SkipReason::code`] (`refused` for
//!      `elevated`, `failed` for `sandbox_unavailable`);
//!    * an orphaned cache: the removal walk is journaled first (intent with the cache's path,
//!      identity, parent and tree fingerprint, so crash recovery can reconcile it, SAFE-19),
//!      then audited as `removed` with the D-36 evidence summary as its reason
//!      (`d36_orphaned:searched_<n>_dirs`), or `partial` / `failed` / `refused`.
//!
//!    Items not attempted because the run was cancelled get no row
//!    ([`QuickCleanReport::cancelled`]): finish the operation with `OperationEnd::Cancelled`.
//! 3. Both measure the free space of the caches' volumes around their work through the
//!    operation's meter; [`Operation::finish`] records the sum as the measured delta next to the
//!    predicted bytes (I-5).
//!
//! Residual gap: a tool run is not journaled (the journal has no step kind for it); if scull
//! crashes while a tool runs, recovery records that tool as `skipped` / `interrupted`.

mod audit;
mod output;
mod process;
mod resolve;
mod shim;

pub use audit::{MAX_AUDIT_ARG_CHARS, MAX_AUDIT_ARGS};
pub use output::{CapturedOutput, escape_line, needs_escape};
pub use process::Ended;
pub use resolve::{Lookup, NodeScript, TrustedDirs};
pub use scull_detectors::caches::NOT_UNDOABLE_NOTE;
pub use shim::MAX_SHIM_BYTES;

use crate::executor::{DeleteTarget, Executor};
use crate::journal::{StepPoint, at, fingerprint};
use crate::measure::{ItemExecution, Tally, planned_sizes};
use crate::operation::Operation;
use crate::orphaned::{OrphanedCacheEvidence, not_orphan_removable};
use process::{Ran, Sandbox, Spec};
use scull_core::{
    Action, Candidate, Category, FileIdentity, FsPath, ItemId, Plan, PlanItem, PlanParams,
    ScullError, Tier,
};
use scull_detectors::caches::{CacheProblem, CacheScan, CacheTool, LocatedCache, NativeCommand};
use scull_engine::now_unix;
use scull_platform::remove::RemoveReport;
use scull_platform::{Elevation, Platform, free_space};
use scull_safety::{ActivityProbe, ProbeResult, Tri, check_in_use};
use scull_scanner::ActionReclaim;
use scull_scanner::{CancelToken, NullSink, ScanOptions};
use scull_storage::{ItemRecord, JournalAction, JournalIntent};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Default per-tool timeout (a large npm cache verify takes minutes).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Default cap on each of stdout and stderr, per tool.
pub const DEFAULT_MAX_OUTPUT: usize = 64 * 1024;

/// Longest a `yarn --version` probe may take.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Fixed variables of the sanitized environment (see the module docs).
const FIXED_ENV: [(&str, &str); 11] = [
    ("LC_ALL", "C"),
    ("LANG", "C"),
    ("NO_COLOR", "1"),
    ("npm_config_update_notifier", "false"),
    ("npm_config_offline", "true"),
    ("COREPACK_ENABLE_NETWORK", "0"),
    ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
    ("COREPACK_ENABLE_AUTO_PIN", "0"),
    ("YARN_IGNORE_PATH", "1"),
    ("YARN_ENABLE_TELEMETRY", "0"),
    ("YARN_ENABLE_NETWORK", "0"),
];

/// Machine facts and limits for a Quick Clean.
#[derive(Debug, Clone)]
pub struct QuickCleanOptions {
    /// The trusted-locations search list, `PATH` syntax (the CLI passes the user's `PATH`;
    /// tests a temp dir). Filtered by [`TrustedDirs`].
    pub search_path: OsString,
    /// The current directory: never trusted for binaries.
    pub cwd: Option<PathBuf>,
    /// The scan roots: never trusted for binaries.
    pub excluded_roots: Vec<PathBuf>,
    /// Per-tool timeout.
    pub timeout: Duration,
    /// Cap on each of stdout and stderr, per tool.
    pub max_output: usize,
    /// Where the private sandbox (neutral working directory, empty home) is created; the
    /// system temp folder when `None`.
    pub sandbox_base: Option<PathBuf>,
    /// Variables passed through to the tool unchanged (`SystemRoot` on Windows).
    pub passthrough_env: Vec<(String, OsString)>,
    /// Run even when elevated (SAFE-14, `--allow-root`).
    pub allow_elevated: bool,
    /// Resolve npm-style `.cmd` shims to `node <script>` (P2-QC-02). On by default on Windows
    /// only (where these shims are installed); the fixtures turn it on elsewhere.
    pub cmd_shims: bool,
}

impl QuickCleanOptions {
    /// Defaults for `search_path`; on Windows, passes `SystemRoot` through.
    pub fn new(search_path: impl Into<OsString>) -> Self {
        let mut passthrough_env = Vec::new();
        if cfg!(windows)
            && let Some(v) = std::env::var_os("SystemRoot")
        {
            passthrough_env.push(("SystemRoot".to_owned(), v));
        }
        Self {
            search_path: search_path.into(),
            cwd: None,
            excluded_roots: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            max_output: DEFAULT_MAX_OUTPUT,
            sandbox_base: None,
            passthrough_env,
            allow_elevated: false,
            cmd_shims: cfg!(windows),
        }
    }
}

/// What a tool's command does to its cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CleanMode {
    /// Removes only unused or old entries.
    Prune,
    /// Removes everything.
    Wipe,
}

impl fmt::Display for CleanMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CleanMode::Prune => "prune",
            CleanMode::Wipe => "wipe",
        })
    }
}

/// The user's choices on the Quick Clean card.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuickCleanRequest {
    /// Tools the user unticked (QC-04, FX-QC-08): never run.
    pub unticked: BTreeSet<CacheTool>,
    /// Tools for which the user asked for the full wipe instead of the prune (QC-03).
    pub wipe: BTreeSet<CacheTool>,
}

/// One tool Quick Clean will run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTool {
    /// The tool.
    pub tool: CacheTool,
    /// The candidate's item ID.
    pub item_id: ItemId,
    /// The cache folder, as located (what sizes refer to).
    pub cache: PathBuf,
    /// Its no-follow identity at planning time (revalidated before running).
    pub identity: FileIdentity,
    /// The folder the locator checked the cache from (home, or the configured location's
    /// parent).
    pub anchor: PathBuf,
    /// The folder handed to the tool as its cache location (npm: the folder holding
    /// `_cacache`; Yarn Berry: the global folder holding `cache/`; otherwise the cache).
    pub location: PathBuf,
    /// The resolved binary: canonical absolute path from a trusted directory (SAFE-12). For a
    /// tool installed as a `.cmd` shim, the trusted Node executable.
    pub binary: PathBuf,
    /// For a `.cmd` shim (P2-QC-02): the tool's JavaScript entry point that Node runs; it is
    /// also the first of [`PlannedTool::args`].
    pub script: Option<PathBuf>,
    /// For a `.cmd` shim: the shim it was resolved from (read, never run).
    pub shim: Option<PathBuf>,
    /// The exact arguments after the binary (for a shim: the script, then the tool's own).
    pub args: Vec<OsString>,
    /// Tool-specific variables (the location) added to the sanitized environment.
    pub env: Vec<(String, OsString)>,
    /// What the command does.
    pub mode: CleanMode,
    /// Whether the user asked for the wipe.
    pub wipe_requested: bool,
    /// The candidate's reclaimable estimate, bytes.
    pub reclaimable_bytes: u64,
    /// Extra facts for the preview (e.g. "pnpm has no wipe").
    pub notes: Vec<String>,
    /// [`NOT_UNDOABLE_NOTE`].
    pub not_undoable: &'static str,
    /// The plan item recorded for this tool ([`QuickCleanPlan::audit_plan`]): the candidate's,
    /// with its action resolved to the binary and arguments above (escaped and capped).
    pub item: PlanItem,
}

impl PlannedTool {
    /// The command as the preview shows it: location variables, the resolved binary and the
    /// arguments. Display only: the command is never run through a shell.
    pub fn command_line(&self) -> String {
        let mut s = String::new();
        for (k, v) in &self.env {
            s.push_str(&format!("{k}={} ", v.to_string_lossy()));
        }
        s.push_str(&self.binary.to_string_lossy());
        for a in &self.args {
            s.push(' ');
            s.push_str(&a.to_string_lossy());
        }
        s
    }
}

/// Why a tool was not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The locator refused the configured location (T-23, QC-05).
    Refused(String),
    /// The user unticked it (FX-QC-08).
    Unticked,
    /// A QC-05 location check failed (symlink or junction, other volume).
    Problem(CacheProblem),
    /// The cache's volume could not be compared with its anchor's (no file identity).
    VolumeUnverified,
    /// The folder doesn't look like this tool's cache.
    Unrecognized,
    /// No candidate was classified for this cache.
    NotClassified,
    /// The candidate's tier or action doesn't allow a tool command (D-28: S2/S3 get none).
    NotEligible {
        /// The candidate's tier.
        tier: Tier,
    },
    /// The folder layout isn't the one the location option expects (e.g. npm's cache not
    /// named `_cacache`), so the tool could not be pointed at the same folder.
    UnexpectedLayout(String),
    /// A process holds the folder: working directory or open file (QC-05, FX-QC-03).
    InUse(Vec<String>),
    /// Whether a process holds the folder could not be determined (treated as in use).
    ActivityUnknown(Vec<String>),
    /// No trusted binary: the cache is left behind for manual removal (see the module docs).
    NotInstalled {
        /// The folder left in place.
        manual_removal: PathBuf,
    },
    /// Only a shim scull can't run without a shell exists (SAFE-12): a `.bat`/`.ps1`, or a
    /// `.cmd` that is not an npm-style Node shim or whose Node or script isn't trusted
    /// (P2-QC-02).
    ShellShimOnly(PathBuf),
    /// The binary's version doesn't match this cache (e.g. Yarn 1 for the Berry cache).
    WrongVersion {
        /// What `--version` printed (escaped), or why it failed.
        reported: String,
    },
    /// Something changed between planning and running (SAFE-05).
    Changed(String),
    /// The process is elevated and [`QuickCleanOptions::allow_elevated`] is off (SAFE-14).
    Elevated(Elevation),
    /// The operation was cancelled before this tool started.
    Cancelled,
    /// The private sandbox could not be created.
    Sandbox(String),
    /// Not one of the items of the operation's plan: not approved, so never started and not
    /// audited (see [Audit](self#audit-safe-08-safe-19)).
    NotApproved,
    /// D-36 never removes this tool's cache itself (conda's `pkgs/`, which environments
    /// hard-link into: [`crate::orphaned::not_orphan_removable`]).
    NotOrphanRemovable(String),
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::Refused(r) => write!(f, "location refused: {r}"),
            SkipReason::Unticked => f.write_str("unticked"),
            SkipReason::Problem(CacheProblem::Link { path }) => write!(
                f,
                "{} is a symlink or junction: its target can't be verified",
                path.display()
            ),
            SkipReason::Problem(CacheProblem::OtherVolume { anchor }) => {
                write!(f, "on a different volume than {}", anchor.display())
            }
            SkipReason::VolumeUnverified => {
                f.write_str("its volume could not be verified (no file identity)")
            }
            SkipReason::Unrecognized => f.write_str("contents don't match the tool's cache layout"),
            SkipReason::NotClassified => f.write_str("no classified candidate for this cache"),
            SkipReason::NotEligible { tier } => {
                write!(
                    f,
                    "classified {tier}: tool commands are allowed at S0/S1 only"
                )
            }
            SkipReason::UnexpectedLayout(r) => write!(f, "unexpected layout: {r}"),
            SkipReason::InUse(by) => write!(f, "in use ({})", by.join("; ")),
            SkipReason::ActivityUnknown(why) => {
                write!(
                    f,
                    "could not check whether it is in use ({})",
                    why.join("; ")
                )
            }
            SkipReason::NotInstalled { manual_removal } => write!(
                f,
                "tool not installed (no trusted binary); {} left in place for manual removal",
                manual_removal.display()
            ),
            SkipReason::ShellShimOnly(p) => {
                write!(f, "only {} found, which needs a shell to run", p.display())
            }
            SkipReason::WrongVersion { reported } => {
                write!(f, "binary version doesn't match this cache: {reported}")
            }
            SkipReason::Changed(r) => write!(f, "changed since planning: {r}"),
            SkipReason::Elevated(e) => write!(f, "refusing to run tools while {e} (SAFE-14)"),
            SkipReason::Cancelled => f.write_str("cancelled"),
            SkipReason::Sandbox(e) => write!(f, "could not create the private sandbox: {e}"),
            SkipReason::NotApproved => f.write_str("not part of the approved operation plan"),
            SkipReason::NotOrphanRemovable(why) => f.write_str(why),
        }
    }
}

/// A tool that was not (or will not be) run, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedTool {
    /// The tool.
    pub tool: CacheTool,
    /// Its cache folder, if known.
    pub path: Option<PathBuf>,
    /// Why.
    pub reason: SkipReason,
}

/// A process scull started (I-7: only allowlisted tools, from trusted paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    /// The binary.
    pub binary: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
}

/// A cache whose tool is not installed and that scull may remove itself, through the
/// Executor, if the user confirms this tool (D-36, provisional). Also listed in
/// [`QuickCleanPlan::skipped`] as [`SkipReason::NotInstalled`]: the tool runner never touches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedCache {
    /// The uninstalled tool.
    pub tool: CacheTool,
    /// The cache folder.
    pub cache: PathBuf,
    /// The folder it was located from (the removal's anchor).
    pub anchor: PathBuf,
    /// Its no-follow identity at planning time.
    pub identity: FileIdentity,
    /// The classified item (S1 package cache).
    pub item: PlanItem,
    /// The candidate's reclaimable estimate, bytes.
    pub reclaimable_bytes: u64,
    /// [`NOT_UNDOABLE_NOTE`]: the preview must show it and ask for this tool's confirmation.
    pub not_undoable: &'static str,
}

/// The Quick Clean preview: what will run, and what won't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickCleanPlan {
    /// Tools to run, in reporting order.
    pub tools: Vec<PlannedTool>,
    /// Tools skipped, with reasons (QC-05).
    pub skipped: Vec<SkippedTool>,
    /// Processes started while planning (Yarn version probes).
    pub spawned: Vec<Spawn>,
    /// Caches of uninstalled tools that qualify for removal by scull (D-36), each only after
    /// its own confirmation ([`QuickClean::remove_orphaned`]).
    pub orphaned: Vec<OrphanedCache>,
}

impl QuickCleanPlan {
    /// Sum of the planned tools' reclaimable estimates.
    pub fn total_reclaimable(&self) -> u64 {
        self.tools.iter().map(|t| t.reclaimable_bytes).sum()
    }

    /// The plan of record for this Quick Clean (SAFE-08): one item per planned tool
    /// ([`PlannedTool::item`]: resolved binary and exact arguments) and one per orphaned cache
    /// whose tool is in `confirmed_orphans` ([`OrphanedCache::item`]). Record it and start an
    /// [`Operation`] for it before [`QuickClean::run`] / [`QuickClean::remove_orphaned`]. Fails
    /// like [`Plan::new`] (e.g. an empty machine ID or user).
    pub fn audit_plan(
        &self,
        confirmed_orphans: &BTreeSet<CacheTool>,
        params: PlanParams,
    ) -> Result<Plan, ScullError> {
        let items = self
            .tools
            .iter()
            .map(|t| t.item.clone())
            .chain(
                self.orphaned
                    .iter()
                    .filter(|o| confirmed_orphans.contains(&o.tool))
                    .map(|o| o.item.clone()),
            )
            .collect();
        Plan::new(params, items)
    }
}

/// A cache folder's measured size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheSize {
    /// Allocated bytes (hard-linked files counted once).
    pub allocated: u64,
    /// Logical bytes.
    pub logical: u64,
    /// Allocated bytes of files also linked from outside the folder (pnpm: still referenced
    /// by projects).
    pub shared_allocated: u64,
    /// Whether the walk saw everything.
    pub complete: bool,
}

/// What running one tool did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// The tool.
    pub tool: CacheTool,
    /// The cache folder.
    pub cache: PathBuf,
    /// The binary run.
    pub binary: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// The mode.
    pub mode: CleanMode,
    /// How it ended.
    pub ended: Ended,
    /// Captured stdout, escaped and capped.
    pub stdout: CapturedOutput,
    /// Captured stderr, escaped and capped.
    pub stderr: CapturedOutput,
    /// How long it ran.
    pub elapsed: Duration,
    /// Free space on the cache's volume before, bytes.
    pub free_before: Option<u64>,
    /// Free space on the cache's volume after, bytes.
    pub free_after: Option<u64>,
    /// The cache's size before.
    pub size_before: Option<CacheSize>,
    /// The cache's size after (zero when the tool removed the folder).
    pub size_after: Option<CacheSize>,
    /// Why less was freed than one might expect (pnpm, QC-06).
    pub explanation: Option<String>,
}

impl ToolResult {
    /// Whether the tool exited with status 0.
    pub fn succeeded(&self) -> bool {
        self.ended == Ended::Exited(Some(0))
    }

    /// Measured change in the volume's free space (positive: space freed). Other activity on
    /// the volume shows up here too.
    pub fn free_space_delta(&self) -> Option<i128> {
        Some(i128::from(self.free_after?) - i128::from(self.free_before?))
    }

    /// Allocated bytes the cache shrank by (negative if it grew).
    pub fn cache_bytes_freed(&self) -> Option<i128> {
        Some(i128::from(self.size_before?.allocated) - i128::from(self.size_after?.allocated))
    }
}

/// What removing one orphaned cache did (D-36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedRemoval {
    /// The uninstalled tool.
    pub tool: CacheTool,
    /// The cache folder.
    pub cache: PathBuf,
    /// The walker's report (it may be incomplete: entries kept, each with a reason), or why
    /// the Executor refused (nothing removed).
    pub outcome: Result<RemoveReport, ScullError>,
    /// Free space on the cache's volume before, bytes.
    pub free_before: Option<u64>,
    /// Free space on the cache's volume after, bytes.
    pub free_after: Option<u64>,
    /// The cache's size before.
    pub size_before: Option<CacheSize>,
    /// [`NOT_UNDOABLE_NOTE`].
    pub not_undoable: &'static str,
}

/// What [`QuickClean::remove_orphaned`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OrphanedReport {
    /// Confirmed caches the Executor was asked to remove.
    pub removed: Vec<OrphanedRemoval>,
    /// Confirmed caches not removed, with the reason (a check failed right before).
    pub skipped: Vec<SkippedTool>,
    /// Whether caches were left unattempted because the run was cancelled (they have no audit
    /// row: finish the operation as cancelled).
    pub cancelled: bool,
    /// The measured free-space delta of the caches' volumes around the removals, bytes
    /// (added to the operation's measured delta).
    pub measured_delta_bytes: Option<i64>,
}

/// What a Quick Clean run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickCleanReport {
    /// Tools that were started, in order.
    pub results: Vec<ToolResult>,
    /// Tools skipped at planning or at run time, with reasons.
    pub skipped: Vec<SkippedTool>,
    /// Every process started.
    pub spawned: Vec<Spawn>,
    /// Whether tools were left unstarted because the run was cancelled (they have no audit
    /// row: finish the operation as cancelled).
    pub cancelled: bool,
    /// The measured free-space delta of the caches' volumes around the tool runs, bytes
    /// (added to the operation's measured delta).
    pub measured_delta_bytes: Option<i64>,
}

/// The Quick Clean runner. See the module docs.
pub struct QuickClean {
    opts: QuickCleanOptions,
    trusted: TrustedDirs,
    platform: Arc<dyn Platform>,
    elevation: Elevation,
}

impl fmt::Debug for QuickClean {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuickClean")
            .field("opts", &self.opts)
            .field("trusted", &self.trusted)
            .field("elevation", &self.elevation)
            .finish_non_exhaustive()
    }
}

impl QuickClean {
    /// A runner for `opts`, checking the disk through `platform` (no-follow metadata, sizes).
    /// The trusted directories are resolved once, here.
    pub fn new(opts: QuickCleanOptions, platform: Arc<dyn Platform>) -> Self {
        let trusted =
            TrustedDirs::new(&opts.search_path, opts.cwd.as_deref(), &opts.excluded_roots)
                .with_cmd_shims(opts.cmd_shims);
        Self {
            opts,
            trusted,
            platform,
            elevation: scull_platform::is_elevated(),
        }
    }

    /// Use `elevation` instead of asking the OS (tests).
    #[must_use]
    pub fn with_elevation(mut self, elevation: Elevation) -> Self {
        self.elevation = elevation;
        self
    }

    /// The trusted directories binaries are resolved from.
    pub fn trusted_dirs(&self) -> &TrustedDirs {
        &self.trusted
    }

    /// Decide what to run for each located cache. `candidates` are the classified candidates
    /// for the same caches (matched by path and rule ID); `probe` answers whether a folder is
    /// in use. See the module docs for the rules.
    pub fn plan(
        &self,
        scan: &CacheScan,
        candidates: &[Candidate],
        request: &QuickCleanRequest,
        probe: &dyn ActivityProbe,
    ) -> QuickCleanPlan {
        let mut plan = QuickCleanPlan {
            tools: Vec::new(),
            skipped: Vec::new(),
            spawned: Vec::new(),
            orphaned: Vec::new(),
        };
        let mut sandbox: Option<Result<Sandbox, String>> = None;
        for r in &scan.refused {
            plan.skipped.push(SkippedTool {
                tool: r.tool,
                path: r.path.clone(),
                reason: SkipReason::Refused(r.reason.clone()),
            });
        }
        for c in &scan.found {
            match self.plan_one(
                c,
                candidates,
                request,
                probe,
                &mut sandbox,
                &mut plan.spawned,
            ) {
                Ok(t) => plan.tools.push(t),
                Err(reason) => {
                    if matches!(reason, SkipReason::NotInstalled { .. })
                        && let Some(o) = self.orphaned(c, candidates, probe)
                    {
                        plan.orphaned.push(o);
                    }
                    plan.skipped.push(SkippedTool {
                        tool: c.tool,
                        path: Some(c.path.clone()),
                        reason,
                    });
                }
            }
        }
        plan
    }

    fn plan_one(
        &self,
        c: &LocatedCache,
        candidates: &[Candidate],
        request: &QuickCleanRequest,
        probe: &dyn ActivityProbe,
        sandbox: &mut Option<Result<Sandbox, String>>,
        spawned: &mut Vec<Spawn>,
    ) -> Result<PlannedTool, SkipReason> {
        if request.unticked.contains(&c.tool) {
            return Err(SkipReason::Unticked);
        }
        if let Some(p) = c.problems.first() {
            return Err(SkipReason::Problem(p.clone()));
        }
        if !c.volume_checked {
            return Err(SkipReason::VolumeUnverified);
        }
        if !c.recognized {
            return Err(SkipReason::Unrecognized);
        }
        let identity = c.identity.ok_or(SkipReason::VolumeUnverified)?;
        let cand = candidates
            .iter()
            .find(|k| k.path == FsPath::from_path(&c.path) && k.rule.id == c.tool.rule_id())
            .ok_or(SkipReason::NotClassified)?;
        let eligible = cand.tier <= Tier::S1
            && matches!(cand.action, Action::ToolCommand { .. })
            && cand.action.allowed_for(cand.tier);
        if !eligible {
            return Err(SkipReason::NotEligible { tier: cand.tier });
        }
        if cand.identity != identity {
            return Err(SkipReason::Changed(
                "the candidate was classified for a different folder identity".into(),
            ));
        }
        let location = location_of(c)?;
        let mut notes = Vec::new();
        let (binary, script, shim) = match self.trusted.find(c.tool.binary()) {
            Lookup::Found(b) => (b, None, None),
            Lookup::NodeScript(n) => {
                notes.push(format!(
                    "{} is a .cmd shim: scull runs Node on its script directly, without a shell",
                    n.shim.display()
                ));
                (n.node, Some(n.script), Some(n.shim))
            }
            Lookup::ShellShimOnly(p) => return Err(SkipReason::ShellShimOnly(p)),
            Lookup::NotFound => {
                return Err(SkipReason::NotInstalled {
                    manual_removal: c.path.clone(),
                });
            }
        };
        if matches!(c.tool, CacheTool::YarnClassic | CacheTool::YarnBerry) {
            self.check_yarn_version(c.tool, &binary, script.as_deref(), sandbox, spawned)?;
        }
        in_use(&location, probe)?;

        let commands = c.commands();
        let wipe_requested = request.wipe.contains(&c.tool);
        let (cmd, mode): (NativeCommand, CleanMode) = match (wipe_requested, commands.wipe) {
            (true, Some(w)) => (w, CleanMode::Wipe),
            (true, None) if commands.preferred_prunes => {
                notes.push(format!(
                    "{} has no full wipe; running the prune instead",
                    c.tool.binary()
                ));
                (commands.preferred, CleanMode::Prune)
            }
            (_, _) if commands.preferred_prunes => (commands.preferred, CleanMode::Prune),
            _ => (commands.preferred, CleanMode::Wipe),
        };
        if c.tool == CacheTool::Pnpm {
            notes.push(
                "the store is never removed directly: projects' node_modules hard-link into it"
                    .into(),
            );
        }
        let (tool_args, env) = tool_invocation(c.tool, cmd, &location);
        let args: Vec<OsString> = script
            .iter()
            .map(|s| s.as_os_str().to_owned())
            .chain(tool_args)
            .collect();
        let item = PlanItem::from_candidate(cand)
            .map_err(|_| SkipReason::NotEligible { tier: cand.tier })?;
        let item = audit::tool_item(item, &binary, &args);
        Ok(PlannedTool {
            tool: c.tool,
            item_id: cand.id.clone(),
            cache: c.path.clone(),
            identity,
            anchor: c.anchor.clone(),
            location,
            binary,
            script,
            shim,
            args,
            env,
            mode,
            wipe_requested,
            reclaimable_bytes: cand.sizes.reclaimable.bytes,
            notes,
            not_undoable: NOT_UNDOABLE_NOTE,
            item,
        })
    }

    /// D-36: whether the located cache of an uninstalled tool qualifies for removal by scull
    /// (S1 package cache, validated location, not a link, same volume, not in use).
    fn orphaned(
        &self,
        c: &LocatedCache,
        candidates: &[Candidate],
        probe: &dyn ActivityProbe,
    ) -> Option<OrphanedCache> {
        if not_orphan_removable(c.tool).is_some() {
            return None;
        }
        if !c.problems.is_empty() || !c.volume_checked || !c.recognized {
            return None;
        }
        let identity = c.identity?;
        let cand = candidates
            .iter()
            .find(|k| k.path == FsPath::from_path(&c.path) && k.rule.id == c.tool.rule_id())?;
        if cand.tier != Tier::S1
            || cand.category != Category::PackageCache
            || cand.identity != identity
            || !matches!(cand.action, Action::ToolCommand { .. })
        {
            return None;
        }
        let item = PlanItem::from_candidate(cand).ok()?;
        self.gather_evidence(c.tool, &c.path, &c.anchor, identity, probe)
            .ok()?;
        Some(OrphanedCache {
            tool: c.tool,
            cache: c.path.clone(),
            anchor: c.anchor.clone(),
            identity,
            item,
            reclaimable_bytes: cand.sizes.reclaimable.bytes,
            not_undoable: NOT_UNDOABLE_NOTE,
        })
    }

    /// Gathers D-36 evidence for `cache` right now: no trusted binary, the folder is a
    /// directory with the located identity, nothing between its anchor and it is a link, it is
    /// on its anchor's volume, and a fresh probe says nothing holds it.
    fn gather_evidence(
        &self,
        tool: CacheTool,
        cache: &Path,
        anchor: &Path,
        identity: FileIdentity,
        probe: &dyn ActivityProbe,
    ) -> Result<OrphanedCacheEvidence, SkipReason> {
        if let Some(why) = not_orphan_removable(tool) {
            return Err(SkipReason::NotOrphanRemovable(why.into()));
        }
        match self.trusted.find(tool.binary()) {
            Lookup::NotFound => {}
            Lookup::ShellShimOnly(p) => return Err(SkipReason::ShellShimOnly(p)),
            Lookup::Found(b) | Lookup::NodeScript(NodeScript { shim: b, .. }) => {
                return Err(SkipReason::Changed(format!(
                    "`{}` is installed now ({}): use its own command",
                    tool.binary(),
                    b.display()
                )));
            }
        }
        let anchor_identity = self.check_location(cache, anchor, identity)?;
        in_use(cache, probe)?;
        Ok(OrphanedCacheEvidence::checked(
            tool,
            cache.to_path_buf(),
            anchor.to_path_buf(),
            identity,
            anchor_identity,
            self.trusted.dirs().len(),
        ))
    }

    /// D-36: remove the orphaned caches of `plan` whose tool the user confirmed, one by one,
    /// each right after gathering fresh evidence ([`OrphanedCacheEvidence`]), through
    /// [`Executor::delete_orphaned_cache`] (`exec` must hold the execution lock: the one `op`
    /// was started with). Tools not in `confirmed` are left alone, and so is a cache whose item
    /// is not in `op`'s plan ([`SkipReason::NotApproved`]). Not undoable ([`NOT_UNDOABLE_NOTE`]).
    /// Stops at `cancel`.
    ///
    /// Each removal is journaled before the walk and audited after it in `op` (SAFE-19,
    /// SAFE-08; see [Audit](self#audit-safe-08-safe-19)); every other confirmed cache gets a
    /// `skipped` or `refused` row. `Err` means the journal or the audit could not be written:
    /// the cache it was for was not touched, and nothing after it was attempted.
    pub fn remove_orphaned(
        &self,
        exec: &Executor,
        op: &mut Operation<'_>,
        plan: &QuickCleanPlan,
        confirmed: &BTreeSet<CacheTool>,
        probe: &dyn ActivityProbe,
        cancel: &CancelToken,
    ) -> Result<OrphanedReport, ScullError> {
        let mut report = OrphanedReport::default();
        let chosen: Vec<&OrphanedCache> = plan
            .orphaned
            .iter()
            .filter(|o| confirmed.contains(&o.tool))
            .collect();
        let anchors: Vec<PathBuf> = chosen.iter().map(|o| o.anchor.clone()).collect();
        let baseline = if chosen.is_empty() {
            None
        } else {
            op.measure_before(&anchors)
        };
        let mut tally = Tally::new();
        for o in chosen {
            let skip = |reason| SkippedTool {
                tool: o.tool,
                path: Some(o.cache.clone()),
                reason,
            };
            if !op.approved(&o.item) {
                report.skipped.push(skip(SkipReason::NotApproved));
                continue;
            }
            let predicted = ActionReclaim::for_action(&planned_sizes(&o.item), &o.item.action);
            let not_done = |report: &mut OrphanedReport,
                            tally: &mut Tally,
                            op: &mut Operation<'_>,
                            reason: SkipReason|
             -> Result<(), ScullError> {
                tally.add(&predicted, &[], ItemExecution::NotExecuted);
                if reason == SkipReason::Cancelled {
                    report.cancelled = true;
                } else {
                    op.record(&reason.audit_record(o.item.item_id.clone(), now_unix()))?;
                }
                report.skipped.push(skip(reason));
                Ok(())
            };
            if cancel.is_cancelled() {
                not_done(&mut report, &mut tally, op, SkipReason::Cancelled)?;
                continue;
            }
            if self.elevation.blocks_destructive() && !self.opts.allow_elevated {
                not_done(
                    &mut report,
                    &mut tally,
                    op,
                    SkipReason::Elevated(self.elevation),
                )?;
                continue;
            }
            let evidence =
                match self.gather_evidence(o.tool, &o.cache, &o.anchor, o.identity, probe) {
                    Ok(e) => e,
                    Err(reason) => {
                        not_done(&mut report, &mut tally, op, reason)?;
                        continue;
                    }
                };
            let free_before = free_space(&o.anchor).ok();
            let size_before = self.measure(&o.cache);
            let searched = evidence.directories_searched();
            let id = &o.item.item_id;
            journal_removal(op, exec, o, &evidence)?;
            let target = DeleteTarget {
                anchor: &o.anchor,
                anchor_identity: Some(evidence.anchor_identity()),
                path: &o.cache,
            };
            let outcome = exec
                .delete_orphaned_cache(&o.item, &target, evidence)
                .map(|d| d.report);
            let recorded_at = now_unix();
            let (rec, done) = match &outcome {
                Ok(walk) => audit::removal_record(id.clone(), walk, searched, recorded_at),
                Err(e) => (
                    ItemRecord::from_error(id.clone(), recorded_at, e),
                    ItemExecution::NotExecuted,
                ),
            };
            tally.add(&predicted, &[], done);
            op.journal_close(Some(&rec), recorded_at)?;
            at(exec.hook(), StepPoint::AfterClose(id));
            op.record(&rec)?;
            at(exec.hook(), StepPoint::AfterAudit(id));
            report.removed.push(OrphanedRemoval {
                tool: o.tool,
                cache: o.cache.clone(),
                outcome,
                free_before,
                free_after: free_space(&o.anchor).ok(),
                size_before,
                not_undoable: NOT_UNDOABLE_NOTE,
            });
        }
        report.measured_delta_bytes = op
            .measure_after(baseline, &tally)
            .and_then(|d| d.measured_delta_bytes());
        Ok(report)
    }

    /// Runs `yarn --version` (sandboxed; `node <script> --version` for a shim) and checks the
    /// major version against the cache.
    fn check_yarn_version(
        &self,
        tool: CacheTool,
        binary: &Path,
        script: Option<&Path>,
        sandbox: &mut Option<Result<Sandbox, String>>,
        spawned: &mut Vec<Spawn>,
    ) -> Result<(), SkipReason> {
        let sb = sandbox
            .get_or_insert_with(|| {
                Sandbox::new(self.opts.sandbox_base.as_deref()).map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| SkipReason::Sandbox(e.clone()))?;
        let args: Vec<OsString> = script
            .map(|s| s.as_os_str().to_owned())
            .into_iter()
            .chain([OsString::from("--version")])
            .collect();
        let env = self.base_env(sb);
        spawned.push(Spawn {
            binary: binary.to_path_buf(),
            args: args.clone(),
        });
        let ran = process::run(&Spec {
            binary,
            args: &args,
            env: &env,
            cwd: &sb.cwd,
            timeout: self.opts.timeout.min(VERSION_PROBE_TIMEOUT),
            max_output: 256,
            cancel: &CancelToken::new(),
        });
        let out = CapturedOutput::from_bytes(&ran.stdout, ran.stdout_truncated);
        let first = out
            .lines
            .first()
            .map(|l| l.trim().to_owned())
            .unwrap_or_default();
        if ran.ended != Ended::Exited(Some(0)) {
            return Err(SkipReason::WrongVersion {
                reported: format!("`yarn --version` failed ({:?}) {first}", ran.ended),
            });
        }
        let major = first.split('.').next().and_then(|m| m.parse::<u32>().ok());
        let ok = match (tool, major) {
            (CacheTool::YarnClassic, Some(1)) => true,
            (CacheTool::YarnBerry, Some(m)) => m >= 2,
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(SkipReason::WrongVersion { reported: first })
        }
    }

    /// Run `plan` inside `op` (started for [`QuickCleanPlan::audit_plan`]; see
    /// [Audit](self#audit-safe-08-safe-19)): revalidate each tool, run it, measure it, audit
    /// it. A tool whose item is not in `op`'s plan is never started
    /// ([`SkipReason::NotApproved`]). Stops starting tools once `cancel` is set (a running tool
    /// is killed). `probe` should be a fresh view of running processes.
    ///
    /// `Err` means an audit row could not be written; no tool is started after that.
    pub fn run(
        &self,
        op: &mut Operation<'_>,
        plan: &QuickCleanPlan,
        probe: &dyn ActivityProbe,
        cancel: &CancelToken,
    ) -> Result<QuickCleanReport, ScullError> {
        let mut report = QuickCleanReport {
            results: Vec::new(),
            skipped: plan.skipped.clone(),
            spawned: Vec::new(),
            cancelled: false,
            measured_delta_bytes: None,
        };
        let mut approved = Vec::new();
        for t in &plan.tools {
            if op.approved(&t.item) {
                approved.push(t);
            } else {
                report.skipped.push(SkippedTool {
                    tool: t.tool,
                    path: Some(t.cache.clone()),
                    reason: SkipReason::NotApproved,
                });
            }
        }
        if approved.is_empty() {
            return Ok(report);
        }
        let anchors: Vec<PathBuf> = approved.iter().map(|t| t.anchor.clone()).collect();
        let baseline = op.measure_before(&anchors);
        let mut tally = Tally::new();
        let predicted =
            |t: &PlannedTool| ActionReclaim::for_action(&planned_sizes(&t.item), &t.item.action);
        let not_started = |report: &mut QuickCleanReport,
                           tally: &mut Tally,
                           op: &mut Operation<'_>,
                           t: &PlannedTool,
                           reason: SkipReason|
         -> Result<(), ScullError> {
            tally.add(&predicted(t), &[], ItemExecution::NotExecuted);
            if reason == SkipReason::Cancelled {
                report.cancelled = true;
            } else {
                op.record(&reason.audit_record(t.item.item_id.clone(), now_unix()))?;
            }
            report.skipped.push(SkippedTool {
                tool: t.tool,
                path: Some(t.cache.clone()),
                reason,
            });
            Ok(())
        };
        let sandbox = if self.elevation.blocks_destructive() && !self.opts.allow_elevated {
            Err(SkipReason::Elevated(self.elevation))
        } else {
            Sandbox::new(self.opts.sandbox_base.as_deref())
                .map_err(|e| SkipReason::Sandbox(e.to_string()))
        };
        let sb = match sandbox {
            Ok(sb) => sb,
            Err(reason) => {
                for t in approved {
                    not_started(&mut report, &mut tally, op, t, reason.clone())?;
                }
                report.measured_delta_bytes = op
                    .measure_after(baseline, &tally)
                    .and_then(|d| d.measured_delta_bytes());
                return Ok(report);
            }
        };
        let base_env = self.base_env(&sb);
        for t in approved {
            if cancel.is_cancelled() {
                not_started(&mut report, &mut tally, op, t, SkipReason::Cancelled)?;
                continue;
            }
            if let Err(reason) = self.revalidate(t, probe) {
                not_started(&mut report, &mut tally, op, t, reason)?;
                continue;
            }
            let free_before = free_space(&t.anchor).ok();
            let size_before = self.measure(&t.cache);
            let mut env = base_env.clone();
            env.extend(t.env.iter().cloned());
            report.spawned.push(Spawn {
                binary: t.binary.clone(),
                args: t.args.clone(),
            });
            let ran: Ran = process::run(&Spec {
                binary: &t.binary,
                args: &t.args,
                env: &env,
                cwd: &sb.cwd,
                timeout: self.opts.timeout,
                max_output: self.opts.max_output,
                cancel,
            });
            let size_after = self.measure(&t.cache);
            let free_after = free_space(&t.anchor).ok();
            let mut result = ToolResult {
                tool: t.tool,
                cache: t.cache.clone(),
                binary: t.binary.clone(),
                args: t.args.clone(),
                mode: t.mode,
                ended: ran.ended,
                stdout: CapturedOutput::from_bytes(&ran.stdout, ran.stdout_truncated),
                stderr: CapturedOutput::from_bytes(&ran.stderr, ran.stderr_truncated),
                elapsed: ran.elapsed,
                free_before,
                free_after,
                size_before,
                size_after,
                explanation: None,
            };
            result.explanation = pnpm_explanation(&result);
            let (rec, done) = audit::tool_record(t.item.item_id.clone(), &result, now_unix());
            tally.add(&predicted(t), &[], done);
            report.results.push(result);
            op.record(&rec)?;
        }
        report.measured_delta_bytes = op
            .measure_after(baseline, &tally)
            .and_then(|d| d.measured_delta_bytes());
        Ok(report)
    }

    /// SAFE-05-style checks right before starting a tool.
    fn revalidate(&self, t: &PlannedTool, probe: &dyn ActivityProbe) -> Result<(), SkipReason> {
        self.check_location(&t.cache, &t.anchor, t.identity)?;
        if self.trusted.find(t.tool.binary()).program()
            != Some((t.binary.clone(), t.script.clone()))
        {
            return Err(SkipReason::Changed(format!(
                "the trusted `{}` program is no longer {}",
                t.tool.binary(),
                t.binary.display()
            )));
        }
        in_use(&t.location, probe)
    }

    /// The cache is still a directory with `identity`, nothing between `anchor` and it is a
    /// link (no-follow lookups), and it is on the anchor's volume. Returns the anchor's
    /// identity.
    fn check_location(
        &self,
        cache: &Path,
        anchor: &Path,
        identity: FileIdentity,
    ) -> Result<FileIdentity, SkipReason> {
        let md = self
            .platform
            .metadata(cache)
            .map_err(|e| SkipReason::Changed(format!("{} can't be read: {e}", cache.display())))?;
        if md.kind.is_link() {
            return Err(SkipReason::Problem(CacheProblem::Link {
                path: cache.to_path_buf(),
            }));
        }
        if md.kind != scull_platform::EntryKind::Dir {
            return Err(SkipReason::Changed(format!(
                "{} is no longer a folder",
                cache.display()
            )));
        }
        if md.identity != Some(identity) {
            return Err(SkipReason::Changed(format!(
                "{} was replaced (different file identity)",
                cache.display()
            )));
        }
        // Every component strictly below the anchor, down to the cache (no-follow each).
        let rest = cache.strip_prefix(anchor).map_err(|_| {
            SkipReason::Changed(format!(
                "{} is not inside {}",
                cache.display(),
                anchor.display()
            ))
        })?;
        let mut at = anchor.to_path_buf();
        for comp in rest.components() {
            at.push(comp.as_os_str());
            match self.platform.metadata(&at) {
                Ok(m) if m.kind.is_link() => {
                    return Err(SkipReason::Problem(CacheProblem::Link { path: at }));
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(SkipReason::Changed(format!("{}: {e}", at.display())));
                }
            }
        }
        let anchor_identity = self.platform.metadata(anchor).ok().and_then(|m| m.identity);
        match anchor_identity {
            Some(a) if a.volume == identity.volume => Ok(a),
            Some(_) => Err(SkipReason::Problem(CacheProblem::OtherVolume {
                anchor: anchor.to_path_buf(),
            })),
            None => Err(SkipReason::VolumeUnverified),
        }
    }

    /// The cache's size, or zero when it no longer exists. `None` when it can't be measured.
    fn measure(&self, path: &Path) -> Option<CacheSize> {
        match self.platform.metadata(path) {
            Err(e) if e.is_not_found() => {
                return Some(CacheSize {
                    complete: true,
                    ..CacheSize::default()
                });
            }
            Err(_) => return None,
            Ok(_) => {}
        }
        let mut opts = ScanOptions::default();
        opts.threads = 2;
        let out = scull_scanner::scan(
            self.platform.clone(),
            path,
            &opts,
            &CancelToken::new(),
            NullSink,
        )
        .ok()?;
        let root = out.root?;
        Some(CacheSize {
            allocated: root.totals.allocated,
            logical: root.totals.logical,
            shared_allocated: root.shared_allocated,
            complete: root.complete,
        })
    }

    /// The sanitized environment (see the module docs), without the tool's location.
    fn base_env(&self, sb: &Sandbox) -> Vec<(String, OsString)> {
        let mut env = self.opts.passthrough_env.clone();
        let mut put = |k: &str, v: OsString| env.push((k.to_owned(), v));
        put("PATH", self.trusted.joined());
        for (k, v) in [
            ("HOME", &sb.home),
            ("USERPROFILE", &sb.home),
            ("XDG_CONFIG_HOME", &sb.config),
            ("XDG_CACHE_HOME", &sb.cache),
            ("XDG_DATA_HOME", &sb.data),
            ("XDG_STATE_HOME", &sb.state),
            ("APPDATA", &sb.config),
            ("LOCALAPPDATA", &sb.data),
            ("TMPDIR", &sb.tmp),
            ("TEMP", &sb.tmp),
            ("TMP", &sb.tmp),
        ] {
            put(k, v.as_os_str().to_owned());
        }
        for (k, v) in FIXED_ENV {
            put(k, OsString::from(v));
        }
        env
    }
}

/// Journals the intent of an orphaned cache's removal walk (SAFE-19), exactly like a plan
/// item's: path, identity, kind, parent identity and the tree's fingerprint, so crash recovery
/// can tell an untouched cache from a partly removed one.
fn journal_removal(
    op: &mut Operation<'_>,
    exec: &Executor,
    o: &OrphanedCache,
    evidence: &OrphanedCacheEvidence,
) -> Result<(), ScullError> {
    let id = &o.item.item_id;
    at(exec.hook(), StepPoint::BeforeIntent(id));
    let parent = o
        .cache
        .parent()
        .and_then(|p| scull_platform::Native.metadata(p).ok())
        .and_then(|m| m.identity);
    let intent = JournalIntent {
        operation_id: op.id().clone(),
        item_id: id.clone(),
        action: JournalAction::Delete,
        source_path: FsPath::from_path(&o.cache),
        source_identity: evidence.identity(),
        source_kind: scull_platform::EntryKind::Dir.as_str().to_owned(),
        source_parent: parent,
        destination: None,
        destination_dir: None,
        tree: Some(fingerprint(&o.cache)),
        recorded_at: now_unix(),
    };
    op.journal_intent(&intent)?;
    at(exec.hook(), StepPoint::AfterIntent(id));
    Ok(())
}

/// The folder handed to the tool as its location (see the module docs table).
fn location_of(c: &LocatedCache) -> Result<PathBuf, SkipReason> {
    let parent_if_named = |name: &str| -> Result<PathBuf, SkipReason> {
        match (c.path.file_name(), c.path.parent()) {
            (Some(n), Some(p)) if n == name => Ok(p.to_path_buf()),
            _ => Err(SkipReason::UnexpectedLayout(format!(
                "{} is not named `{name}`",
                c.path.display()
            ))),
        }
    };
    let loc = match c.tool {
        CacheTool::Npm => parent_if_named("_cacache")?,
        CacheTool::YarnBerry => parent_if_named("cache")?,
        CacheTool::Pnpm
        | CacheTool::Bun
        | CacheTool::YarnClassic
        | CacheTool::Pip
        | CacheTool::Uv
        | CacheTool::Conda => c.path.clone(),
    };
    // Absolute, so it can never be read as an option.
    if !loc.is_absolute() {
        return Err(SkipReason::UnexpectedLayout(format!(
            "{} is not absolute",
            loc.display()
        )));
    }
    Ok(loc)
}

/// The exact arguments and the location variables for `cmd` acting on `location`.
fn tool_invocation(
    tool: CacheTool,
    cmd: NativeCommand,
    location: &Path,
) -> (Vec<OsString>, Vec<(String, OsString)>) {
    let mut args: Vec<OsString> = cmd.args.iter().map(OsString::from).collect();
    let mut env = Vec::new();
    let loc = location.as_os_str().to_owned();
    match tool {
        CacheTool::Npm => {
            args.push("--cache".into());
            args.push(loc);
        }
        CacheTool::Pnpm => {
            args.push("--store-dir".into());
            args.push(loc);
        }
        CacheTool::Bun => env.push(("BUN_INSTALL_CACHE_DIR".to_owned(), loc)),
        CacheTool::YarnClassic => env.push(("YARN_CACHE_FOLDER".to_owned(), loc)),
        CacheTool::YarnBerry => env.push(("YARN_GLOBAL_FOLDER".to_owned(), loc)),
        // pip's general options are accepted after the subcommand; the flag outranks
        // `PIP_CACHE_DIR` and every pip config file.
        CacheTool::Pip => {
            args.push("--cache-dir".into());
            args.push(loc);
            env.push(("PIP_DISABLE_PIP_VERSION_CHECK".to_owned(), "1".into()));
            env.push(("PIP_NO_INPUT".to_owned(), "1".into()));
        }
        // uv's global `--cache-dir` outranks `UV_CACHE_DIR` and `uv.toml`; `UV_NO_CONFIG`
        // stops uv from reading any `uv.toml` (system, user, or an ancestor of the sandbox).
        CacheTool::Uv => {
            args.push("--cache-dir".into());
            args.push(loc);
            env.push(("UV_NO_CONFIG".to_owned(), "1".into()));
            env.push(("UV_OFFLINE".to_owned(), "1".into()));
        }
        // conda has no cache-location option: `CONDA_PKGS_DIRS` (one entry, this folder) is
        // its highest-precedence source for `pkgs_dirs`.
        CacheTool::Conda => {
            env.push(("CONDA_PKGS_DIRS".to_owned(), loc));
            env.push(("CONDA_OFFLINE".to_owned(), "true".into()));
        }
    }
    (args, env)
}

/// QC-05: nothing may hold `dir`; "unknown" counts as held.
fn in_use(dir: &Path, probe: &dyn ActivityProbe) -> Result<(), SkipReason> {
    let verdict = check_in_use(dir, &[], probe);
    match verdict.in_use {
        Tri::No => Ok(()),
        Tri::Yes => Err(SkipReason::InUse(
            verdict
                .answers
                .iter()
                .filter_map(|(signal, r)| match r {
                    ProbeResult::InUse(holders) => Some(
                        holders
                            .iter()
                            .map(|h| {
                                let who = h.name.as_deref().map_or_else(
                                    || format!("pid {}", h.pid),
                                    |n| format!("{} (pid {})", escape_line(n), h.pid),
                                );
                                format!("{who}: {}", signal.label())
                            })
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .flatten()
                .collect(),
        )),
        Tri::Unknown => Err(SkipReason::ActivityUnknown(
            verdict
                .answers
                .iter()
                .filter_map(|(signal, r)| match r {
                    ProbeResult::Unknown(why) => Some(format!("{}: {why}", signal.label())),
                    _ => None,
                })
                .collect(),
        )),
    }
}

/// QC-06: why a pnpm prune freed less than half of the store.
fn pnpm_explanation(r: &ToolResult) -> Option<String> {
    if r.tool != CacheTool::Pnpm || r.mode != CleanMode::Prune {
        return None;
    }
    let before = r.size_before?;
    let after = r.size_after?;
    let freed = before.allocated.saturating_sub(after.allocated);
    if before.allocated == 0 || freed.saturating_mul(2) >= before.allocated {
        return None;
    }
    Some(format!(
        "`pnpm store prune` only removes packages that no project references. {} of {} bytes \
         remain, {} of them hard-linked from projects' node_modules: that space is freed only \
         when those projects are removed or reinstalled.",
        after.allocated, before.allocated, after.shared_allocated
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_passes_the_location_through_each_tools_own_option() {
        let loc = PathBuf::from(if cfg!(windows) {
            r"C:\u\.npm"
        } else {
            "/u/.npm"
        });
        let cmd = |tool, args| NativeCommand { tool, args };
        let (a, e) = tool_invocation(CacheTool::Npm, cmd("npm", &["cache", "verify"]), &loc);
        assert_eq!(
            a,
            [
                "cache",
                "verify",
                "--cache",
                loc.to_str().unwrap_or_default()
            ]
        );
        assert!(e.is_empty());
        let (a, e) = tool_invocation(CacheTool::Pnpm, cmd("pnpm", &["store", "prune"]), &loc);
        assert_eq!(
            a,
            [
                "store",
                "prune",
                "--store-dir",
                loc.to_str().unwrap_or_default()
            ]
        );
        assert!(e.is_empty());
        for (tool, var) in [
            (CacheTool::Bun, "BUN_INSTALL_CACHE_DIR"),
            (CacheTool::YarnClassic, "YARN_CACHE_FOLDER"),
            (CacheTool::YarnBerry, "YARN_GLOBAL_FOLDER"),
        ] {
            let (a, e) = tool_invocation(tool, cmd("x", &["cache", "clean"]), &loc);
            assert_eq!(a, ["cache", "clean"]);
            assert_eq!(e, [(var.to_owned(), loc.clone().into_os_string())]);
        }
    }

    #[test]
    fn pnpm_explanation_only_when_prune_freed_little() {
        let size = |allocated, shared| {
            Some(CacheSize {
                allocated,
                logical: allocated,
                shared_allocated: shared,
                complete: true,
            })
        };
        let mut r = ToolResult {
            tool: CacheTool::Pnpm,
            cache: PathBuf::from("/s"),
            binary: PathBuf::from("/bin/pnpm"),
            args: Vec::new(),
            mode: CleanMode::Prune,
            ended: Ended::Exited(Some(0)),
            stdout: CapturedOutput::default(),
            stderr: CapturedOutput::default(),
            elapsed: Duration::ZERO,
            free_before: Some(10),
            free_after: Some(30),
            size_before: size(100, 80),
            size_after: size(90, 80),
            explanation: None,
        };
        assert!(pnpm_explanation(&r).is_some_and(|e| e.contains("80 of them")));
        assert_eq!(r.free_space_delta(), Some(20));
        assert_eq!(r.cache_bytes_freed(), Some(10));
        r.size_after = size(40, 0);
        assert_eq!(pnpm_explanation(&r), None);
        r.tool = CacheTool::Npm;
        r.size_after = size(90, 80);
        assert_eq!(pnpm_explanation(&r), None);
    }

    #[test]
    fn command_line_shows_location_binary_and_arguments() {
        let t = PlannedTool {
            tool: CacheTool::Bun,
            item_id: ItemId::derive(FileIdentity::new(1, 2), b"/c"),
            cache: PathBuf::from("/c"),
            identity: FileIdentity::new(1, 2),
            anchor: PathBuf::from("/"),
            location: PathBuf::from("/c"),
            binary: PathBuf::from("/usr/bin/bun"),
            script: None,
            shim: None,
            args: vec!["pm".into(), "cache".into(), "rm".into()],
            env: vec![("BUN_INSTALL_CACHE_DIR".into(), "/c".into())],
            mode: CleanMode::Wipe,
            wipe_requested: false,
            reclaimable_bytes: 0,
            notes: Vec::new(),
            not_undoable: NOT_UNDOABLE_NOTE,
            item: PlanItem {
                item_id: ItemId::derive(FileIdentity::new(1, 2), b"/c"),
                identity: FileIdentity::new(1, 2),
                path: FsPath::from("/c"),
                category: Category::PackageCache,
                rule: scull_core::RuleRef {
                    id: CacheTool::Bun.rule_id().into(),
                    version: 1,
                },
                tier: Tier::S1,
                action: Action::None,
                reclaimable_bytes: 0,
            },
        };
        assert_eq!(
            t.command_line(),
            "BUN_INSTALL_CACHE_DIR=/c /usr/bin/bun pm cache rm"
        );
    }
}
