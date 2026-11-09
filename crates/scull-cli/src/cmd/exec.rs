//! Shared pieces of the commands that change the disk (`scull clean`, and the Quick Clean,
//! restore and quarantine purge commands built on it): who and where a plan is bound to, the
//! refusal to run elevated (SAFE-14), the elevated-confirmation rule (SAFE-07, CLI-03),
//! terminal prompts, IDs, the execution lock (SAFE-09) and the database.
//!
//! # Which confirmations apply (SAFE-07, D-3, D-28, D-36)
//!
//! * **Standard confirmation** (`--confirm`, or answering `y` at the prompt on a terminal):
//!   every destructive run. S0 items are deleted after it (D-3: regeneration is their restore
//!   path); S1/S2 items are moved into same-volume quarantine (restorable with
//!   `scull restore`) or, where quarantine is impossible, the Trash.
//! * **Elevated confirmation** (typing the item count on a terminal; `--allow-permanent`
//!   without one, CLI-03): any **S1/S2 item whose action can't be undone by scull**
//!   ([`needs_elevated_confirmation`]): its restorability is "regenerate" (delete) or "not
//!   restorable" (tool command, `git worktree remove/prune`). Under D-28 `Delete` is S0-only,
//!   so a `scull clean` plan never deletes an S1/S2 item, and the planner does not put tool or
//!   Git actions into `scull clean` plans; the rule is still enforced on every plan file, so a
//!   future action, or a plan made elsewhere, can't slip past it. Permanently deleting
//!   quarantined (S1/S2) items, i.e. purging the quarantine before or after its retention, is
//!   the other permanent delete of S1/S2 data and takes the same elevated confirmation.
//! * **Package caches** (Quick Clean, QC-04) take a single confirmation and no elevated one:
//!   they hold no unique data. An orphaned cache whose tool is uninstalled (D-36), which
//!   scull deletes itself, is listed per tool with the not-undoable note and takes its own
//!   confirmation per tool (`--remove-orphaned TOOL`, or its own prompt on a terminal); the
//!   single Quick Clean confirmation never covers it.
//! * Elevated runs are refused unless `--allow-root` ([`check_not_elevated`], SAFE-14).

use crate::output::Ui;
use scull_cleaner::{Executor, LockOptions, QuarantineSettings};
use scull_core::config::QuarantineConfig;
use scull_core::{ErrorKind, ScullError};
use scull_platform::{AppDirs, Elevation, is_elevated};
use scull_storage::Store;
use std::io::{self, BufRead, IsTerminal, Write};

/// Test hook: when set (non-empty), scull behaves as if it ran elevated. It can only add a
/// refusal, never remove one, so it is safe to honor in release builds.
pub const ASSUME_ELEVATED_ENV: &str = "SCULL_TEST_ASSUME_ELEVATED";

/// The flag that allows destructive runs as root/administrator (SAFE-14).
pub const ALLOW_ROOT_FLAG: &str = "--allow-root";

/// The flag that stands in for the typed elevated confirmation without a terminal (CLI-03).
pub const ALLOW_PERMANENT_FLAG: &str = "--allow-permanent";

/// The machine and user a plan is bound to (SAFE-11, PRD §10.4): the shared flow's binding.
pub use scull_cleaner::flow::Binding as Who;

/// scull's per-user directories (`SCULL_HOME` overrides them).
pub fn dirs() -> Result<AppDirs, ScullError> {
    scull_platform::app_dirs().map_err(|e| ScullError::new(ErrorKind::Config, e.to_string()))
}

/// This machine's ID (created on first use) and the current user.
pub fn who() -> Result<Who, ScullError> {
    Who::current(&dirs()?.data)
}

/// Whether this process runs elevated, honoring [`ASSUME_ELEVATED_ENV`].
pub fn elevation() -> Elevation {
    if std::env::var_os(ASSUME_ELEVATED_ENV).is_some_and(|v| !v.is_empty()) {
        return Elevation::Elevated;
    }
    is_elevated()
}

/// SAFE-14 / FX-EXEC-12: refuse a destructive run as root or administrator (or when that
/// can't be told) unless `allow_root`. `what` names the command for the message.
pub fn check_not_elevated(
    elevation: Elevation,
    allow_root: bool,
    what: &str,
) -> Result<(), ScullError> {
    if !elevation.blocks_destructive() || allow_root {
        return Ok(());
    }
    let how = match elevation {
        Elevation::Unknown => {
            "could not tell whether it runs as root/administrator, so it assumes it does"
        }
        _ => "is running as root/administrator",
    };
    Err(ScullError::new(
        ErrorKind::SafetyBlocked,
        format!(
            "{what} {how} and refuses to change anything (SAFE-14): scull never needs elevated \
             rights to clean your own files, and an elevated run could remove things your \
             account can't. Run it as your normal user, or pass {ALLOW_ROOT_FLAG} if you really \
             mean it. Nothing was changed."
        ),
    ))
}

pub use scull_cleaner::flow::needs_elevated_confirmation;

/// Whether prompts can be shown and answered: a human format, and stdin and stderr are
/// terminals. Without one, nothing destructive runs without `--confirm` (CLI-03).
pub fn interactive(ui: &Ui) -> bool {
    !ui.format.is_machine() && io::stdin().is_terminal() && io::stderr().is_terminal()
}

/// Ask a yes/no question on `err`, reading the answer from `input`. Only `y` or `yes` (any
/// case) accepts; anything else, including an empty line or end of input, declines.
pub fn ask_yes_no(
    input: &mut dyn BufRead,
    err: &mut dyn Write,
    question: &str,
) -> io::Result<bool> {
    write!(err, "{question} [y/N] ")?;
    err.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let a = line.trim().to_ascii_lowercase();
    Ok(a == "y" || a == "yes")
}

/// Ask for `expected` to be typed exactly (surrounding whitespace ignored). Anything else
/// declines.
pub fn ask_typed(
    input: &mut dyn BufRead,
    err: &mut dyn Write,
    prompt: &str,
    expected: &str,
) -> io::Result<bool> {
    write!(err, "{prompt} ")?;
    err.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    Ok(line.trim() == expected)
}

/// [`ask_yes_no`] on the real terminal.
pub fn prompt_yes_no(question: &str) -> Result<bool, ScullError> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut err = io::stderr().lock();
    ask_yes_no(&mut input, &mut err, question).map_err(|e| ScullError::from_io(&e))
}

/// [`ask_typed`] on the real terminal.
pub fn prompt_typed(prompt: &str, expected: &str) -> Result<bool, ScullError> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut err = io::stderr().lock();
    ask_typed(&mut input, &mut err, prompt, expected).map_err(|e| ScullError::from_io(&e))
}

pub use scull_cleaner::flow::{new_id_suffix, new_operation_id, new_plan_id};

/// The quarantine settings of a run (P2-EXEC-13): the configured `[quarantine]` base and
/// per-volume bases (`SCULL_QUARANTINE_BASE` included); nothing configured means the automatic
/// location (the shallowest folder above the item, on its volume, that the user owns).
pub fn quarantine_settings(config: &QuarantineConfig) -> QuarantineSettings {
    QuarantineSettings::from_config(config)
}

pub use scull_cleaner::flow::trash_locations;

/// An Executor holding the current user's execution lock (SAFE-09); fails at once with
/// `lock_held` (exit 6) when another operation runs.
pub fn lock() -> Result<Executor, ScullError> {
    Executor::acquire(&LockOptions::for_current_user()?)
}

/// The execution lock, then every operation a crash interrupted reconciled with the disk
/// ([`scull_cleaner::flow::lock_and_recover`]); recovered operations are reported on stderr.
pub fn lock_and_recover(store: &mut Store, ui: &Ui) -> Result<Executor, ScullError> {
    let (executor, report) = scull_cleaner::flow::lock_and_recover(store)?;
    if !ui.quiet {
        for op in &report.operations {
            let _ = writeln!(
                io::stderr(),
                "scull: recovered interrupted operation {} ({} item(s) settled from the journal \
                 and the disk; nothing was resumed). See `scull history {}`.",
                op.operation_id,
                op.items.len(),
                op.operation_id
            );
        }
    }
    Ok(executor)
}

/// The database in scull's data directory (created and migrated if needed).
pub fn open_store() -> Result<Store, ScullError> {
    let dirs = dirs()?;
    scull_engine::open_history(&dirs.data).map_err(ScullError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scull_core::{Action, Tier};

    #[test]
    fn elevated_confirmation_covers_unrestorable_s1_s2_only() {
        let tool = Action::ToolCommand {
            tool: "npm".into(),
            args: vec!["cache".into(), "verify".into()],
        };
        // S0 delete: standard confirmation (D-3).
        assert!(!needs_elevated_confirmation(Tier::S0, &Action::Delete));
        assert!(!needs_elevated_confirmation(Tier::S0, &tool));
        // Restorable moves never need it.
        for t in [Tier::S1, Tier::S2] {
            assert!(!needs_elevated_confirmation(t, &Action::Quarantine));
            assert!(!needs_elevated_confirmation(t, &Action::Trash));
        }
        // Irreversible at S1/S2 (FX-EXEC-11).
        assert!(needs_elevated_confirmation(Tier::S1, &tool));
        assert!(needs_elevated_confirmation(
            Tier::S1,
            &Action::GitWorktreeRemove
        ));
        assert!(needs_elevated_confirmation(
            Tier::S1,
            &Action::GitWorktreePrune
        ));
        assert!(needs_elevated_confirmation(Tier::S1, &Action::Delete));
        assert!(needs_elevated_confirmation(Tier::S2, &Action::Delete));
        assert!(!needs_elevated_confirmation(Tier::S3, &Action::None));
    }

    #[test]
    fn elevated_runs_are_refused_without_allow_root() {
        assert!(check_not_elevated(Elevation::NotElevated, false, "scull clean").is_ok());
        for e in [Elevation::Elevated, Elevation::Unknown] {
            let err = check_not_elevated(e, false, "scull clean").unwrap_err();
            assert_eq!(err.kind, ErrorKind::SafetyBlocked);
            assert!(err.message.contains(ALLOW_ROOT_FLAG), "{}", err.message);
            assert!(check_not_elevated(e, true, "scull clean").is_ok());
        }
    }

    #[test]
    fn prompts_decline_by_default() {
        let ask = |answer: &str| {
            let mut input = io::Cursor::new(answer.as_bytes().to_vec());
            let mut err = Vec::new();
            ask_yes_no(&mut input, &mut err, "Go?").unwrap()
        };
        assert!(ask("y\n"));
        assert!(ask(" YES \n"));
        assert!(!ask("\n"));
        assert!(!ask(""));
        assert!(!ask("no\n"));
        assert!(!ask("yep\n"));

        let typed = |answer: &str| {
            let mut input = io::Cursor::new(answer.as_bytes().to_vec());
            let mut err = Vec::new();
            let ok = ask_typed(&mut input, &mut err, "Type 2:", "2").unwrap();
            assert_eq!(String::from_utf8(err).unwrap(), "Type 2: ");
            ok
        };
        assert!(typed("2\n"));
        assert!(typed("  2  \n"));
        assert!(!typed("y\n"));
        assert!(!typed("3\n"));
        assert!(!typed(""));
    }

    #[test]
    fn ids_are_valid_and_unique() {
        let a = new_plan_id(1_800_000_000).unwrap();
        let b = new_plan_id(1_800_000_000).unwrap();
        assert_ne!(a, b);
        assert!(new_operation_id(1_800_000_000).is_ok());
    }
}
