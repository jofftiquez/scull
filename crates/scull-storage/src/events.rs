//! The `events` stream (INS-06 "Alerts and responses"): alerts raised by `scull check` and
//! other timestamped facts that are not scans, plans or audit rows.
//!
//! Each row is `(occurred_at, kind, source, payload_json)`. The payload is the caller's JSON
//! object; this crate stores it as given and only requires it to be an object. It must
//! follow INS-07 (paths, categories, sizes, counts and times; never file contents,
//! environment values or secrets).
//!
//! Alerts ([`EventKind::Alert`]) carry their stable key next to the payload so repeated
//! checks can be de-duplicated and responses recorded against them later. Events are not
//! append-only: retention (DATA-06) may thin them.

use crate::error::{Result, StoreError};
use crate::store::{Store, internal};
use rusqlite::{OptionalExtension, params};
use scull_core::{ErrorKind, FsPath, ScanId};

/// What an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// An alert raised by a check (PRD §16).
    Alert,
    /// A reclaim plan prepared for review (PRO-05); never executed by itself.
    ReviewPlan,
}

impl EventKind {
    /// The stored name: `alert`, `review_plan`.
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Alert => "alert",
            EventKind::ReviewPlan => "review_plan",
        }
    }
}

/// One recorded event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Row id (assigned by the store; ignored when recording).
    pub event_id: i64,
    /// When it happened (Unix seconds).
    pub at: i64,
    /// What it is.
    pub kind: EventKind,
    /// Which interface recorded it: `check`, `cli`, `desktop`, `mcp`.
    pub source: String,
    /// The caller's JSON object.
    pub payload: serde_json::Value,
}

/// An alert as recorded: its stable key (`<kind>:<subject>`) and the full alert as JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertEvent {
    /// Row id (assigned by the store; ignored when recording).
    pub event_id: i64,
    /// When it was raised (Unix seconds).
    pub at: i64,
    /// Which interface raised it.
    pub source: String,
    /// The alert's stable key.
    pub key: String,
    /// The alert itself (a JSON object).
    pub alert: serde_json::Value,
}

fn require_object(v: &serde_json::Value) -> Result<()> {
    if v.is_object() {
        Ok(())
    } else {
        Err(StoreError::rejected(
            ErrorKind::Usage,
            "an event payload must be a JSON object",
        ))
    }
}

impl Store {
    /// The roots of scan `scan_id` as recorded (in the order given at its start), or `None`
    /// when there is no such scan.
    pub fn scan_roots(&self, scan_id: &ScanId) -> Result<Option<Vec<FsPath>>> {
        self.read(|conn| {
            conn.query_row(
                "SELECT roots_json FROM scans WHERE scan_id = ?1",
                [scan_id.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|json| serde_json::from_str(&json).map_err(internal))
            .transpose()
        })
    }

    /// Record events in one transaction. `event_id` is ignored.
    pub fn record_events(&mut self, events: &[Event]) -> Result<()> {
        for e in events {
            require_object(&e.payload)?;
        }
        self.write(|tx| {
            let mut stmt = tx.prepare(
                "INSERT INTO events (occurred_at, kind, source, payload_json) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in events {
                let json = serde_json::to_string(&e.payload).map_err(internal)?;
                stmt.execute(params![e.at, e.kind.as_str(), e.source, json])?;
            }
            Ok(())
        })
    }

    /// Events of `kind` that happened at or after `since`, oldest first.
    pub fn events(&self, kind: EventKind, since: i64) -> Result<Vec<Event>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT event_id, occurred_at, source, payload_json FROM events \
                 WHERE kind = ?1 AND occurred_at >= ?2 ORDER BY occurred_at, event_id",
            )?;
            let rows = stmt.query_map(params![kind.as_str(), since], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            rows.map(|row| {
                let (event_id, at, source, json) = row?;
                Ok(Event {
                    event_id,
                    at,
                    kind,
                    source,
                    payload: serde_json::from_str(&json).map_err(internal)?,
                })
            })
            .collect()
        })
    }

    /// The most recent event of `kind` (latest time, then latest row), if any.
    pub fn latest_event(&self, kind: EventKind) -> Result<Option<Event>> {
        self.read(|conn| {
            conn.query_row(
                "SELECT event_id, occurred_at, source, payload_json FROM events \
                 WHERE kind = ?1 ORDER BY occurred_at DESC, event_id DESC LIMIT 1",
                [kind.as_str()],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
            .map(|(event_id, at, source, json)| {
                Ok(Event {
                    event_id,
                    at,
                    kind,
                    source,
                    payload: serde_json::from_str(&json).map_err(internal)?,
                })
            })
            .transpose()
        })
    }

    /// Record one alert (see [`Store::record_alerts`]).
    pub fn record_alert(&mut self, alert: &AlertEvent) -> Result<()> {
        self.record_alerts(std::slice::from_ref(alert))
    }

    /// Record alerts as [`EventKind::Alert`] events, in one transaction. Each payload is
    /// `{"key": <key>, "alert": <alert>}`; `alert` must be a JSON object.
    pub fn record_alerts(&mut self, alerts: &[AlertEvent]) -> Result<()> {
        let events: Vec<Event> = alerts
            .iter()
            .map(|a| Event {
                event_id: 0,
                at: a.at,
                kind: EventKind::Alert,
                source: a.source.clone(),
                payload: serde_json::json!({ "key": a.key, "alert": a.alert }),
            })
            .collect();
        for a in alerts {
            require_object(&a.alert)?;
        }
        self.record_events(&events)
    }

    /// Alerts raised at or after `since`, oldest first.
    pub fn alerts(&self, since: i64) -> Result<Vec<AlertEvent>> {
        self.events(EventKind::Alert, since)?
            .into_iter()
            .map(|e| {
                let key = e
                    .payload
                    .get("key")
                    .and_then(|k| k.as_str())
                    .ok_or_else(|| {
                        StoreError::rejected(ErrorKind::Internal, "alert event without a key")
                    })?;
                Ok(AlertEvent {
                    event_id: e.event_id,
                    at: e.at,
                    source: e.source.clone(),
                    key: key.to_owned(),
                    alert: e
                        .payload
                        .get("alert")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DefaultPermissions, ScanStart};
    use serde_json::json;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = Store::open(&dir.path().join("scull.db"), &DefaultPermissions).expect("open");
        (dir, s)
    }

    #[test]
    fn scan_roots_round_trip_in_order() {
        let (_d, mut s) = store();
        let id = ScanId::from_suffix("r1").expect("id");
        let roots = vec![FsPath::from("/z/b"), FsPath::from("/a/c")];
        s.record_scan_start(&ScanStart {
            scan_id: id.clone(),
            started_at: 10,
            roots: roots.clone(),
        })
        .expect("start");
        assert_eq!(s.scan_roots(&id).expect("roots"), Some(roots));
        let other = ScanId::from_suffix("nope").expect("id");
        assert_eq!(s.scan_roots(&other).expect("roots"), None);
    }

    #[test]
    fn alerts_are_recorded_and_read_back_by_time() {
        let (_d, mut s) = store();
        s.record_alerts(&[
            AlertEvent {
                event_id: 0,
                at: 200,
                source: "check".into(),
                key: "stale:cache".into(),
                alert: json!({"kind": "stale", "bytes": 7}),
            },
            AlertEvent {
                event_id: 0,
                at: 100,
                source: "check".into(),
                key: "low_space:/".into(),
                alert: json!({"kind": "low_space"}),
            },
        ])
        .expect("record");
        s.record_alert(&AlertEvent {
            event_id: 0,
            at: 300,
            source: "cli".into(),
            key: "growth_spike:container".into(),
            alert: json!({"kind": "growth_spike"}),
        })
        .expect("record one");
        let all = s.alerts(0).expect("alerts");
        let keys: Vec<&str> = all.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["low_space:/", "stale:cache", "growth_spike:container"]
        );
        assert_eq!(all[1].alert["bytes"], 7);
        assert_eq!(all[2].source, "cli");
        assert!(all.iter().all(|a| a.event_id > 0));
        let recent = s.alerts(150).expect("since");
        assert_eq!(recent.len(), 2);
        // Alerts are not other events.
        assert!(
            s.events(EventKind::ReviewPlan, 0)
                .expect("events")
                .is_empty()
        );
    }

    #[test]
    fn non_object_payloads_are_rejected_and_latest_event_wins() {
        let (_d, mut s) = store();
        let bad = AlertEvent {
            event_id: 0,
            at: 1,
            source: "check".into(),
            key: "k".into(),
            alert: json!("text"),
        };
        assert!(s.record_alert(&bad).is_err());
        assert!(s.alerts(0).expect("alerts").is_empty());
        assert_eq!(s.latest_event(EventKind::ReviewPlan).expect("latest"), None);
        for (at, id) in [(5, "a"), (9, "b"), (9, "c"), (7, "d")] {
            s.record_events(&[Event {
                event_id: 0,
                at,
                kind: EventKind::ReviewPlan,
                source: "check".into(),
                payload: json!({ "plan_id": id }),
            }])
            .expect("record");
        }
        let latest = s
            .latest_event(EventKind::ReviewPlan)
            .expect("latest")
            .expect("some");
        assert_eq!(latest.payload["plan_id"], "c");
        assert_eq!(latest.at, 9);
        let since = s.events(EventKind::ReviewPlan, 7).expect("events");
        assert_eq!(since.len(), 3);
    }
}
