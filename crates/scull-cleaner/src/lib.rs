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
//!   record; where that is impossible they are blocked, never deleted.
//!   [`Executor::purge_quarantine`] removes expired entries and records the freed bytes.
//!   [`Executor::restore`] moves an entry back to its recorded path only (the database manifest
//!   is authoritative), after verifying its identity, kind and size and that the original
//!   folder is the same directory object; it never overwrites (a taken path gets a
//!   `<name>.scull-restored-<n>` sibling), is idempotent, and logs every attempt (T-32).
//!
//! # Not wired into any interface yet
//!
//! **Do not call the Executor from the CLI, desktop app or MCP server** until the remaining
//! Phase 2 safeguards land; the inspect-only release gate checks that `scull clean` is still a
//! stub. Missing today, each tracked by its issue:
//!
//! * measured free-space delta (PHY-02): P2-EXEC-10, #76;
//! * refusal to run elevated and type-to-confirm for permanent deletes (SAFE-14, SAFE-07):
//!   P2-EXEC-11, #77;
//! * the Trash fallback: P2-EXEC-07, #73;
//! * Windows quarantine moves and restore (handle-based rename, with the Windows walker's
//!   helpers).

mod execution;
mod executor;
pub mod journal;
mod lock;
pub mod measure;
mod operation;
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
