//! History recording around every scan, from the very first one (INS-01, INS-06), and the
//! cheap queries the insights, forecast and status features read (PRD §15, §16, PRO-09).
//!
//! # Recording
//!
//! The CLI (or desktop, or `scull check`) calls, for every scan:
//!
//! 1. [`Store::record_scan_start`] when the scan begins;
//! 2. [`Store::record_scan`] when it ends, with a [`ScanRecord`]: the [`ScanFinish`] (totals and
//!    per category × tier × ecosystem [`Rollup`]s, see [`rollups_from_candidates`]), the
//!    candidates seen, and one [`VolumeSample`] per volume the caller measured (measuring free
//!    space is a platform concern, so the value is passed in).
//!
//! `record_scan` writes everything in one transaction: the scan row and rollups, the item
//! lifecycle, the free-space samples, and a sample of scull's own footprint (DATA-05).
//!
//! # Item lifecycle
//!
//! * Every candidate seen updates its item row (`last_seen_*`, sizes, tier). A new item gets a
//!   `first_seen` event; later sightings add a `changed` event only when the tier changes or the
//!   allocated size moved by at least [`SIZE_CHANGE_MIN_BYTES`] and [`SIZE_CHANGE_FRACTION`]
//!   since the last recorded event, so history grows with churn, not with the number of scans.
//! * **Absence** marks an item gone only after a [`ScanStatus::Completed`] scan whose roots
//!   contain the item's path and that did not see it. Partial, failed and cancelled scans never
//!   mark anything gone (an unreadable or unvisited directory is not a deleted one).
//! * **Replacement** is positive evidence: when a candidate with a different [`ItemId`] (new
//!   file identity) is seen at the path of a live item, the old item is marked gone at that
//!   scan, whatever its status. (This relies on stable file identities from `scull-platform`;
//!   a filesystem without them would show every sighting as a rebuild.)
//! * An item that reappears (the same item coming back, e.g. restored from quarantine, or a new
//!   item at the path where one of the same category went away, e.g. a rebuilt `node_modules`)
//!   is recorded as a [`RegrowthEvent`].
//!
//! # What is never recorded (INS-07)
//!
//! Only paths, categories, artifact names, tiers, sizes, counts and times. Never file contents,
//! evidence or reason text, environment values or anything about paths outside scan roots.
//! Rollups only: no table holds a row per scanned file; `items` holds candidates (DATA-02).
//!
//! # Retention and downsampling (DATA-06, INS-02)
//!
//! [`Store::maintain`] is the hook background maintenance calls during scheduled checks (never
//! during an interactive cleanup); see [`crate::retention`] for the age tiers and the storage
//! budget (FX-DATA-02). Append-only tables (plans, approvals, operations, audit) are never
//! touched.

use crate::error::{Result, StoreError};
use crate::records::{Rollup, ScanFinish, ScanStatus, ScanTotals};
use crate::store::{Store, int, internal, serde_name};
use rusqlite::{OptionalExtension, Transaction, params};
use scull_core::{Candidate, Category, ErrorKind, FsPath, ItemId, ScanId, Tier};
use std::collections::BTreeMap;
use std::path::Path;

/// A `changed` item event needs the allocated size to move by at least this many bytes...
pub const SIZE_CHANGE_MIN_BYTES: u64 = 1024 * 1024;
/// ...and by at least this fraction of the size at the last recorded event.
pub const SIZE_CHANGE_FRACTION: f64 = 0.05;

/// Free space of one volume at one moment, measured by the caller (`scull-platform`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSample {
    /// Stable volume identifier (e.g. mount point or volume GUID path). Not empty.
    pub volume_id: String,
    /// Volume size in bytes.
    pub total: u64,
    /// Bytes available to the user. Must not exceed `total`.
    pub free: u64,
    /// When it was measured (Unix seconds).
    pub at: i64,
}

/// Everything recorded when a scan ends; see [`Store::record_scan`].
#[derive(Debug, Clone)]
pub struct ScanRecord<'a> {
    /// Outcome, totals and rollups.
    pub finish: ScanFinish,
    /// Candidates the scan saw (all of them, even for partial or cancelled scans).
    pub candidates: &'a [Candidate],
    /// Free space of the volumes the scan covered.
    pub volumes: &'a [VolumeSample],
}

/// What [`Store::record_scan`] changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanRecorded {
    /// Items seen for the first time.
    pub new_items: u64,
    /// Items seen before and seen again.
    pub seen_again: u64,
    /// Items that had been marked gone and were seen again (same item ID).
    pub reappeared: u64,
    /// Items marked gone by this scan (absence after a completed scan, or replacement).
    pub disappeared: u64,
    /// Regrowth events recorded.
    pub regrowth: u64,
    /// scull's own footprint at the time of recording.
    pub footprint: Footprint,
}

/// scull's own disk use (DATA-05), in bytes (logical file lengths).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Footprint {
    /// The database file.
    pub database_bytes: u64,
    /// The `-wal` and `-shm` files.
    pub wal_bytes: u64,
    /// Migration backups.
    pub backup_bytes: u64,
    /// Damaged databases set aside by [`crate::restore_backup`].
    pub damaged_bytes: u64,
}

impl Footprint {
    /// Sum of all parts.
    pub fn total(&self) -> u64 {
        self.database_bytes
            .saturating_add(self.wal_bytes)
            .saturating_add(self.backup_bytes)
            .saturating_add(self.damaged_bytes)
    }
}

/// A recorded footprint sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FootprintSample {
    /// When it was measured.
    pub at: i64,
    /// Scan it was recorded with, if any.
    pub scan_id: Option<ScanId>,
    /// The sizes.
    pub footprint: Footprint,
}

/// A finished scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSummary {
    /// Scan identifier.
    pub scan_id: ScanId,
    /// When it started.
    pub started_at: i64,
    /// When it finished.
    pub finished_at: i64,
    /// Outcome.
    pub status: ScanStatus,
    /// Roots.
    pub roots: Vec<FsPath>,
    /// Totals.
    pub totals: ScanTotals,
}

/// One point of [`Store::rollup_history`]: a category's bytes in one scan (all tiers and
/// ecosystems summed; zero when the scan found nothing of that category).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupPoint {
    /// Scan finish time.
    pub at: i64,
    /// The scan.
    pub scan_id: ScanId,
    /// Candidates.
    pub items: u64,
    /// Logical bytes.
    pub logical_bytes: u64,
    /// Allocated bytes.
    pub allocated_bytes: u64,
    /// Reclaimable bytes.
    pub reclaimable_bytes: u64,
}

/// Kind of an [`ItemEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ItemEventKind {
    /// First sighting.
    FirstSeen,
    /// Tier changed or size moved significantly.
    Changed,
    /// Marked gone.
    Disappeared,
    /// Seen again after being marked gone.
    Reappeared,
}

impl ItemEventKind {
    fn as_str(self) -> &'static str {
        match self {
            ItemEventKind::FirstSeen => "first_seen",
            ItemEventKind::Changed => "changed",
            ItemEventKind::Disappeared => "disappeared",
            ItemEventKind::Reappeared => "reappeared",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [
            ItemEventKind::FirstSeen,
            ItemEventKind::Changed,
            ItemEventKind::Disappeared,
            ItemEventKind::Reappeared,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }
}

/// One lifecycle event of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemEvent {
    /// When it happened (scan time).
    pub at: i64,
    /// Scan that observed it.
    pub scan_id: ScanId,
    /// What happened.
    pub kind: ItemEventKind,
    /// Tier at that time.
    pub tier: Tier,
    /// Logical bytes at that time.
    pub logical_bytes: u64,
    /// Allocated bytes at that time.
    pub allocated_bytes: u64,
    /// Reclaimable bytes at that time.
    pub reclaimable_bytes: u64,
}

/// An item's lifecycle and its recorded events (oldest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemHistory {
    /// The item.
    pub item_id: ItemId,
    /// Its path.
    pub path: FsPath,
    /// Its category.
    pub category: Category,
    /// Its current (last seen) tier.
    pub tier: Tier,
    /// First sighting.
    pub first_seen_at: i64,
    /// Last sighting.
    pub last_seen_at: i64,
    /// When it was marked gone, if it is gone.
    pub disappeared_at: Option<i64>,
    /// Events, oldest first.
    pub events: Vec<ItemEvent>,
}

/// An artifact that came back where one had gone (INS-03 regrowth, INS-06 rebuild events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegrowthEvent {
    /// Path (same for both items).
    pub path: FsPath,
    /// Artifact type, e.g. "Cargo build output".
    pub artifact: String,
    /// The item seen now.
    pub item_id: ItemId,
    /// The item that had gone. Equal to `item_id` when the same object came back (e.g. a
    /// restore from quarantine) rather than being rebuilt.
    pub previous_item_id: ItemId,
    /// Last sighting of the previous item.
    pub previous_last_seen_at: i64,
    /// When the previous item was marked gone.
    pub disappeared_at: i64,
    /// When the artifact was seen again.
    pub reappeared_at: i64,
    /// Scan that saw it again.
    pub scan_id: ScanId,
}

impl RegrowthEvent {
    /// Whether the same object came back (not a rebuild).
    pub fn same_item(&self) -> bool {
        self.item_id == self.previous_item_id
    }
}

/// The latest usable scan, as shown by `scull status --short`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastScan {
    /// Scan identifier.
    pub scan_id: ScanId,
    /// When it started.
    pub started_at: i64,
    /// When it finished.
    pub finished_at: i64,
    /// `Completed` or `Partial`.
    pub status: ScanStatus,
    /// Candidates found.
    pub candidates: u64,
    /// Reclaimable bytes of all candidates.
    pub reclaimable_bytes: u64,
    /// Reclaimable bytes of S0 (safe) candidates.
    pub safe_reclaimable_bytes: u64,
}

/// Cached status for `scull status --short` (PRO-09), read with one indexed query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusSummary {
    /// Latest scan that finished `completed` or `partial` (by start time), if any.
    pub last_scan: Option<LastScan>,
    /// Latest free-space sample of every volume ever sampled, by volume ID.
    pub volumes: Vec<VolumeSample>,
}

/// Per category × tier × ecosystem rollups of `candidates`, sorted, for [`ScanFinish::rollups`].
pub fn rollups_from_candidates(candidates: &[Candidate]) -> Result<Vec<Rollup>> {
    let mut buckets: BTreeMap<(String, String, String), Rollup> = BTreeMap::new();
    for c in candidates {
        let ecosystem = c.project.as_ref().map(|p| p.ecosystem.clone());
        let key = (
            serde_name(&c.category)?,
            c.tier.to_string(),
            ecosystem.clone().unwrap_or_default(),
        );
        let r = buckets.entry(key).or_insert_with(|| Rollup {
            category: c.category,
            tier: c.tier,
            ecosystem,
            items: 0,
            logical_bytes: 0,
            allocated_bytes: 0,
            reclaimable_bytes: 0,
        });
        r.items = r.items.saturating_add(1);
        r.logical_bytes = r.logical_bytes.saturating_add(c.sizes.logical);
        r.allocated_bytes = r.allocated_bytes.saturating_add(c.sizes.allocated);
        r.reclaimable_bytes = r
            .reclaimable_bytes
            .saturating_add(c.sizes.reclaimable.bytes);
    }
    Ok(buckets.into_values().collect())
}

const USABLE: &str = "('completed', 'partial')";

impl Store {
    /// Record the end of a scan and everything history needs, atomically: the scan's totals
    /// and rollups, the lifecycle of every candidate seen, items gone since the last scan (only
    /// if the scan [`ScanStatus::Completed`]), free-space samples and scull's own footprint.
    ///
    /// The scan must have been started with [`Store::record_scan_start`]. All sightings and
    /// disappearances are stamped with `finish.finished_at`.
    pub fn record_scan(&mut self, record: &ScanRecord<'_>) -> Result<ScanRecorded> {
        validate_volumes(record.volumes)?;
        let footprint = self.footprint();
        let finish = &record.finish;
        self.write(|tx| {
            crate::store::finish_scan_tx(tx, finish)?;
            let mut out = ScanRecorded {
                footprint,
                ..ScanRecorded::default()
            };
            observe_items(
                tx,
                &finish.scan_id,
                finish.finished_at,
                record.candidates,
                &mut out,
            )?;
            if finish.status == ScanStatus::Completed {
                mark_absent_items_gone(tx, &finish.scan_id, finish.finished_at, &mut out)?;
            }
            insert_volume_samples(tx, record.volumes, Some(&finish.scan_id))?;
            tx.execute(
                "INSERT INTO footprint_samples (sampled_at, scan_id, database_bytes, wal_bytes, \
                 backup_bytes, damaged_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    finish.finished_at,
                    finish.scan_id.as_str(),
                    int(footprint.database_bytes)?,
                    int(footprint.wal_bytes)?,
                    int(footprint.backup_bytes)?,
                    int(footprint.damaged_bytes)?,
                ],
            )?;
            Ok(out)
        })
    }

    /// Record free-space samples outside a scan (e.g. a scheduled `scull check`), or attach
    /// them to `scan`.
    pub fn record_free_space(
        &mut self,
        samples: &[VolumeSample],
        scan: Option<&ScanId>,
    ) -> Result<()> {
        validate_volumes(samples)?;
        self.write(|tx| insert_volume_samples(tx, samples, scan))
    }

    /// scull's own footprint right now, measured from the files next to the database
    /// ([`crate::footprint_files`]). Missing files count as zero.
    pub fn footprint(&self) -> Footprint {
        let db = self.path();
        let backups = crate::backup::list_backups(db);
        let damaged = crate::backup::list_damaged(db);
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let sum = |ps: &[std::path::PathBuf]| {
            ps.iter()
                .fold(0u64, |a, p| a.saturating_add(size(p.as_path())))
        };
        Footprint {
            database_bytes: size(db),
            wal_bytes: size(&crate::backup::with_suffix(db, "-wal"))
                .saturating_add(size(&crate::backup::with_suffix(db, "-shm"))),
            backup_bytes: sum(&backups),
            damaged_bytes: sum(&damaged),
        }
    }

    /// The latest scan of `root` (exact root string as recorded) that finished `completed` or
    /// `partial`, by start time. Cancelled and failed scans are skipped: their totals are
    /// incomplete. One index walk on `scan_roots`.
    pub fn latest_scan(&self, root: &FsPath) -> Result<Option<ScanSummary>> {
        self.read(|conn| {
            conn.query_row(
                &format!(
                    "SELECT s.scan_id, s.started_at, s.finished_at, s.status, s.roots_json, \
                     s.entries_scanned, s.candidates, s.logical_bytes, s.allocated_bytes, \
                     s.reclaimable_bytes, s.errors \
                     FROM scan_roots r JOIN scans s ON s.scan_id = r.scan_id \
                     WHERE r.root = ?1 AND s.status IN {USABLE} \
                     ORDER BY r.started_at DESC, r.scan_id DESC LIMIT 1"
                ),
                [root.as_str()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        [
                            r.get::<_, i64>(5)?,
                            r.get::<_, i64>(6)?,
                            r.get::<_, i64>(7)?,
                            r.get::<_, i64>(8)?,
                            r.get::<_, i64>(9)?,
                            r.get::<_, i64>(10)?,
                        ],
                    ))
                },
            )
            .optional()?
            .map(|(id, started_at, finished_at, status, roots, t)| {
                Ok(ScanSummary {
                    scan_id: scan_id(id)?,
                    started_at,
                    finished_at,
                    status: status_of(&status)?,
                    roots: serde_json::from_str(&roots).map_err(internal)?,
                    totals: ScanTotals {
                        entries_scanned: uint(t[0]),
                        candidates: uint(t[1]),
                        logical_bytes: uint(t[2]),
                        allocated_bytes: uint(t[3]),
                        reclaimable_bytes: uint(t[4]),
                        errors: uint(t[5]),
                    },
                })
            })
            .transpose()
        })
    }

    /// Bytes of `category` per usable (`completed`/`partial`) scan finished at or after
    /// `since`, oldest first. Every usable scan yields a point (zero if it found nothing of the
    /// category), so cleanups show as drops. Points of scans with different roots are not
    /// comparable; use [`ScanSummary::roots`] via [`Store::latest_scan`] to pick a series.
    pub fn rollup_history(&self, category: Category, since: i64) -> Result<Vec<RollupPoint>> {
        let cat = serde_name(&category)?;
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT s.finished_at, s.scan_id, coalesce(sum(r.items), 0), \
                 coalesce(sum(r.logical_bytes), 0), coalesce(sum(r.allocated_bytes), 0), \
                 coalesce(sum(r.reclaimable_bytes), 0) \
                 FROM scans s LEFT JOIN scan_rollups r \
                   ON r.scan_id = s.scan_id AND r.category = ?1 \
                 WHERE s.finished_at >= ?2 AND s.status IN {USABLE} \
                 GROUP BY s.scan_id ORDER BY s.finished_at, s.scan_id"
            ))?;
            let rows = stmt.query_map(params![cat, since], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    [
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ],
                ))
            })?;
            rows.map(|row| {
                let (at, id, v) = row?;
                Ok(RollupPoint {
                    at,
                    scan_id: scan_id(id)?,
                    items: uint(v[0]),
                    logical_bytes: uint(v[1]),
                    allocated_bytes: uint(v[2]),
                    reclaimable_bytes: uint(v[3]),
                })
            })
            .collect()
        })
    }

    /// Free-space samples of `volume_id` taken at or after `since`, oldest first (input for
    /// the disk-full forecast, PRO-01).
    pub fn free_space_history(&self, volume_id: &str, since: i64) -> Result<Vec<VolumeSample>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT volume, total_bytes, free_bytes, sampled_at FROM free_space_samples \
                 WHERE volume = ?1 AND sampled_at >= ?2 ORDER BY sampled_at, sample_id",
            )?;
            let rows = stmt.query_map(params![volume_id, since], volume_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Recorded footprint samples taken at or after `since`, oldest first.
    pub fn footprint_history(&self, since: i64) -> Result<Vec<FootprintSample>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT sampled_at, scan_id, database_bytes, wal_bytes, backup_bytes, \
                 damaged_bytes FROM footprint_samples WHERE sampled_at >= ?1 \
                 ORDER BY sampled_at, sample_id",
            )?;
            let rows = stmt.query_map([since], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    [
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ],
                ))
            })?;
            rows.map(|row| {
                let (at, id, v) = row?;
                Ok(FootprintSample {
                    at,
                    scan_id: id.map(scan_id).transpose()?,
                    footprint: Footprint {
                        database_bytes: uint(v[0]),
                        wal_bytes: uint(v[1]),
                        backup_bytes: uint(v[2]),
                        damaged_bytes: uint(v[3]),
                    },
                })
            })
            .collect()
        })
    }

    /// An item's lifecycle and events, or `None` if it was never recorded.
    pub fn item_history(&self, item_id: &ItemId) -> Result<Option<ItemHistory>> {
        self.read(|conn| {
            let row = conn
                .query_row(
                    "SELECT path, category, tier, first_seen_at, last_seen_at, disappeared_at \
                     FROM items WHERE item_id = ?1",
                    [item_id.as_str()],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((path, category, tier, first_seen_at, last_seen_at, disappeared_at)) = row
            else {
                return Ok(None);
            };
            let mut stmt = conn.prepare(
                "SELECT occurred_at, scan_id, kind, tier, logical_bytes, allocated_bytes, \
                 reclaimable_bytes FROM item_events WHERE item_id = ?1 ORDER BY event_id",
            )?;
            let events = stmt
                .query_map([item_id.as_str()], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        [
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, i64>(6)?,
                        ],
                    ))
                })?
                .map(|row| {
                    let (at, id, kind, tier, v) = row?;
                    Ok(ItemEvent {
                        at,
                        scan_id: scan_id(id)?,
                        kind: ItemEventKind::parse(&kind)
                            .ok_or_else(|| bad(format!("unknown item event kind {kind:?}")))?,
                        tier: tier_of(&tier)?,
                        logical_bytes: uint(v[0]),
                        allocated_bytes: uint(v[1]),
                        reclaimable_bytes: uint(v[2]),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Some(ItemHistory {
                item_id: item_id.clone(),
                path: FsPath::from(path.as_str()),
                category: category_of(&category)?,
                tier: tier_of(&tier)?,
                first_seen_at,
                last_seen_at,
                disappeared_at,
                events,
            }))
        })
    }

    /// Artifacts of `category` that came back at or after `since`, oldest first.
    pub fn regrowth(&self, category: Category, since: i64) -> Result<Vec<RegrowthEvent>> {
        let cat = serde_name(&category)?;
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT path, artifact, item_id, previous_item_id, previous_last_seen_at, \
                 disappeared_at, reappeared_at, scan_id FROM regrowth_events \
                 WHERE category = ?1 AND reappeared_at >= ?2 \
                 ORDER BY reappeared_at, event_id",
            )?;
            let rows = stmt.query_map(params![cat, since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    [
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                    ],
                    r.get::<_, String>(7)?,
                ))
            })?;
            rows.map(|row| {
                let (path, artifact, item, prev, t, scan) = row?;
                Ok(RegrowthEvent {
                    path: FsPath::from(path.as_str()),
                    artifact,
                    item_id: item_id(item)?,
                    previous_item_id: item_id(prev)?,
                    previous_last_seen_at: t[0],
                    disappeared_at: t[1],
                    reappeared_at: t[2],
                    scan_id: scan_id(scan)?,
                })
            })
            .collect()
        })
    }

    /// Everything `scull status --short` shows, in one statement over indexes (the latest
    /// usable scan via `scans_started`, its S0 rollups via their primary key, and the
    /// per-volume cache): cost does not grow with the number of scans. No filesystem access.
    pub fn status_summary(&self) -> Result<StatusSummary> {
        self.read(|conn| {
            let (scan, volumes_json) = conn.query_row(
                &format!(
                    "WITH last AS ( \
                       SELECT scan_id, started_at, finished_at, status, candidates, \
                              reclaimable_bytes \
                       FROM scans WHERE status IN {USABLE} \
                       ORDER BY started_at DESC, scan_id DESC LIMIT 1) \
                     SELECT last.scan_id, last.started_at, last.finished_at, last.status, \
                       last.candidates, last.reclaimable_bytes, \
                       (SELECT coalesce(sum(r.reclaimable_bytes), 0) FROM scan_rollups r \
                        WHERE r.scan_id = last.scan_id AND r.tier = 'S0'), \
                       (SELECT json_group_array(json_array(volume, total_bytes, free_bytes, \
                                                           sampled_at)) \
                        FROM (SELECT * FROM volumes ORDER BY volume)) \
                     FROM (SELECT 1) LEFT JOIN last"
                ),
                [],
                |r| {
                    let id: Option<String> = r.get(0)?;
                    let scan = match id {
                        None => None,
                        Some(id) => Some((
                            id,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                            r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                            r.get::<_, i64>(6)?,
                        )),
                    };
                    Ok((scan, r.get::<_, String>(7)?))
                },
            )?;
            let last_scan = scan
                .map(|(id, started_at, finished_at, status, cand, recl, safe)| {
                    Ok::<_, StoreError>(LastScan {
                        scan_id: scan_id(id)?,
                        started_at,
                        finished_at,
                        status: status_of(&status)?,
                        candidates: uint(cand),
                        reclaimable_bytes: uint(recl),
                        safe_reclaimable_bytes: uint(safe),
                    })
                })
                .transpose()?;
            let raw: Vec<(String, i64, i64, i64)> =
                serde_json::from_str(&volumes_json).map_err(internal)?;
            let volumes = raw
                .into_iter()
                .map(|(volume_id, total, free, at)| VolumeSample {
                    volume_id,
                    total: uint(total),
                    free: uint(free),
                    at,
                })
                .collect();
            Ok(StatusSummary { last_scan, volumes })
        })
    }
}

// ---- recording internals --------------------------------------------------------------------

fn validate_volumes(samples: &[VolumeSample]) -> Result<()> {
    for s in samples {
        if s.volume_id.is_empty() {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                "volume sample without a volume id",
            ));
        }
        if s.free > s.total {
            return Err(StoreError::rejected(
                ErrorKind::Usage,
                format!(
                    "volume {:?}: free bytes {} exceed total {}",
                    s.volume_id, s.free, s.total
                ),
            ));
        }
    }
    Ok(())
}

fn insert_volume_samples(
    tx: &Transaction<'_>,
    samples: &[VolumeSample],
    scan: Option<&ScanId>,
) -> Result<()> {
    let mut ins = tx.prepare(
        "INSERT INTO free_space_samples (sampled_at, scan_id, volume, total_bytes, free_bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut latest = tx.prepare(
        "INSERT INTO volumes (volume, sampled_at, scan_id, total_bytes, free_bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (volume) DO UPDATE SET sampled_at = excluded.sampled_at, \
           scan_id = excluded.scan_id, total_bytes = excluded.total_bytes, \
           free_bytes = excluded.free_bytes \
         WHERE excluded.sampled_at >= volumes.sampled_at",
    )?;
    for s in samples {
        let scan = scan.map(ScanId::as_str);
        let (total, free) = (int(s.total)?, int(s.free)?);
        ins.execute(params![s.at, scan, s.volume_id, total, free])?;
        latest.execute(params![s.volume_id, s.at, scan, total, free])?;
    }
    Ok(())
}

/// Current state of an item row.
struct Known {
    disappeared_at: Option<i64>,
    last_seen_at: i64,
}

/// Insert or refresh the lifecycle of every candidate seen by `scan` at `seen_at`, recording
/// item events and regrowth. Shared by [`Store::record_scan`] and [`Store::upsert_items`].
pub(crate) fn observe_items(
    tx: &Transaction<'_>,
    scan: &ScanId,
    seen_at: i64,
    candidates: &[Candidate],
    out: &mut ScanRecorded,
) -> Result<()> {
    let mut get_item =
        tx.prepare("SELECT disappeared_at, last_seen_at FROM items WHERE item_id = ?1")?;
    let mut last_event = tx.prepare(
        "SELECT tier, allocated_bytes FROM item_events WHERE item_id = ?1 \
         ORDER BY event_id DESC LIMIT 1",
    )?;
    let mut insert_item = tx.prepare(
        "INSERT INTO items (item_id, path, category, artifact, ecosystem, project_root, tier, \
         first_seen_at, last_seen_at, first_seen_scan, last_seen_scan, logical_bytes, \
         allocated_bytes, reclaimable_bytes) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?9, ?10, ?11, ?12)",
    )?;
    let mut update_item = tx.prepare(
        "UPDATE items SET path = ?2, category = ?3, artifact = ?4, ecosystem = ?5, \
         project_root = ?6, tier = ?7, last_seen_at = max(first_seen_at, ?8), \
         last_seen_scan = ?9, logical_bytes = ?10, allocated_bytes = ?11, \
         reclaimable_bytes = ?12, disappeared_at = NULL, disappeared_scan = NULL \
         WHERE item_id = ?1",
    )?;
    let mut live_at_path = tx.prepare(
        "SELECT item_id FROM items WHERE path = ?1 AND disappeared_at IS NULL \
         AND item_id <> ?2",
    )?;
    let mut gone_at_path = tx.prepare(
        "SELECT item_id, last_seen_at, disappeared_at FROM items \
         WHERE path = ?1 AND disappeared_at IS NOT NULL AND category = ?2 AND item_id <> ?3 \
         ORDER BY disappeared_at DESC, last_seen_at DESC LIMIT 1",
    )?;
    let mut insert_regrowth = tx.prepare(
        "INSERT INTO regrowth_events (item_id, previous_item_id, path, category, artifact, \
         previous_last_seen_at, disappeared_at, reappeared_at, scan_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;

    for c in candidates {
        let id = c.id.as_str();
        let category = serde_name(&c.category)?;
        let tier = c.tier.to_string();
        let sizes = [
            int(c.sizes.logical)?,
            int(c.sizes.allocated)?,
            int(c.sizes.reclaimable.bytes)?,
        ];
        let known = get_item
            .query_row([id], |r| {
                Ok(Known {
                    disappeared_at: r.get(0)?,
                    last_seen_at: r.get(1)?,
                })
            })
            .optional()?;
        let item_params = params![
            id,
            c.path.as_str(),
            category,
            c.artifact,
            c.project.as_ref().map(|p| p.ecosystem.as_str()),
            c.project.as_ref().map(|p| p.root.as_str()),
            tier,
            seen_at,
            scan.as_str(),
            sizes[0],
            sizes[1],
            sizes[2],
        ];
        let kind = match known {
            None => {
                // A different object now sits at this path: the old one is gone (replacement
                // is positive evidence, unlike absence).
                let replaced: Vec<String> = live_at_path
                    .query_map(params![c.path.as_str(), id], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                for old in &replaced {
                    mark_gone(tx, old, scan, seen_at)?;
                    out.disappeared += 1;
                }
                insert_item.execute(item_params)?;
                let previous = gone_at_path
                    .query_row(params![c.path.as_str(), category, id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })
                    .optional()?;
                if let Some((prev, prev_last_seen, gone_at)) = previous {
                    insert_regrowth.execute(params![
                        id,
                        prev,
                        c.path.as_str(),
                        category,
                        c.artifact,
                        prev_last_seen,
                        gone_at,
                        seen_at,
                        scan.as_str(),
                    ])?;
                    out.regrowth += 1;
                }
                out.new_items += 1;
                Some(ItemEventKind::FirstSeen)
            }
            Some(k) => {
                update_item.execute(item_params)?;
                out.seen_again += 1;
                if let Some(gone_at) = k.disappeared_at {
                    insert_regrowth.execute(params![
                        id,
                        id,
                        c.path.as_str(),
                        category,
                        c.artifact,
                        k.last_seen_at,
                        gone_at,
                        seen_at,
                        scan.as_str(),
                    ])?;
                    out.reappeared += 1;
                    out.regrowth += 1;
                    Some(ItemEventKind::Reappeared)
                } else {
                    let last: Option<(String, i64)> = last_event
                        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    let changed = match last {
                        None => true,
                        Some((t, alloc)) => {
                            t != tier || significant_change(uint(alloc), c.sizes.allocated)
                        }
                    };
                    changed.then_some(ItemEventKind::Changed)
                }
            }
        };
        if let Some(kind) = kind {
            insert_event(tx, id, seen_at, scan, kind, &tier, sizes)?;
        }
    }
    Ok(())
}

fn significant_change(before: u64, now: u64) -> bool {
    let diff = before.abs_diff(now);
    diff >= SIZE_CHANGE_MIN_BYTES && (diff as f64) >= (before as f64) * SIZE_CHANGE_FRACTION
}

fn insert_event(
    tx: &Transaction<'_>,
    item: &str,
    at: i64,
    scan: &ScanId,
    kind: ItemEventKind,
    tier: &str,
    sizes: [i64; 3],
) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO item_events (item_id, occurred_at, scan_id, kind, tier, logical_bytes, \
         allocated_bytes, reclaimable_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?
    .execute(params![
        item,
        at,
        scan.as_str(),
        kind.as_str(),
        tier,
        sizes[0],
        sizes[1],
        sizes[2]
    ])?;
    Ok(())
}

/// Mark a live item gone at `at` and record a `disappeared` event with its last known sizes.
fn mark_gone(tx: &Transaction<'_>, item: &str, scan: &ScanId, at: i64) -> Result<()> {
    tx.prepare_cached(
        "UPDATE items SET disappeared_at = ?2, disappeared_scan = ?3 \
         WHERE item_id = ?1 AND disappeared_at IS NULL",
    )?
    .execute(params![item, at, scan.as_str()])?;
    let (tier, sizes): (String, [i64; 3]) = tx
        .prepare_cached(
            "SELECT tier, logical_bytes, allocated_bytes, reclaimable_bytes FROM items \
             WHERE item_id = ?1",
        )?
        .query_row([item], |r| {
            Ok((r.get(0)?, [r.get(1)?, r.get(2)?, r.get(3)?]))
        })?;
    insert_event(tx, item, at, scan, ItemEventKind::Disappeared, &tier, sizes)
}

/// After a completed scan: every live item under one of the scan's roots that this scan did
/// not see (and that was last seen no later than the scan started) is gone.
fn mark_absent_items_gone(
    tx: &Transaction<'_>,
    scan: &ScanId,
    at: i64,
    out: &mut ScanRecorded,
) -> Result<()> {
    let (started_at, roots_json): (i64, String) = tx.query_row(
        "SELECT started_at, roots_json FROM scans WHERE scan_id = ?1",
        [scan.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let roots: Vec<FsPath> = serde_json::from_str(&roots_json).map_err(internal)?;
    if roots.is_empty() {
        return Ok(());
    }
    let roots: Vec<_> = roots.iter().map(FsPath::to_path_buf).collect();
    let live: Vec<(String, String)> = tx
        .prepare(
            "SELECT item_id, path FROM items \
             WHERE disappeared_at IS NULL AND last_seen_scan <> ?1 AND last_seen_at <= ?2",
        )?
        .query_map(params![scan.as_str(), started_at], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (item, path) in live {
        let p = Path::new(&path);
        if roots.iter().any(|root| p.starts_with(root)) {
            mark_gone(tx, &item, scan, at)?;
            out.disappeared += 1;
        }
    }
    Ok(())
}

// ---- row helpers ----------------------------------------------------------------------------

pub(crate) fn uint(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

pub(crate) fn bad(m: String) -> StoreError {
    StoreError::rejected(ErrorKind::Internal, m)
}

pub(crate) fn scan_id(s: String) -> Result<ScanId> {
    ScanId::try_from(s).map_err(bad)
}

pub(crate) fn item_id(s: String) -> Result<ItemId> {
    ItemId::try_from(s).map_err(bad)
}

pub(crate) fn tier_of(s: &str) -> Result<Tier> {
    match s {
        "S0" => Ok(Tier::S0),
        "S1" => Ok(Tier::S1),
        "S2" => Ok(Tier::S2),
        "S3" => Ok(Tier::S3),
        other => Err(bad(format!("unknown tier {other:?}"))),
    }
}

pub(crate) fn category_of(s: &str) -> Result<Category> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|e| bad(format!("unknown category {s:?}: {e}")))
}

fn status_of(s: &str) -> Result<ScanStatus> {
    ScanStatus::parse(s).ok_or_else(|| bad(format!("unexpected scan status {s:?}")))
}

fn volume_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<VolumeSample> {
    Ok(VolumeSample {
        volume_id: r.get(0)?,
        total: uint(r.get(1)?),
        free: uint(r.get(2)?),
        at: r.get(3)?,
    })
}
