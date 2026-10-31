//! The Executor: the only code path in scull that deletes anything (SAFE-10).
//!
//! See `docs/PRD.md` §10 and `HANDOVER.md` for responsibilities and boundaries.
//!
//! * [`Executor`]: takes one already verified and narrowed plan item
//!   ([`scull_core::narrow`]) and deletes it through the platform's fd-relative, no-follow
//!   removal walker (`scull_platform::remove`, SAFE-04, SAFE-05, SAFE-18). This crate is the
//!   only one that enables `scull-platform`'s `delete` feature.
//! * [`Executor::execute_plan`]: verifies a plan, then for **each** item, immediately before
//!   acting on it, revalidates it against the disk ([`Revalidation`], SAFE-05): a bounded,
//!   fresh re-derivation of the item's parent directory with the read-only engine (Git
//!   tracking, a fresh in-use probe, activity, protection), [`scull_core::narrow`], and a
//!   no-follow identity and kind check. Items that fail are skipped and reported with a reason
//!   ([`Skip`]), never executed "best effort".
//! * [`Executor::acquire`] / [`ExecutionLock`]: the single execution lock per user account
//!   (SAFE-09), an OS file lock held for the Executor's whole lifetime; a second executor fails
//!   with `LockHeld` (exit 6) or waits up to a timeout. Plans run only under the lock.
//!
//! # Not wired into any interface yet
//!
//! **Do not call the Executor from the CLI, desktop app or MCP server** until the remaining
//! Phase 2 safeguards land; the inspect-only release gate checks that `scull clean` is still a
//! stub. Missing today, each tracked by its issue:
//!
//! * write-ahead journal and crash recovery (SAFE-19): P2-EXEC-08, #74;
//! * append-only audit log (SAFE-08): P2-EXEC-09, #75;
//! * measured free-space delta (PHY-02): P2-EXEC-10, #76;
//! * refusal to run elevated and type-to-confirm for permanent deletes (SAFE-14, SAFE-07):
//!   P2-EXEC-11, #77;
//! * quarantine, Trash and tool-native actions: P2-EXEC-05 #71, P2-EXEC-07 #73, P2-QC-01 #78;
//! * the Windows walker: P2-EXEC-02, #68.

mod execution;
mod executor;
mod lock;
mod revalidate;

pub use execution::{ExecutionReport, ItemOutcome, ItemReport, PlanContext};
pub use executor::{DeleteReport, DeleteTarget, Executor};
pub use lock::{ExecutionLock, LockOptions};
pub use revalidate::{Revalidated, Revalidation, Skip};
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
