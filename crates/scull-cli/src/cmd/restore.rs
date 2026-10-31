//! `scull restore <operation-id>` and `scull clean --purge-quarantine` (P2-CLI-03; PRD §10.6,
//! §12, SAFE-07, SAFE-16).
//!
//! # Restore
//!
//! Puts an operation's quarantined items back where they were, through
//! `scull_cleaner::Executor::restore`: the database manifest is authoritative (the on-disk
//! `manifest.json` is never read), identity, kind and size are verified, the original folder
//! must be the same directory object, and **nothing is ever overwritten**: a taken path gets a
//! `<name>.scull-restored-<n>` sibling, reported as such. `--item` (item ID, original path or
//! manifest entry number) restores a subset. Without `--confirm` it previews (and asks on a
//! terminal); refused elevated without `--allow-root`; runs under the execution lock after
//! crash recovery. Deleted items (S0, regenerate them) and items moved to the Trash (restore
//! them from the Trash) are listed as not restorable by scull. Exit 3 if an entry was refused.
//! Machine output: `scull/restore@1`.
//!
//! # Quarantine purge
//!
//! `scull clean --purge-quarantine [--older-than AGE]` permanently deletes held quarantine
//! entries whose retention has expired (or that are older than AGE) through
//! `scull_cleaner::Executor::purge_quarantine` (fd-relative walker, identity-checked), and
//! reports the freed bytes. These are S1/S2 items, so this is the permanent delete SAFE-07
//! guards: typing the entry count on a terminal, or `--confirm --allow-permanent` without one
//! (exit 5 otherwise). The selection is computed once, at one instant, and the purge uses the
//! same instant, so it never removes more than was shown. Machine output:
//! `scull/quarantine-purge@1`.

use super::history::parse_operation_id;
use super::{emit_machine, emit_text, exec};
use crate::cancel;
use crate::cli::{CleanArgs, RestoreArgs};
use crate::output::Ui;
use crate::output::bytes::format_bytes;
use crate::output::escape::escape_for_terminal;
use crate::output::json::SchemaId;
use crate::output::style::Style;
use crate::output::table::{Align, Cell, Table};
use schemars::JsonSchema;
use scull_cleaner::{PurgeOutcome, PurgePolicy, RestoreOutcome};
use scull_core::{ErrorKind, FsPath, ItemId, OperationId, ScullError};
use scull_storage::{ItemOutcome, QuarantineEntry};
use serde::Serialize;
use std::path::Path;

/// Schema of `scull restore --json`.
pub const RESTORE_SCHEMA: SchemaId = SchemaId::new("restore", 1);
/// Schema of `scull clean --purge-quarantine --json`.
pub const PURGE_SCHEMA: SchemaId = SchemaId::new("quarantine-purge", 1);

/// Preview or run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Nothing changed.
    Preview,
    /// Executed.
    Run,
}

/// `scull restore` (`scull/restore@1`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreReport {
    /// The operation.
    pub operation_id: OperationId,
    /// Preview or run.
    pub mode: Mode,
    /// The selected quarantine entries.
    pub entries: Vec<RestoreLine>,
    /// Items of the operation scull can't restore (deleted, moved to the Trash).
    pub not_restorable: Vec<NotRestorable>,
    /// Entries restored in this run.
    pub restored: u64,
    /// Entries restored to a sibling path because the original path was taken.
    pub restored_to_sibling: u64,
    /// Entries refused.
    pub refused: u64,
    /// What to run next, if anything.
    pub next_step: Option<String>,
}

/// One quarantine entry of a restore.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RestoreLine {
    /// Manifest entry number.
    pub entry_id: i64,
    /// The plan item.
    pub item_id: ItemId,
    /// Where it goes back to.
    pub original_path: FsPath,
    /// Where it is held.
    pub quarantine_path: FsPath,
    /// Allocated bytes.
    pub bytes: u64,
    /// State before this run: `held`, `restored` or `purged`.
    pub state: String,
    /// What this run did: `restored`, `restored_to_sibling`, `already_restored`, `refused`,
    /// or `none` (preview, or purged: gone for good).
    pub outcome: String,
    /// Where it is now, when restored.
    pub restored_to: Option<FsPath>,
    /// Why, when refused or not restorable.
    pub message: Option<String>,
}

/// An item scull can't restore.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotRestorable {
    /// The item's path.
    pub path: FsPath,
    /// What was done (`removed`, `moved_to_trash`, `tool_run`, ...).
    pub outcome: String,
    /// What to do instead.
    pub hint: String,
}

fn state(e: &QuarantineEntry) -> &'static str {
    if e.restored.is_some() {
        "restored"
    } else if e.purged.is_some() {
        "purged"
    } else {
        "held"
    }
}

fn base_line(e: &QuarantineEntry) -> RestoreLine {
    RestoreLine {
        entry_id: e.entry_id,
        item_id: e.entry.item_id.clone(),
        original_path: e.entry.original_path.clone(),
        quarantine_path: e.entry.quarantine_path.clone(),
        bytes: e.entry.bytes,
        state: state(e).to_owned(),
        outcome: "none".into(),
        restored_to: e.restored.as_ref().map(|(_, p)| p.clone()),
        message: e
            .purged
            .map(|(t, _)| format!("purged {}: gone for good", super::clean::utc_datetime(t))),
    }
}

/// Whether `e` is what `--item w` names: the entry number, item ID or original path.
fn matches(e: &QuarantineEntry, w: &str, cwd: &Path) -> bool {
    let p = if Path::new(w).is_absolute() {
        Path::new(w).to_path_buf()
    } else {
        cwd.join(w)
    };
    e.entry_id.to_string() == w
        || e.entry.item_id.as_str() == w
        || e.entry.original_path.as_str() == w
        || e.entry.original_path.to_path_buf() == p
}

/// Run `scull restore`.
pub fn run_restore(args: &RestoreArgs, ui: &Ui) -> Result<(), ScullError> {
    let op = parse_operation_id(&args.operation_id)?;
    let wants_run = args.confirm || (!args.dry_run && exec::interactive(ui));
    let elevation = exec::elevation();
    if wants_run {
        exec::check_not_elevated(elevation, args.allow_root, "scull restore")?;
    }
    let mut store = exec::open_store()?;
    let Some(rec) = store.operation(&op)? else {
        return Err(ScullError::new(
            ErrorKind::NotFound,
            format!("no operation {op} is recorded; see `scull history`"),
        ));
    };
    let _ = rec;
    let all = store.quarantine_entries(Some(&op))?;
    let cwd = std::env::current_dir().map_err(|e| ScullError::from_io(&e))?;
    for w in &args.items {
        if !all.iter().any(|e| matches(e, w, &cwd)) {
            return Err(ScullError::new(
                ErrorKind::Usage,
                format!(
                    "--item {}: no quarantined item of {op} has this entry number, item ID or \
                     original path",
                    escape_for_terminal(w)
                ),
            ));
        }
    }
    let selected: Vec<&QuarantineEntry> = all
        .iter()
        .filter(|e| args.items.is_empty() || args.items.iter().any(|w| matches(e, w, &cwd)))
        .collect();
    let not_restorable = store
        .audit_items(&op)?
        .into_iter()
        .filter_map(|i| {
            let hint = match i.outcome {
                ItemOutcome::Removed | ItemOutcome::Partial => {
                    "deleted (S0): regenerate it (rebuild or reinstall)"
                }
                ItemOutcome::MovedToTrash => "moved to the Trash: restore it from the Trash",
                ItemOutcome::ToolRun => "cleaned by its tool: packages re-download on next install",
                _ => return None,
            };
            Some(NotRestorable {
                path: i.path,
                outcome: i.outcome.as_str().to_owned(),
                hint: hint.to_owned(),
            })
        })
        .collect();
    let mut report = RestoreReport {
        operation_id: op.clone(),
        mode: Mode::Preview,
        entries: selected.iter().map(|e| base_line(e)).collect(),
        not_restorable,
        restored: 0,
        restored_to_sibling: 0,
        refused: 0,
        next_step: None,
    };
    let held: Vec<&QuarantineEntry> = selected.iter().copied().filter(|e| e.is_held()).collect();
    if held.is_empty() || !wants_run {
        if !held.is_empty() {
            report.next_step = Some(format!(
                "Nothing was changed. To restore {} item(s): scull restore {op} --confirm",
                held.len()
            ));
        }
        return emit_restore(&report, ui);
    }
    if !args.confirm {
        emit_restore(&report, ui)?;
        if !exec::prompt_yes_no(&format!(
            "Restore {} item(s) to where they were (never overwriting)?",
            held.len()
        ))? {
            eprintln!("Nothing was restored.");
            return Err(cancel::cancelled_error());
        }
    }
    let executor = exec::lock_and_recover(&mut store, ui)?;
    let now = scull_engine::now_unix();
    for line in &mut report.entries {
        if line.state != "held" {
            continue;
        }
        if cancel::is_cancelled() {
            break;
        }
        match executor.restore(&mut store, line.entry_id, now) {
            Ok(r) => match r.outcome {
                RestoreOutcome::Restored { to, sibling } => {
                    line.restored_to = Some(FsPath::from_path(&to));
                    report.restored += 1;
                    if sibling {
                        report.restored_to_sibling += 1;
                        line.outcome = "restored_to_sibling".into();
                        line.message = Some(format!(
                            "{} was taken: restored next to it, nothing overwritten",
                            line.original_path.as_str()
                        ));
                    } else {
                        line.outcome = "restored".into();
                    }
                }
                RestoreOutcome::AlreadyRestored { to } => {
                    line.outcome = "already_restored".into();
                    line.restored_to = Some(to);
                }
                #[allow(unreachable_patterns)]
                other => {
                    line.outcome = "restored".into();
                    line.message = Some(format!("{other:?}"));
                }
            },
            Err(e) => {
                line.outcome = "refused".into();
                line.message = Some(format!("{} ({})", e.message, e.kind));
                report.refused += 1;
            }
        }
    }
    report.mode = Mode::Run;
    emit_restore(&report, ui)?;
    if cancel::is_cancelled() {
        return Err(cancel::cancelled_error());
    }
    if report.refused > 0 {
        return Err(ScullError::new(
            ErrorKind::PartialFailure,
            format!(
                "{} item(s) could not be restored; each is listed with the reason (nothing was \
                 moved for them)",
                report.refused
            ),
        ));
    }
    Ok(())
}

fn emit_restore(r: &RestoreReport, ui: &Ui) -> Result<(), ScullError> {
    if ui.format.is_machine() {
        emit_machine(ui.format, RESTORE_SCHEMA, "restore", r)
    } else {
        emit_text(&render_restore(r, ui.color_out))
    }
}

fn render_restore(r: &RestoreReport, color: bool) -> String {
    let mut out = format!(
        "{}\n",
        Style::Bold.paint(
            &match r.mode {
                Mode::Preview => format!(
                    "Restore from operation {} (nothing changed yet)",
                    r.operation_id
                ),
                Mode::Run => format!("Restore from operation {}", r.operation_id),
            },
            color
        )
    );
    if r.entries.is_empty() {
        out.push_str("This operation quarantined nothing.\n");
    } else {
        let mut t = Table::new([
            ("Entry", Align::Right),
            ("State", Align::Left),
            ("Result", Align::Left),
            ("Size", Align::Right),
            ("Original path", Align::Left),
            ("Note", Align::Left),
        ]);
        for e in &r.entries {
            let note = match (&e.restored_to, &e.message) {
                (_, Some(m)) if e.outcome != "restored_to_sibling" => m.clone(),
                (Some(to), _) => format!("now at {}", to.as_str()),
                (None, Some(m)) => m.clone(),
                (None, None) if e.state == "held" => {
                    format!("held at {}", e.quarantine_path.as_str())
                }
                _ => String::new(),
            };
            let style = if e.outcome == "refused" {
                Style::Warn
            } else {
                Style::Plain
            };
            t.push(vec![
                Cell::text(&e.entry_id.to_string()),
                Cell::text(&e.state),
                Cell::text(&e.outcome.replace('_', " ")).styled(style),
                Cell::bytes(e.bytes),
                Cell::path(e.original_path.as_str()),
                Cell::text(&note),
            ]);
        }
        out.push_str(&t.render(color));
    }
    if r.mode == Mode::Run {
        out.push_str(&format!(
            "\n{} restored ({} next to a path that was taken; nothing was overwritten), {} refused.\n",
            r.restored, r.restored_to_sibling, r.refused
        ));
    }
    if !r.not_restorable.is_empty() {
        out.push_str(&format!(
            "\n{}\n",
            Style::Bold.paint("Not restorable by scull:", color)
        ));
        for n in &r.not_restorable {
            out.push_str(&format!(
                "  {}: {}\n",
                escape_for_terminal(n.path.as_str()),
                escape_for_terminal(&n.hint)
            ));
        }
    }
    if let Some(n) = &r.next_step {
        out.push_str(&format!("\n{}\n", escape_for_terminal(n)));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Quarantine purge
// ---------------------------------------------------------------------------------------------

/// `scull clean --purge-quarantine` (`scull/quarantine-purge@1`).
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PurgeReport {
    /// Preview or run.
    pub mode: Mode,
    /// The instant the selection was made (Unix seconds).
    pub at: i64,
    /// `--older-than`, in seconds.
    pub older_than_secs: Option<u64>,
    /// The selected entries.
    pub entries: Vec<PurgeLine>,
    /// Bytes held by the selection.
    pub selected_bytes: u64,
    /// Bytes freed by this run.
    pub freed_bytes: u64,
    /// Entries kept by this run (changed or not removable).
    pub kept: u64,
    /// What to run next, if anything.
    pub next_step: Option<String>,
}

/// One selected quarantine entry.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PurgeLine {
    /// Manifest entry number.
    pub entry_id: i64,
    /// Its operation.
    pub operation_id: OperationId,
    /// Where it came from.
    pub original_path: FsPath,
    /// Allocated bytes held.
    pub bytes: u64,
    /// When it was quarantined.
    pub quarantined_at: i64,
    /// When its retention ends.
    pub expires_at: Option<i64>,
    /// `none` (preview), `purged`, `already_gone` or `kept`.
    pub outcome: String,
    /// Bytes freed.
    pub freed_bytes: u64,
    /// Why it was kept.
    pub reason: Option<String>,
}

/// Run `scull clean --purge-quarantine`.
pub fn run_purge(args: &CleanArgs, ui: &Ui) -> Result<(), ScullError> {
    let wants_run = args.confirm || (!args.dry_run && exec::interactive(ui));
    if wants_run {
        exec::check_not_elevated(
            exec::elevation(),
            args.allow_root,
            "scull clean --purge-quarantine",
        )?;
    }
    let mut store = exec::open_store()?;
    let now = scull_engine::now_unix();
    let policy = PurgePolicy {
        now,
        expired: true,
        // `--include-unexpired`: everything quarantined up to this instant.
        older_than: if args.include_unexpired {
            Some(now.saturating_add(1))
        } else {
            args.older_than
                .map(|s| now.saturating_sub(i64::try_from(s).unwrap_or(i64::MAX)))
        },
        size_cap: None,
    };
    let selected: Vec<QuarantineEntry> = store
        .quarantine_held()?
        .into_iter()
        .filter(|e| {
            (policy.expired && e.entry.expires_at.is_some_and(|x| x <= policy.now))
                || policy
                    .older_than
                    .is_some_and(|t| e.entry.quarantined_at < t)
        })
        .collect();
    let mut report = PurgeReport {
        mode: Mode::Preview,
        at: now,
        older_than_secs: args.older_than,
        entries: selected
            .iter()
            .map(|e| PurgeLine {
                entry_id: e.entry_id,
                operation_id: e.entry.operation_id.clone(),
                original_path: e.entry.original_path.clone(),
                bytes: e.entry.bytes,
                quarantined_at: e.entry.quarantined_at,
                expires_at: e.entry.expires_at,
                outcome: "none".into(),
                freed_bytes: 0,
                reason: None,
            })
            .collect(),
        selected_bytes: selected.iter().map(|e| e.entry.bytes).sum(),
        freed_bytes: 0,
        kept: 0,
        next_step: None,
    };
    let n = selected.len() as u64;
    if n == 0 || !wants_run {
        if n > 0 {
            report.next_step = Some(format!(
                "Nothing was deleted. To delete these {n} item(s) for good: type {n} at the \
                 prompt on a terminal, or pass --confirm --allow-permanent"
            ));
        }
        return emit_purge(&report, ui);
    }
    // SAFE-07: permanently deleting quarantined S1/S2 items takes the elevated confirmation.
    if args.allow_permanent {
        // `--allow-permanent` requires `--confirm`: the flag is the elevated confirmation.
    } else if exec::interactive(ui) {
        emit_purge(&report, ui)?;
        let expected = n.to_string();
        if !exec::prompt_typed(
            &format!(
                "This permanently deletes {n} quarantined item(s) ({}); they can't be restored \
                 afterwards. Type {expected} to confirm:",
                format_bytes(report.selected_bytes)
            ),
            &expected,
        )? {
            eprintln!("Nothing was deleted.");
            return Err(cancel::cancelled_error());
        }
    } else {
        return Err(ScullError::new(
            ErrorKind::SafetyBlocked,
            format!(
                "purging the quarantine permanently deletes {n} item(s) (S1/S2, not undoable): \
                 pass {} with --confirm to do it without a terminal (SAFE-07). Nothing was \
                 deleted.",
                exec::ALLOW_PERMANENT_FLAG
            ),
        ));
    }
    let executor = exec::lock_and_recover(&mut store, ui)?;
    let done = executor.purge_quarantine(&mut store, &policy)?;
    for (entry, outcome) in &done.entries {
        let Some(line) = report
            .entries
            .iter_mut()
            .find(|l| l.entry_id == entry.entry_id)
        else {
            continue;
        };
        match outcome {
            PurgeOutcome::Purged { freed_bytes } => {
                line.outcome = "purged".into();
                line.freed_bytes = *freed_bytes;
            }
            PurgeOutcome::AlreadyGone => line.outcome = "already_gone".into(),
            PurgeOutcome::Kept { reason } => {
                line.outcome = "kept".into();
                line.reason = Some(reason.clone());
                report.kept += 1;
            }
            #[allow(unreachable_patterns)]
            other => {
                line.outcome = "kept".into();
                line.reason = Some(format!("{other:?}"));
                report.kept += 1;
            }
        }
    }
    report.freed_bytes = done.freed_bytes;
    report.mode = Mode::Run;
    emit_purge(&report, ui)?;
    if report.kept > 0 {
        return Err(ScullError::new(
            ErrorKind::PartialFailure,
            format!(
                "{} quarantined item(s) were kept; each is listed with the reason",
                report.kept
            ),
        ));
    }
    Ok(())
}

fn emit_purge(r: &PurgeReport, ui: &Ui) -> Result<(), ScullError> {
    if ui.format.is_machine() {
        emit_machine(ui.format, PURGE_SCHEMA, "purge", r)
    } else {
        emit_text(&render_purge(r, ui.color_out))
    }
}

fn render_purge(r: &PurgeReport, color: bool) -> String {
    let mut out = format!(
        "{}\n",
        Style::Bold.paint(
            match r.mode {
                Mode::Preview => "Quarantine purge (nothing deleted yet)",
                Mode::Run => "Quarantine purge",
            },
            color
        )
    );
    if r.entries.is_empty() {
        out.push_str(
            "Nothing to purge: no quarantined item has reached the end of its retention.\n",
        );
        return out;
    }
    let mut t = Table::new([
        ("Entry", Align::Right),
        ("Operation", Align::Left),
        ("Held", Align::Right),
        ("Retention ends", Align::Left),
        ("Result", Align::Left),
        ("Original path", Align::Left),
    ]);
    for e in &r.entries {
        t.push(vec![
            Cell::text(&e.entry_id.to_string()),
            Cell::text(e.operation_id.as_str()),
            Cell::bytes(e.bytes),
            Cell::text(
                &e.expires_at
                    .map_or("-".to_owned(), super::clean::utc_datetime),
            ),
            Cell::text(&match &e.reason {
                Some(why) => format!("{} ({why})", e.outcome),
                None => e.outcome.replace('_', " "),
            }),
            Cell::path(e.original_path.as_str()),
        ]);
    }
    out.push_str(&t.render(color));
    match r.mode {
        Mode::Preview => out.push_str(&format!(
            "\n{} item(s), {} held. {}\n",
            r.entries.len(),
            format_bytes(r.selected_bytes),
            Style::Warn.paint(
                "Purging is permanent: purged items can't be restored.",
                color
            )
        )),
        Mode::Run => out.push_str(&format!(
            "\nFreed {} ({} kept).\n",
            format_bytes(r.freed_bytes),
            r.kept
        )),
    }
    if let Some(n) = &r.next_step {
        out.push_str(&format!("{}\n", escape_for_terminal(n)));
    }
    out
}
