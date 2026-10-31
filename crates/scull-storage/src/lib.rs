//! SQLite store (WAL): scans, items, plans, approvals, audit, insights (D-14, PRD §18.3).
//!
//! One schema shared by every interface; nothing outside this crate writes SQL (DATA-01).
//!
//! * [`Store::open`] creates or opens the database, refuses newer or foreign schemas and
//!   damaged files without touching them, and runs embedded migrations with a backup before
//!   each one (DATA-03).
//! * Owner-only permissions (DATA-04) come from an [`OwnerOnly`] implementation injected by
//!   the caller (OS APIs live in `scull-platform`).
//! * Plans, approvals, operations and audit are append-only, enforced by SQLite triggers;
//!   approvals are single-use and expiring (§13.3).
//! * History is recorded around every scan from the first one ([`Store::record_scan`]):
//!   rollups, item lifecycle, free space per volume and scull's own footprint, with cheap
//!   queries for insights, forecasts and `scull status --short` ([`Store::status_summary`]);
//!   lifecycle rules, privacy (INS-07) and retention are described in [`history`].
//!   Retention and the storage budget for background maintenance: [`Store::maintain`]
//!   ([`retention`], DATA-06, FX-DATA-02).
//! * Typed reads behind local insights and their purge ([`insights`], INS-02/03).
//! * Alerts and other events (INS-06): [`Store::record_alerts`], [`Store::alerts`],
//!   [`Store::record_events`], [`Store::latest_event`]; each scan's roots:
//!   [`Store::scan_roots`].
//! * The database location is the caller's job: a local, **non-roaming** per-user data
//!   directory (never `%APPDATA%\Roaming` on Windows; FX-WIN-12) joined with
//!   [`DB_FILE_NAME`].
//!
//! The bundled SQLite (rusqlite `bundled`) gives every OS the same SQLite version and features.

mod backup;
mod error;
mod events;
pub mod history;
pub mod insights;
mod migrate;
mod perms;
mod records;
pub mod retention;
mod store;

pub use backup::{list_backups, restore_backup};
pub use error::{Result, StoreError};
pub use events::{AlertEvent, Event, EventKind};
pub use history::{
    Footprint, FootprintSample, ItemEvent, ItemEventKind, ItemHistory, LastScan, RegrowthEvent,
    RollupPoint, SIZE_CHANGE_FRACTION, SIZE_CHANGE_MIN_BYTES, ScanRecord, ScanRecorded,
    ScanSummary, StatusSummary, VolumeSample, rollups_from_candidates,
};
pub use insights::{InsightsPurged, ItemTimeline, OperationAccuracy};
pub use migrate::{APPLICATION_ID, SCHEMA_VERSION};
pub use perms::{DefaultPermissions, OwnerOnly};
pub use records::{
    Approval, ApprovalMethod, AuditEntry, AuditOutcome, NewApproval, OperationStart, Rollup,
    ScanFinish, ScanStart, ScanStatus, ScanTotals,
};
pub use retention::{DEFAULT_BUDGET_BYTES, MaintenanceReport, RetentionPolicy};
pub use store::{Store, StoreOptions};

use std::path::{Path, PathBuf};

/// Crate name, used in diagnostics and `scull doctor` output.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// File name of the database inside scull's local (non-roaming) data directory.
pub const DB_FILE_NAME: &str = "scull.db";

/// Every file that belongs to the database at `db_path` and exists: the database, its `-wal`
/// and `-shm` files, migration backups and set-aside damaged copies. Used to show scull's own
/// footprint as its own category (DATA-05).
pub fn footprint_files(db_path: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ["", "-wal", "-shm"]
        .iter()
        .map(|s| backup::with_suffix(db_path, s))
        .filter(|p| p.is_file())
        .collect();
    out.extend(list_backups(db_path));
    out.extend(backup::list_damaged(db_path));
    out
}

#[cfg(test)]
mod tests;
