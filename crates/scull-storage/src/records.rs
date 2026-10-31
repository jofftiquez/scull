//! Typed records accepted and returned by [`crate::Store`]. All timestamps are Unix seconds
//! supplied by the caller, so behavior is deterministic in tests.

use scull_core::{Category, ContentHash, FsPath, OperationId, PlanId, ScanId, Tier};

/// Start of a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanStart {
    /// Scan identifier.
    pub scan_id: ScanId,
    /// When the scan started.
    pub started_at: i64,
    /// Scan roots.
    pub roots: Vec<FsPath>,
}

/// How a scan ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanStatus {
    /// Finished without errors.
    Completed,
    /// Finished, but some entries could not be read.
    Partial,
    /// Aborted by an error.
    Failed,
    /// Cancelled by the user.
    Cancelled,
}

impl ScanStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ScanStatus::Completed => "completed",
            ScanStatus::Partial => "partial",
            ScanStatus::Failed => "failed",
            ScanStatus::Cancelled => "cancelled",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        [
            ScanStatus::Completed,
            ScanStatus::Partial,
            ScanStatus::Failed,
            ScanStatus::Cancelled,
        ]
        .into_iter()
        .find(|v| v.as_str() == s)
    }
}

/// Totals of a finished scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanTotals {
    /// Filesystem entries visited.
    pub entries_scanned: u64,
    /// Candidates detected.
    pub candidates: u64,
    /// Logical bytes of all candidates.
    pub logical_bytes: u64,
    /// Allocated bytes of all candidates.
    pub allocated_bytes: u64,
    /// Reclaimable bytes of all candidates.
    pub reclaimable_bytes: u64,
    /// Entries that could not be read.
    pub errors: u64,
}

/// One byte rollup row of a scan snapshot (no per-file rows are ever stored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollup {
    /// Candidate category.
    pub category: Category,
    /// Risk tier.
    pub tier: Tier,
    /// Ecosystem (e.g. `node`, `rust`), if known.
    pub ecosystem: Option<String>,
    /// Number of candidates in this bucket.
    pub items: u64,
    /// Logical bytes.
    pub logical_bytes: u64,
    /// Allocated bytes.
    pub allocated_bytes: u64,
    /// Reclaimable bytes.
    pub reclaimable_bytes: u64,
}

/// End of a scan with its totals and rollups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFinish {
    /// Scan identifier (must have been started).
    pub scan_id: ScanId,
    /// When the scan finished.
    pub finished_at: i64,
    /// Outcome.
    pub status: ScanStatus,
    /// Totals.
    pub totals: ScanTotals,
    /// Per category/tier/ecosystem rollups. Buckets must be unique.
    pub rollups: Vec<Rollup>,
}

/// How a human approved a plan (§13.3). Model-mediated channels are never an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ApprovalMethod {
    /// `scull approve <plan-id>` in an interactive terminal with a typed confirmation code.
    CliTty,
    /// Interactive confirmation of a CLI `clean` the user started themselves.
    CliInteractive,
    /// The desktop Cleanup Review dialog.
    Desktop,
}

impl ApprovalMethod {
    /// Stable string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMethod::CliTty => "cli_tty",
            ApprovalMethod::CliInteractive => "cli_interactive",
            ApprovalMethod::Desktop => "desktop",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Self> {
        [
            ApprovalMethod::CliTty,
            ApprovalMethod::CliInteractive,
            ApprovalMethod::Desktop,
        ]
        .into_iter()
        .find(|m| m.as_str() == s)
    }
}

/// A human approval of one plan, bound to its content hash (§13.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApproval {
    /// Approved plan (must already be recorded with [`crate::Store::insert_plan`]).
    pub plan_id: PlanId,
    /// Hash of the plan content the human saw.
    pub content_hash: ContentHash,
    /// Approval channel.
    pub method: ApprovalMethod,
    /// OS user who approved.
    pub approver: String,
    /// When the approval was given.
    pub approved_at: i64,
    /// When it lapses (no later than the plan's own expiry).
    pub expires_at: i64,
}

/// A recorded approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// Row identifier.
    pub approval_id: i64,
    /// Approved plan.
    pub plan_id: PlanId,
    /// Bound content hash.
    pub content_hash: ContentHash,
    /// Approval channel.
    pub method: ApprovalMethod,
    /// OS user who approved.
    pub approver: String,
    /// When the approval was given.
    pub approved_at: i64,
    /// When it lapses.
    pub expires_at: i64,
    /// When it was consumed by an execution (single use).
    pub used_at: Option<i64>,
}

/// Start of an executed operation (SAFE-08), passed to [`crate::Store::begin_operation`].
/// Append-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationStart {
    /// Operation identifier.
    pub operation_id: OperationId,
    /// Executed plan.
    pub plan_id: PlanId,
    /// Hash of the executed plan.
    pub content_hash: ContentHash,
    /// Who approved it.
    pub approver: String,
    /// How it was approved.
    pub method: ApprovalMethod,
    /// When execution started.
    pub started_at: i64,
}
