//! Retention, downsampling and the storage budget (INS-02, DATA-05, DATA-06, FX-DATA-02).
//!
//! scull records generously but must never become a disk hog itself. [`Store::maintain`] is
//! the hook background maintenance calls during scheduled checks (never during an interactive
//! cleanup). It thins the **raw history streams**, always oldest first:
//!
//! | Stream | Thinned to |
//! |---|---|
//! | free-space samples (`free_space_samples`) | last sample per volume per bucket |
//! | scull's own footprint (`footprint_samples`) | last sample per bucket |
//! | item size/tier changes (`item_events` of kind `changed`) | last change per item per bucket |
//!
//! Lifecycle events (`first_seen`, `disappeared`, `reappeared`), regrowth events, scans and
//! their rollups are kept: they are small (they grow with churn, not with time) and other
//! history depends on them. Append-only tables (plans, approvals, operations, audit) and the
//! per-volume `volumes` cache read by `scull status --short` are never touched.
//!
//! # Age tiers (INS-02)
//!
//! * Raw data older than [`RetentionPolicy::raw_secs`] (12 months) is thinned to one point per
//!   UTC day ("daily rollups").
//! * Data older than [`RetentionPolicy::daily_secs`] (3 years) is thinned to one point per
//!   UTC month ("monthly rollups", kept forever).
//!
//! # Storage budget (FX-DATA-02)
//!
//! With [`RetentionPolicy::budget_bytes`] set (default [`DEFAULT_BUDGET_BYTES`], 1 GiB), the
//! bytes counted against the budget are [`Store::budget_bytes_used`]: the database pages in
//! use (`(page_count - freelist_count) × page_size`) plus migration backups and set-aside
//! damaged copies. When that exceeds the budget, maintenance walks month boundaries from the
//! oldest raw sample forward and thins everything older than each boundary to **daily**, then
//! (if still over) to **monthly**, stopping as soon as the budget is respected. The newest raw
//! data is therefore the last to be downsampled. If even monthly points of everything do not
//! fit (for example because backups alone exceed the budget),
//! [`MaintenanceReport::over_budget`] is set; nothing else is deleted.
//!
//! Freed pages go on SQLite's free list and are reused by later writes, so the file stops
//! growing; it is not shrunk here (no `VACUUM`, which rewrites the whole file).

use crate::error::Result;
use crate::store::Store;
use rusqlite::{Transaction, params};

/// Default storage budget for scull's own data (INS-02): 1 GiB.
pub const DEFAULT_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;

/// Retention settings for [`Store::maintain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Raw samples and events older than this many seconds are thinned to one per UTC day
    /// (INS-02: raw data is kept 12 months).
    pub raw_secs: i64,
    /// Data older than this many seconds is thinned to one per UTC month (INS-02: daily
    /// rollups are kept 3 years, monthly rollups forever).
    pub daily_secs: i64,
    /// Storage budget in bytes (see the module docs), or `None` for no budget.
    pub budget_bytes: Option<u64>,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            raw_secs: 365 * 86_400,
            daily_secs: 3 * 365 * 86_400,
            budget_bytes: Some(DEFAULT_BUDGET_BYTES),
        }
    }
}

/// What [`Store::maintain`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// Free-space samples removed by downsampling.
    pub free_space_samples_removed: u64,
    /// Footprint samples removed by downsampling.
    pub footprint_samples_removed: u64,
    /// `changed` item events removed by downsampling.
    pub item_events_removed: u64,
    /// Bytes counted against the budget before maintenance ([`Store::budget_bytes_used`]).
    pub bytes_before: u64,
    /// Bytes counted against the budget after maintenance.
    pub bytes_after: u64,
    /// The budget applied, if any.
    pub budget_bytes: Option<u64>,
    /// The newest cutoff the budget pass had to downsample up to (Unix seconds), if the
    /// budget pass ran.
    pub budget_cutoff: Option<i64>,
    /// Still over budget after thinning everything that may be thinned.
    pub over_budget: bool,
}

impl MaintenanceReport {
    fn add(&mut self, t: Thinned) {
        self.free_space_samples_removed += t.free_space;
        self.footprint_samples_removed += t.footprint;
        self.item_events_removed += t.item_events;
    }
}

/// Downsampling granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    Day,
    Month,
}

impl Bucket {
    /// `strftime` format naming the UTC bucket of a Unix time.
    fn format(self) -> &'static str {
        match self {
            Bucket::Day => "%Y-%m-%d",
            Bucket::Month => "%Y-%m",
        }
    }
}

/// Rows removed by one [`thin`] call.
#[derive(Debug, Clone, Copy, Default)]
struct Thinned {
    free_space: u64,
    footprint: u64,
    item_events: u64,
}

impl Store {
    /// Retention, downsampling and storage-budget hook for background maintenance (DATA-06,
    /// FX-DATA-02); see the [module docs](crate::retention). Safe to call at any time and
    /// repeatedly: with nothing old enough and the budget respected it changes nothing. Call it
    /// from scheduled checks, never during an interactive cleanup.
    pub fn maintain(&mut self, policy: &RetentionPolicy, now: i64) -> Result<MaintenanceReport> {
        let outside = self.outside_db_bytes();
        let raw_cutoff = now.saturating_sub(policy.raw_secs.max(0));
        let daily_cutoff = now.saturating_sub(policy.daily_secs.max(0));
        self.write(|tx| {
            let mut report = MaintenanceReport {
                budget_bytes: policy.budget_bytes,
                bytes_before: used_db_bytes(tx)?.saturating_add(outside),
                ..MaintenanceReport::default()
            };
            report.add(thin(tx, raw_cutoff, Bucket::Day)?);
            report.add(thin(tx, daily_cutoff, Bucket::Month)?);

            if let Some(budget) = policy.budget_bytes {
                let over = |tx: &Transaction<'_>| -> Result<bool> {
                    Ok(used_db_bytes(tx)?.saturating_add(outside) > budget)
                };
                'passes: for bucket in [Bucket::Day, Bucket::Month] {
                    if !over(tx)? {
                        break;
                    }
                    let Some(oldest) = oldest_raw(tx)? else {
                        break;
                    };
                    let mut cutoff = next_month_start(tx, oldest)?;
                    loop {
                        report.add(thin(tx, cutoff, bucket)?);
                        report.budget_cutoff =
                            Some(report.budget_cutoff.map_or(cutoff, |c| c.max(cutoff)));
                        if !over(tx)? {
                            break 'passes;
                        }
                        if cutoff > now {
                            break;
                        }
                        cutoff = next_month_start(tx, cutoff)?;
                    }
                }
                report.over_budget = over(tx)?;
            }
            report.bytes_after = used_db_bytes(tx)?.saturating_add(outside);
            Ok(report)
        })
    }

    /// Bytes counted against the storage budget: database pages in use plus migration backups
    /// and set-aside damaged copies (see the [module docs](crate::retention)).
    pub fn budget_bytes_used(&self) -> Result<u64> {
        let outside = self.outside_db_bytes();
        self.read(|conn| Ok(used_db_bytes(conn)?.saturating_add(outside)))
    }

    /// Bytes of scull's own files that are not the live database (backups, damaged copies).
    fn outside_db_bytes(&self) -> u64 {
        let f = self.footprint();
        f.backup_bytes.saturating_add(f.damaged_bytes)
    }
}

/// Database pages in use, in bytes (free-list pages excluded). Inside a write transaction this
/// reflects its uncommitted changes.
fn used_db_bytes(conn: &rusqlite::Connection) -> Result<u64> {
    let (pages, free, size): (i64, i64, i64) = conn.query_row(
        "SELECT (SELECT page_count FROM pragma_page_count()), \
                (SELECT freelist_count FROM pragma_freelist_count()), \
                (SELECT page_size FROM pragma_page_size())",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let used = u64::try_from(pages.saturating_sub(free)).unwrap_or(0);
    Ok(used.saturating_mul(u64::try_from(size).unwrap_or(0)))
}

/// Oldest timestamp among the thinnable raw streams.
fn oldest_raw(tx: &Transaction<'_>) -> Result<Option<i64>> {
    Ok(tx.query_row(
        "SELECT min(t) FROM ( \
           SELECT min(sampled_at) AS t FROM free_space_samples \
           UNION ALL SELECT min(sampled_at) FROM footprint_samples \
           UNION ALL SELECT min(occurred_at) FROM item_events WHERE kind = 'changed')",
        [],
        |r| r.get(0),
    )?)
}

/// The first second of the UTC month after the one containing `t`.
fn next_month_start(tx: &Transaction<'_>, t: i64) -> Result<i64> {
    Ok(tx.query_row(
        "SELECT CAST(strftime('%s', ?1, 'unixepoch', 'start of month', '+1 month') AS INTEGER)",
        [t],
        |r| r.get(0),
    )?)
}

/// Thin every raw stream older than `before` to its last point per `bucket` (see the module
/// docs). SQLite returns the bare id column from the row holding `max(<time>)`.
fn thin(tx: &Transaction<'_>, before: i64, bucket: Bucket) -> Result<Thinned> {
    let f = bucket.format();
    let free_space = tx.execute(
        &format!(
            "DELETE FROM free_space_samples WHERE sampled_at < ?1 AND sample_id NOT IN ( \
               SELECT sample_id FROM ( \
                 SELECT sample_id, max(sampled_at) FROM free_space_samples \
                 WHERE sampled_at < ?1 \
                 GROUP BY volume, strftime('{f}', sampled_at, 'unixepoch')))"
        ),
        params![before],
    )?;
    let footprint = tx.execute(
        &format!(
            "DELETE FROM footprint_samples WHERE sampled_at < ?1 AND sample_id NOT IN ( \
               SELECT sample_id FROM ( \
                 SELECT sample_id, max(sampled_at) FROM footprint_samples \
                 WHERE sampled_at < ?1 \
                 GROUP BY strftime('{f}', sampled_at, 'unixepoch')))"
        ),
        params![before],
    )?;
    let item_events = tx.execute(
        &format!(
            "DELETE FROM item_events WHERE kind = 'changed' AND occurred_at < ?1 \
             AND event_id NOT IN ( \
               SELECT event_id FROM ( \
                 SELECT event_id, max(occurred_at) FROM item_events \
                 WHERE kind = 'changed' AND occurred_at < ?1 \
                 GROUP BY item_id, strftime('{f}', occurred_at, 'unixepoch')))"
        ),
        params![before],
    )?;
    Ok(Thinned {
        free_space: free_space as u64,
        footprint: footprint as u64,
        item_events: item_events as u64,
    })
}
