//! The Executor: the only code path in scull that deletes anything (SAFE-10).
//!
//! See `docs/PRD.md` §10 and `HANDOVER.md` for responsibilities and boundaries.
//!
//! * [`Executor`]: takes one already verified and narrowed plan item
//!   ([`scull_core::narrow`]) and deletes it through the platform's handle-relative, no-follow
//!   removal walker (`scull_platform::remove`, SAFE-04, SAFE-05, SAFE-18): `openat`/`unlinkat`
//!   on Unix (P2-EXEC-01), `NtCreateFile` relative opens and handle-based deletion on Windows
//!   (P2-EXEC-02). This crate is the only one that enables `scull-platform`'s `delete`
//!   feature.
//! * [`Executor::execute_plan`]: verifies a plan, then for **each** item, immediately before
//!   acting on it, revalidates it against the disk ([`Revalidation`], SAFE-05): a bounded,
//!   fresh re-derivation of the item's parent directory with the read-only engine (Git
//!   tracking, a fresh in-use probe, activity, protection), [`scull_core::narrow`], and a
//!   no-follow identity and kind check. Items that fail are skipped and reported with a reason
//!   ([`Skip`]), never executed "best effort".
//! * [`Executor::acquire`] / [`ExecutionLock`]: the single execution lock per user account
//!   (SAFE-09), an OS file lock held for the Executor's whole lifetime; a second executor fails
//!   with `LockHeld` (exit 6) or waits up to a timeout. Plans run only under the lock.
//! * [`Executor::begin_operation`] / [`Operation`]: one executed operation. It refuses a plan
//!   whose targets another operation touched since the plan was created (SAFE-09, exit 4),
//!   records the operation and its target identities, audits every item outcome (SAFE-08) and
//!   owns the operation's quarantine folders.
//! * Write-ahead journal and crash recovery (SAFE-19, [`journal`], [`recover`]): within an
//!   operation every mutating step is journaled (fsynced) before the disk is touched and closed
//!   after it; [`Executor::recover`] reconciles what a crash left open with the disk on the
//!   next start, never resuming a deletion or a move, and closes the operation as interrupted.
//! * Quarantine (D-3, SAFE-16): S1/S2 items move into `.scull-quarantine/<operation-id>/` on
//!   their own volume by one atomic no-replace rename, recorded in the database manifest of
//!   record. Its location is the configured base ([`QuarantineSettings::base_for`],
//!   `[quarantine]` in the user config, P2-EXEC-13) when that is on the item's volume, else
//!   chosen automatically; under the test harness (`SCULL_TEST_SANDBOX`) nothing outside the
//!   sandbox is used and the automatic choice is refused. Only where quarantine is impossible
//!   items fall back to the Trash
//!   ([`Operation::with_trash`], P2-EXEC-07), and where that is impossible too they are
//!   blocked, never deleted. [`Executor::purge_quarantine`] removes expired entries and records the freed bytes.
//!   [`Executor::restore`] moves an entry back to its recorded path only (the database manifest
//!   is authoritative), after verifying its identity, kind and size and that the original
//!   folder is the same directory object; it never overwrites (a taken path gets a
//!   `<name>.scull-restored-<n>` sibling), is idempotent, and logs every attempt (T-32).
//!
//! * Measured free-space delta (PHY-02, [`measure`]): every operation measures its volumes
//!   around `execute_plan` ([`Operation::with_meter`]); the accuracy report is
//!   [`ExecutionReport::measurement`] and [`Operation::finish`] records the measured delta next
//!   to the predicted bytes (I-5).
//!
//! * Orphaned package caches (D-36, provisional, [`orphaned`]): an S1 package cache whose tool
//!   is not installed may be removed through the walker by
//!   [`Executor::delete_orphaned_cache`], only with [`OrphanedCacheEvidence`] that Quick Clean
//!   gathers right before ([`quickclean::QuickClean::remove_orphaned`]) and a per-tool
//!   confirmation. `Action::allowed_for` is unchanged: no other path deletes at S1.
//!
//! * Quick Clean is audited like any operation (SAFE-08, P2-EXEC-12): the confirmed preview is
//!   recorded as a plan ([`quickclean::QuickCleanPlan::audit_plan`]: resolved binary and exact
//!   arguments per tool) and [`quickclean::QuickClean::run`] /
//!   [`quickclean::QuickClean::remove_orphaned`] run inside its [`Operation`], one audit row per
//!   tool run or orphaned removal; orphaned removals are journaled first (SAFE-19).
//!
//! # Interfaces
//!
//! The CLI is wired to the Executor on the `phase2` branch: `scull clean --plan FILE
//! --confirm` (executes through [`Executor::execute_plan`] in an [`Operation`]: lock, journal,
//! audit, revalidation, quarantine, measured delta), `scull clean caches` (Quick Clean in one audited operation,
//! and [`quickclean::QuickClean::remove_orphaned`] for D-36), `scull restore` and
//! `scull clean --purge-quarantine`. The CLI owns the user-facing safeguards around it: the
//! refusal to run elevated without `--allow-root` (SAFE-14) and the elevated confirmation
//! (typed count, or `--allow-permanent`) for S1/S2 items that can't be undone (SAFE-07),
//! P2-EXEC-11 / #77. The Trash fallback (P2-EXEC-07, #73) and the measured free-space delta
//! (P2-EXEC-10, #76) are in this crate.
//!
//! Agent plans (P4-MCP-01): `scull mcp serve --allow-destructive` executes plans an AI agent
//! prepared through the same path, only after the user approved them out of band
//! (`scull approve`); [`Executor::begin_operation`] consumes that approval atomically and
//! refuses an MCP plan without it, and [`Executor::execute_plan`] refuses an MCP plan outside
//! that operation (MCP-04). The desktop app does not call the Executor yet. The inspect-only release
//! gate (`packaging/check_inspect_only.py`) applies to `main`, where this wiring does not exist;
//! on `phase2` it is expected to fail.
//!
//! `scull clean` passes the configured quarantine location and enables the Trash fallback
//! ([`Operation::with_trash`] with the user's Trash; under the test harness, a Trash inside the
//! sandbox).
//!
//! Still open:
//!
//! * the Windows quarantine/restore moves (#71/#72) and the CLI tests that execute them are
//!   implemented but have not run on Windows CI yet.
mod execution;
mod executor;
pub mod journal;
mod lock;
pub mod measure;
mod operation;
pub mod orphaned;
pub mod own_data;
mod quarantine;
pub mod quickclean;
pub mod recover;
mod restore;
mod revalidate;
pub mod trash;

pub use execution::{ExecutionReport, ItemOutcome, ItemReport, PlanContext};
pub use executor::{DeleteReport, DeleteTarget, Executor};
pub use journal::{StepHook, StepPoint};
pub use lock::{ExecutionLock, LockOptions};
pub use operation::Operation;
pub use orphaned::OrphanedCacheEvidence;
pub use quarantine::{
    DEFAULT_RETENTION_SECS, DISK_MANIFEST, PurgeOutcome, PurgePolicy, PurgeReport,
    QuarantineSettings, Quarantined,
};
pub use recover::{RecoveredItem, RecoveredOperation, Recovery, RecoveryReport};
pub use restore::{RESTORED_SUFFIX, RestoreOutcome, RestoreReport};
pub use revalidate::{Revalidated, Revalidation, Skip};
pub use scull_platform::quarantine::QUARANTINE_DIR_NAME;
pub use scull_platform::remove::{
    AbortReason, Anomaly, RemoveOutcome, RemoveReport, RemovedCounts, SkipReason, SkippedEntry,
    VERIFY_EVERY, WalkEvent,
};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "scull-cleaner");
    }
}
