//! Reads and the purge behind local insights (INS-02, INS-03; `scull insights`).
//!
//! The insights engine (`scull-insights`) computes everything from these typed reads plus the
//! history queries in [`crate::history`]; nothing outside this crate writes SQL (DATA-01).
//!
//! * [`Store::item_timelines`]: every candidate alive at some point since a time, with the
//!   events needed to know its size at any moment of the window (growth, ecosystem mix,
//!   duplication, agent footprint).
//! * [`Store::operation_accuracy`]: predicted vs. measured bytes per cleanup operation, read
//!   from the append-only audit (cleanup effectiveness, G3).
//! * [`Store::purge_insights`]: forgets recorded trends (`scull insights purge`).
//!
//! # What `purge_insights` deletes
//!
//! Only rows of the history streams, which are not append-only (see `v002_history.sql`):
//! free-space samples, footprint samples, item events and regrowth events, plus the rows of
//! items that are **gone** (marked disappeared), whose whole lifecycle is history. It keeps:
//!
//! * live items (they are what the latest scan found; the next sighting records a fresh
//!   baseline event for each);
//! * scans and their rollups (scan records; `scull data purge --all` removes those);
//! * the per-volume `volumes` cache, so `scull status --short` keeps working;
//! * plans, approvals, operations and audit (append-only, enforced by triggers) and the
//!   quarantine manifest.
//!
//! No file is deleted: this is row deletion inside scull's own database.

use crate::error::Result;
use crate::history::{ItemEvent, ItemEventKind, bad, category_of, item_id, scan_id, tier_of, uint};
use crate::store::Store;
use rusqlite::params;
use scull_core::{Category, FsPath, ItemId, Tier};
use std::collections::HashMap;

/// A candidate's lifecycle with the events needed to know its size at any time since the
/// `since` given to [`Store::item_timelines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemTimeline {
    /// The item.
    pub item_id: ItemId,
    /// Its path.
    pub path: FsPath,
    /// Its category.
    pub category: Category,
    /// Artifact type, e.g. "Installed npm dependencies".
    pub artifact: String,
    /// Ecosystem of its project, e.g. "node", if it belongs to one.
    pub ecosystem: Option<String>,
    /// Root of its project or repository, if it belongs to one.
    pub project_root: Option<FsPath>,
    /// Tier at the last sighting.
    pub tier: Tier,
    /// First sighting.
    pub first_seen_at: i64,
    /// Last sighting.
    pub last_seen_at: i64,
    /// When it was marked gone, if it is gone.
    pub disappeared_at: Option<i64>,
    /// Allocated bytes at the last sighting.
    pub allocated_bytes: u64,
    /// Reclaimable bytes at the last sighting.
    pub reclaimable_bytes: u64,
    /// Events, oldest first: the last one at or before `since` (if any), then every later one.
    /// May be empty after `purge_insights` until the item is seen again.
    pub events: Vec<ItemEvent>,
}

impl ItemTimeline {
    /// Allocated bytes the item held at time `t`: the size of the last event at or before `t`
    /// (zero after a `disappeared` event, and before the item was first seen). When history
    /// before `t` was purged or thinned away but the item already existed, the oldest event
    /// left (or the last sighting) stands in for it.
    pub fn allocated_at(&self, t: i64) -> u64 {
        if !self.alive_at(t) {
            return 0;
        }
        match self.events.iter().rev().find(|e| e.at <= t) {
            Some(e) => e.allocated_bytes,
            None => self
                .events
                .first()
                .filter(|e| e.kind != ItemEventKind::Disappeared)
                .map_or(self.allocated_bytes, |e| e.allocated_bytes),
        }
    }

    /// Whether the item was present (first seen, and not marked gone) at time `t`.
    pub fn alive_at(&self, t: i64) -> bool {
        if t < self.first_seen_at {
            return false;
        }
        match self.events.iter().rev().find(|e| e.at <= t) {
            Some(e) => e.kind != ItemEventKind::Disappeared,
            None => self.disappeared_at.is_none_or(|d| d > t),
        }
    }
}

/// Predicted vs. measured bytes of one cleanup operation, from its audit rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationAccuracy {
    /// The operation.
    pub operation_id: String,
    /// When it started.
    pub started_at: i64,
    /// Time of its last audit row.
    pub finished_at: i64,
    /// Audit rows with both a predicted and a measured size.
    pub items_compared: u64,
    /// Sum of predicted bytes over those rows.
    pub predicted_bytes: u64,
    /// Sum of measured bytes over those rows.
    pub measured_bytes: u64,
    /// Items of the operation that were later rebuilt (a regrowth event names them as the
    /// previous item): the rebuild cost paid afterwards.
    pub regrown_items: u64,
}

/// What [`Store::purge_insights`] deleted (rows per stream).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InsightsPurged {
    /// Free-space samples.
    pub free_space_samples: u64,
    /// Footprint samples.
    pub footprint_samples: u64,
    /// Item lifecycle events.
    pub item_events: u64,
    /// Regrowth events.
    pub regrowth_events: u64,
    /// Rows of gone items.
    pub gone_items: u64,
}

impl InsightsPurged {
    /// Rows deleted in all.
    pub fn total(&self) -> u64 {
        self.free_space_samples
            + self.footprint_samples
            + self.item_events
            + self.regrowth_events
            + self.gone_items
    }
}

type ItemRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    [i64; 4],
    Option<i64>,
);

impl Store {
    /// Every item present at some point at or after `since` (live, or marked gone at or after
    /// `since`), sorted by path then ID, each with the last event at or before `since` and all
    /// later events. Two indexed queries.
    pub fn item_timelines(&self, since: i64) -> Result<Vec<ItemTimeline>> {
        self.read(|conn| {
            let mut events: HashMap<String, Vec<ItemEvent>> = HashMap::new();
            let mut stmt = conn.prepare(
                "SELECT e.item_id, e.occurred_at, e.scan_id, e.kind, e.tier, e.logical_bytes, \
                 e.allocated_bytes, e.reclaimable_bytes \
                 FROM items i JOIN item_events e ON e.item_id = i.item_id \
                 WHERE (i.disappeared_at IS NULL OR i.disappeared_at >= ?1) \
                   AND e.event_id >= coalesce((SELECT max(b.event_id) FROM item_events b \
                                               WHERE b.item_id = i.item_id \
                                                 AND b.occurred_at <= ?1), 0) \
                 ORDER BY e.item_id, e.event_id",
            )?;
            let rows = stmt.query_map([since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    [
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                    ],
                ))
            })?;
            for row in rows {
                let (item, at, scan, kind, tier, v) = row?;
                events.entry(item).or_default().push(ItemEvent {
                    at,
                    scan_id: scan_id(scan)?,
                    kind: parse_kind(&kind)?,
                    tier: tier_of(&tier)?,
                    logical_bytes: uint(v[0]),
                    allocated_bytes: uint(v[1]),
                    reclaimable_bytes: uint(v[2]),
                });
            }

            let mut stmt = conn.prepare(
                "SELECT item_id, path, category, artifact, ecosystem, project_root, tier, \
                 first_seen_at, last_seen_at, allocated_bytes, reclaimable_bytes, \
                 disappeared_at FROM items \
                 WHERE disappeared_at IS NULL OR disappeared_at >= ?1 \
                 ORDER BY path, item_id",
            )?;
            let rows = stmt.query_map([since], |r| -> rusqlite::Result<ItemRow> {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    [r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?],
                    r.get(11)?,
                ))
            })?;
            rows.map(|row| {
                let (id, path, category, artifact, ecosystem, project, tier, v, gone) = row?;
                let events = events.remove(&id).unwrap_or_default();
                Ok(ItemTimeline {
                    item_id: item_id(id)?,
                    path: FsPath::from(path.as_str()),
                    category: category_of(&category)?,
                    artifact,
                    ecosystem: ecosystem.filter(|e| !e.is_empty()),
                    project_root: project.map(|p| FsPath::from(p.as_str())),
                    tier: tier_of(&tier)?,
                    first_seen_at: v[0],
                    last_seen_at: v[1],
                    disappeared_at: gone,
                    allocated_bytes: uint(v[2]),
                    reclaimable_bytes: uint(v[3]),
                    events,
                })
            })
            .collect()
        })
    }

    /// Predicted vs. measured bytes of every operation started at or after `since` that has at
    /// least one audit row with both sizes, oldest first.
    ///
    /// Compared rows are item outcomes (`item`, and `legacy` rows from before the hash chain,
    /// whose v1 `measured_bytes` became `freed_bytes`): the plan's predicted bytes against the
    /// bytes actually freed (a removal, or a tool run's cache shrink). Moves to quarantine or the
    /// Trash free nothing yet and have no freed bytes, so they are not compared.
    pub fn operation_accuracy(&self, since: i64) -> Result<Vec<OperationAccuracy>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT o.operation_id, o.started_at, max(a.recorded_at), count(*), \
                 sum(a.predicted_bytes), sum(a.freed_bytes), \
                 (SELECT count(DISTINCT a2.item_id) FROM audit a2 \
                  WHERE a2.operation_id = o.operation_id AND a2.item_id IS NOT NULL \
                    AND a2.kind IN ('item', 'legacy') \
                    AND EXISTS (SELECT 1 FROM regrowth_events r \
                                WHERE r.previous_item_id = a2.item_id \
                                  AND r.item_id <> r.previous_item_id \
                                  AND r.reappeared_at >= a2.recorded_at)) \
                 FROM operations o JOIN audit a ON a.operation_id = o.operation_id \
                 WHERE o.started_at >= ?1 AND a.kind IN ('item', 'legacy') \
                   AND a.predicted_bytes IS NOT NULL AND a.freed_bytes IS NOT NULL \
                 GROUP BY o.operation_id ORDER BY o.started_at, o.operation_id",
            )?;
            let rows = stmt.query_map([since], |r| {
                Ok(OperationAccuracy {
                    operation_id: r.get(0)?,
                    started_at: r.get(1)?,
                    finished_at: r.get(2)?,
                    items_compared: uint(r.get(3)?),
                    predicted_bytes: uint(r.get(4)?),
                    measured_bytes: uint(r.get(5)?),
                    regrown_items: uint(r.get(6)?),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Forget recorded trends (`scull insights purge`): delete history rows older than
    /// `before` (Unix seconds), or all of them with `None`. See the [module
    /// docs](crate::insights) for exactly what is deleted and what is kept. One transaction.
    pub fn purge_insights(&mut self, before: Option<i64>) -> Result<InsightsPurged> {
        let t = before.unwrap_or(i64::MAX);
        self.write(|tx| {
            const GONE: &str =
                "SELECT item_id FROM items WHERE disappeared_at IS NOT NULL AND disappeared_at < ?1";
            let regrowth_events = tx.execute(
                &format!(
                    "DELETE FROM regrowth_events WHERE reappeared_at < ?1 \
                     OR item_id IN ({GONE}) OR previous_item_id IN ({GONE})"
                ),
                params![t],
            )? as u64;
            let item_events = tx.execute(
                &format!("DELETE FROM item_events WHERE occurred_at < ?1 OR item_id IN ({GONE})"),
                params![t],
            )? as u64;
            let gone_items = tx.execute(
                "DELETE FROM items WHERE disappeared_at IS NOT NULL AND disappeared_at < ?1",
                params![t],
            )? as u64;
            let free_space_samples =
                tx.execute("DELETE FROM free_space_samples WHERE sampled_at < ?1", params![t])?
                    as u64;
            let footprint_samples =
                tx.execute("DELETE FROM footprint_samples WHERE sampled_at < ?1", params![t])?
                    as u64;
            Ok(InsightsPurged {
                free_space_samples,
                footprint_samples,
                item_events,
                regrowth_events,
                gone_items,
            })
        })
    }

    /// Every volume ever sampled (from the per-volume cache), sorted.
    pub fn volume_ids(&self) -> Result<Vec<String>> {
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT volume FROM volumes ORDER BY volume")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
}

fn parse_kind(s: &str) -> Result<ItemEventKind> {
    match s {
        "first_seen" => Ok(ItemEventKind::FirstSeen),
        "changed" => Ok(ItemEventKind::Changed),
        "disappeared" => Ok(ItemEventKind::Disappeared),
        "reappeared" => Ok(ItemEventKind::Reappeared),
        other => Err(bad(format!("unknown item event kind {other:?}"))),
    }
}
