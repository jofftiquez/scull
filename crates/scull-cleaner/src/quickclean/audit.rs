//! The audit of a Quick Clean (SAFE-08, SAFE-19, P2-EXEC-12): what is recorded for each tool
//! run and each orphaned-cache removal. See the module docs of [`super`], section "Audit".

use super::{CacheSize, Ended, SkipReason, ToolResult, escape_line};
use crate::measure::ItemExecution;
use scull_core::{Action, ItemId, PlanItem};
use scull_detectors::caches::CacheProblem;
use scull_platform::remove::{RemoveOutcome, RemoveReport};
use scull_storage::{ItemOutcome, ItemRecord, WalkEnd, WalkSummary};
use std::ffi::OsStr;
use std::path::Path;

/// Longest binary path or argument recorded in the audit plan, in characters (after escaping).
pub const MAX_AUDIT_ARG_CHARS: usize = 1024;

/// Most arguments recorded in the audit plan; the rest are summarized in one last entry.
pub const MAX_AUDIT_ARGS: usize = 64;

/// Marker appended to a value cut at [`MAX_AUDIT_ARG_CHARS`].
const CUT: &str = "...[truncated]";

/// `s` escaped ([`escape_line`]: control, bidi and invisible characters made visible; invalid
/// Unicode replaced) and capped at [`MAX_AUDIT_ARG_CHARS`] characters.
pub(crate) fn audit_text(s: &OsStr) -> String {
    let escaped = escape_line(&s.to_string_lossy());
    if escaped.chars().count() <= MAX_AUDIT_ARG_CHARS {
        return escaped;
    }
    let keep = MAX_AUDIT_ARG_CHARS - CUT.len();
    let mut out: String = escaped.chars().take(keep).collect();
    out.push_str(CUT);
    out
}

/// The action recorded for a planned tool: the resolved binary and the exact arguments, both
/// escaped and capped ([`audit_text`], [`MAX_AUDIT_ARGS`]). Display and audit only: what runs
/// is the planned binary and arguments themselves.
pub(crate) fn audited_action(binary: &Path, args: &[std::ffi::OsString]) -> Action {
    let mut recorded: Vec<String> = args
        .iter()
        .take(MAX_AUDIT_ARGS)
        .map(|a| audit_text(a))
        .collect();
    if args.len() > MAX_AUDIT_ARGS {
        recorded.pop();
        recorded.push(format!(
            "[+{} more arguments]",
            args.len() - MAX_AUDIT_ARGS + 1
        ));
    }
    Action::ToolCommand {
        tool: audit_text(binary.as_os_str()),
        args: recorded,
    }
}

/// The plan item recorded for a tool: the candidate's, with its action resolved to the exact
/// command ([`audited_action`]).
pub(crate) fn tool_item(
    mut candidate_item: PlanItem,
    binary: &Path,
    args: &[std::ffi::OsString],
) -> PlanItem {
    candidate_item.action = audited_action(binary, args);
    candidate_item
}

impl SkipReason {
    /// Stable reason code (`^[a-z0-9][a-z0-9_.:-]{0,63}$`), for the audit log.
    pub fn code(&self) -> &'static str {
        match self {
            SkipReason::Refused(_) => "location_refused",
            SkipReason::Unticked => "unticked",
            SkipReason::Problem(CacheProblem::Link { .. }) => "link",
            SkipReason::Problem(CacheProblem::OtherVolume { .. }) => "other_volume",
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
            SkipReason::Sandbox(_) => "sandbox_unavailable",
            SkipReason::NotApproved => "not_approved",
            SkipReason::NotOrphanRemovable(_) => "d36_excluded",
            SkipReason::InspectOnly => "inspect_only",
        }
    }

    /// The audit record of a tool or removal not started for this reason: `refused` for the
    /// elevation check (SAFE-14) and for an inspect-only tool in a plan (only a forged or stale
    /// plan gets that far), `failed` when the sandbox could not be created, `skipped`
    /// otherwise. Nothing was touched in any case.
    pub(crate) fn audit_record(&self, item_id: ItemId, at: i64) -> ItemRecord {
        let outcome = match self {
            SkipReason::Elevated(_) | SkipReason::InspectOnly => ItemOutcome::Refused,
            SkipReason::Sandbox(_) => ItemOutcome::Failed,
            _ => ItemOutcome::Skipped,
        };
        ItemRecord::new(item_id, at, outcome).with_reason(self.code())
    }
}

/// Whether two measurements show a cache provably unchanged in size.
fn same_size(before: Option<CacheSize>, after: Option<CacheSize>) -> bool {
    matches!((before, after), (Some(b), Some(a)) if b.complete && a.complete && a == b)
}

/// The audit record of a tool that was started, and what it did for the measured-delta tally.
///
/// * exit status 0: `tool_run`, with the bytes the cache shrank by as `freed_bytes` (measured;
///   the plan holds the prediction);
/// * otherwise, reason `tool_exit:<code>`, `tool_killed`, `tool_timeout`, `cancelled` or
///   `spawn_failed`: `failed` when the cache is provably unchanged (same complete size before
///   and after, or the process never started), else `partial` (the tool may have removed part
///   of it).
pub(crate) fn tool_record(item_id: ItemId, r: &ToolResult, at: i64) -> (ItemRecord, ItemExecution) {
    let freed = r
        .cache_bytes_freed()
        .map(|b| u64::try_from(b.max(0)).unwrap_or(u64::MAX));
    if r.succeeded() {
        let mut rec = ItemRecord::new(item_id, at, ItemOutcome::ToolRun);
        rec.freed_bytes = freed;
        let removed = freed.unwrap_or(0);
        return (rec, ItemExecution::Done { removed, moved: 0 });
    }
    let reason = match &r.ended {
        Ended::Exited(Some(code)) => format!("tool_exit:{code}"),
        Ended::Exited(None) => "tool_killed".to_owned(),
        Ended::TimedOut(_) => "tool_timeout".to_owned(),
        Ended::Cancelled => "cancelled".to_owned(),
        Ended::SpawnFailed(_) => "spawn_failed".to_owned(),
    };
    let untouched =
        matches!(r.ended, Ended::SpawnFailed(_)) || same_size(r.size_before, r.size_after);
    if untouched {
        let rec = ItemRecord::new(item_id, at, ItemOutcome::Failed).with_reason(reason);
        return (rec, ItemExecution::NotExecuted);
    }
    let mut rec = ItemRecord::new(item_id, at, ItemOutcome::Partial).with_reason(reason);
    rec.freed_bytes = freed;
    let removed = freed.unwrap_or(0);
    (rec, ItemExecution::Partial { removed, moved: 0 })
}

/// The D-36 evidence summary recorded as the reason of a complete orphaned-cache removal: no
/// trusted binary in any of the `searched` trusted directories (the other checks, validated
/// cache root, no link, same volume, not in use, are what the code itself stands for).
pub(crate) fn orphaned_reason(searched: usize) -> String {
    format!("d36_orphaned:searched_{searched}_dirs")
}

/// The audit record of an orphaned-cache removal walk, and what it did for the tally.
pub(crate) fn removal_record(
    item_id: ItemId,
    r: &RemoveReport,
    searched: usize,
    at: i64,
) -> (ItemRecord, ItemExecution) {
    let removed = r.removed.allocated_bytes;
    let end = match r.outcome {
        RemoveOutcome::Complete => WalkEnd::Complete,
        RemoveOutcome::Incomplete => WalkEnd::Incomplete,
        _ => WalkEnd::Aborted,
    };
    let rec = ItemRecord::from_walk(
        item_id,
        at,
        &WalkSummary {
            end,
            entries_removed: r.removed.files
                + r.removed.dirs
                + r.removed.symlinks
                + r.removed.special,
            freed_bytes: removed,
            entries_kept: r.skipped_total,
            reason: (end == WalkEnd::Complete).then(|| orphaned_reason(searched)),
        },
    );
    let done = match rec.outcome {
        ItemOutcome::Removed => ItemExecution::Done { removed, moved: 0 },
        ItemOutcome::Partial => ItemExecution::Partial { removed, moved: 0 },
        _ => ItemExecution::NotExecuted,
    };
    (rec, done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::Duration;

    fn code_ok(c: &str) -> bool {
        let mut chars = c.chars();
        c.len() <= 64
            && chars
                .next()
                .is_some_and(|f| f.is_ascii_lowercase() || f.is_ascii_digit())
            && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || "_.:-".contains(ch))
    }

    #[test]
    fn audited_action_escapes_and_caps() {
        let long = "a".repeat(MAX_AUDIT_ARG_CHARS + 10);
        let mut args: Vec<OsString> = vec!["cache".into(), "x\u{1b}[2J\u{202e}y".into()];
        args.push(long.clone().into());
        let Action::ToolCommand { tool, args: rec } =
            audited_action(Path::new("/opt/bin/npm\u{7}"), &args)
        else {
            panic!("tool command");
        };
        assert_eq!(tool, "/opt/bin/npm\\x07");
        assert_eq!(rec[0], "cache");
        assert_eq!(rec[1], "x\\x1b[2J\\u{202e}y");
        assert_eq!(rec[2].chars().count(), MAX_AUDIT_ARG_CHARS);
        assert!(rec[2].ends_with(CUT));

        let many: Vec<OsString> = (0..MAX_AUDIT_ARGS + 5)
            .map(|i| i.to_string().into())
            .collect();
        let Action::ToolCommand { args: rec, .. } = audited_action(Path::new("/b"), &many) else {
            panic!("tool command");
        };
        assert_eq!(rec.len(), MAX_AUDIT_ARGS);
        assert_eq!(rec[MAX_AUDIT_ARGS - 2], (MAX_AUDIT_ARGS - 2).to_string());
        assert_eq!(rec[MAX_AUDIT_ARGS - 1], "[+6 more arguments]");
    }

    #[test]
    fn every_reason_is_a_stable_code() {
        let reasons = [
            SkipReason::Refused("x".into()),
            SkipReason::Unticked,
            SkipReason::Problem(CacheProblem::Link {
                path: PathBuf::from("/l"),
            }),
            SkipReason::Problem(CacheProblem::OtherVolume {
                anchor: PathBuf::from("/a"),
            }),
            SkipReason::VolumeUnverified,
            SkipReason::Unrecognized,
            SkipReason::NotClassified,
            SkipReason::NotEligible {
                tier: scull_core::Tier::S2,
            },
            SkipReason::UnexpectedLayout("x".into()),
            SkipReason::InUse(vec!["npm (pid 1)".into()]),
            SkipReason::ActivityUnknown(vec![]),
            SkipReason::NotInstalled {
                manual_removal: PathBuf::from("/c"),
            },
            SkipReason::ShellShimOnly(PathBuf::from("/npm.cmd")),
            SkipReason::WrongVersion {
                reported: "1".into(),
            },
            SkipReason::Changed("x".into()),
            SkipReason::Elevated(scull_platform::Elevation::Elevated),
            SkipReason::Cancelled,
            SkipReason::Sandbox("x".into()),
            SkipReason::NotApproved,
        ];
        for r in &reasons {
            assert!(code_ok(r.code()), "{r:?}: {}", r.code());
        }
        for code in [
            "tool_exit:-1073741819",
            "tool_exit:1",
            &orphaned_reason(usize::MAX),
        ] {
            assert!(code_ok(code), "{code}");
        }
    }

    fn result(ended: Ended, before: u64, after: u64) -> ToolResult {
        let size = |allocated| {
            Some(CacheSize {
                allocated,
                logical: allocated,
                shared_allocated: 0,
                complete: true,
            })
        };
        ToolResult {
            tool: scull_detectors::caches::CacheTool::Npm,
            cache: PathBuf::from("/c"),
            binary: PathBuf::from("/bin/npm"),
            args: Vec::new(),
            mode: super::super::CleanMode::Prune,
            ended,
            stdout: super::super::CapturedOutput::default(),
            stderr: super::super::CapturedOutput::default(),
            elapsed: Duration::ZERO,
            free_before: Some(0),
            free_after: Some(0),
            size_before: size(before),
            size_after: size(after),
            explanation: None,
        }
    }

    #[test]
    fn tool_outcomes_map_to_audit_outcomes() {
        let id = ItemId::derive(scull_core::FileIdentity::new(1, 2), b"/c");
        let (rec, done) = tool_record(id.clone(), &result(Ended::Exited(Some(0)), 100, 40), 1);
        assert_eq!(rec.outcome, ItemOutcome::ToolRun);
        assert_eq!(rec.freed_bytes, Some(60));
        assert_eq!(
            done,
            ItemExecution::Done {
                removed: 60,
                moved: 0
            }
        );
        // A tool that grew the cache freed nothing.
        let (rec, _) = tool_record(id.clone(), &result(Ended::Exited(Some(0)), 40, 100), 1);
        assert_eq!(rec.freed_bytes, Some(0));

        let (rec, done) = tool_record(id.clone(), &result(Ended::Exited(Some(1)), 100, 100), 1);
        assert_eq!(rec.outcome, ItemOutcome::Failed);
        assert_eq!(rec.reason.as_deref(), Some("tool_exit:1"));
        assert_eq!(rec.freed_bytes, None);
        assert_eq!(done, ItemExecution::NotExecuted);

        let (rec, done) = tool_record(
            id.clone(),
            &result(Ended::TimedOut(Duration::from_secs(1)), 100, 70),
            1,
        );
        assert_eq!(rec.outcome, ItemOutcome::Partial);
        assert_eq!(rec.reason.as_deref(), Some("tool_timeout"));
        assert_eq!(
            done,
            ItemExecution::Partial {
                removed: 30,
                moved: 0
            }
        );

        let mut unmeasured = result(Ended::Exited(None), 100, 100);
        unmeasured.size_after = None;
        let (rec, _) = tool_record(id.clone(), &unmeasured, 1);
        assert_eq!(
            rec.outcome,
            ItemOutcome::Partial,
            "unknown is not unchanged"
        );
        assert_eq!(rec.reason.as_deref(), Some("tool_killed"));

        let (rec, _) = tool_record(id, &result(Ended::SpawnFailed("x".into()), 1, 0), 1);
        assert_eq!(rec.outcome, ItemOutcome::Failed);
        assert_eq!(rec.reason.as_deref(), Some("spawn_failed"));
    }
}
