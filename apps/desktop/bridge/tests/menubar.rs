//! What the menu bar widget reads from `status` (design 11, P4-UI-01): the ready-to-review plan
//! `scull check` announced (PRO-05) and the alerts it raised (PRD §16), from a real history
//! database in a temp dir. The answers validate against the committed `status` schema, and
//! reading never changes a byte of the database (SAFE-10).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_desktop_bridge::views::{MAX_STATUS_ALERTS, RECENT_ALERTS_SECS, StatusState};
use scull_desktop_bridge::{Session, Setup};
use scull_proactive::{Alert, AlertKind, Severity};
use scull_storage::{AlertEvent, DefaultPermissions, Event, EventKind, Store};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const NOW: i64 = 1_900_000_000;

fn validate(v: &Value) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../schemas/status.schema.json");
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "status: {errors:?}\n{v:#}");
}

fn session(data: &Path) -> Session {
    Session::new(Setup {
        home: None,
        config_file: None,
        data_dir: Some(data.to_path_buf()),
        env: Arc::new(|_| None),
        record_history: true,
        git: false,
        detect_synced_roots: false,
        allow_network_roots: true,
        in_use: false,
    })
}

fn store(data: &Path) -> Store {
    Store::open(&data.join(scull_storage::DB_FILE_NAME), &DefaultPermissions).unwrap()
}

fn plan_event(at: i64, expires_at: i64, file: &str) -> Event {
    Event {
        event_id: 0,
        at,
        kind: EventKind::ReviewPlan,
        source: "check".into(),
        payload: json!({
            "plan_id": "plan_01HZXMENUBAR",
            "file": file,
            "items": 14,
            "reclaimable_bytes": 41_446_948_454u64,
            "created_at": at,
            "expires_at": expires_at,
            "includes_s1": false,
        }),
    }
}

fn alert(kind: AlertKind, severity: Severity, subject: &str, message: &str, at: i64) -> AlertEvent {
    let a = Alert::new(kind, severity, subject, message, at);
    AlertEvent {
        event_id: 0,
        at,
        source: "check".into(),
        key: a.key.clone(),
        alert: serde_json::to_value(&a).unwrap(),
    }
}

/// Every file under `dir` with its bytes.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out: Vec<(PathBuf, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| (e.path(), std::fs::read(e.path()).unwrap_or_default()))
        .collect();
    out.sort();
    out
}

#[test]
fn status_carries_the_current_review_plan_and_recent_alerts() {
    let data = tempfile::tempdir().unwrap();
    {
        let mut s = store(data.path());
        s.record_events(&[plan_event(
            NOW - 600,
            NOW + 86_400,
            "/Users/ada/scull data/review-plan.json",
        )])
        .unwrap();
        s.record_alerts(&[
            // Too old: before the window.
            alert(
                AlertKind::LowSpace,
                Severity::Critical,
                "/",
                "old",
                NOW - RECENT_ALERTS_SECS - 1,
            ),
            // Raised twice: only the newest counts.
            alert(
                AlertKind::BudgetExceeded,
                Severity::Warning,
                "ml_cache",
                "Hugging Face cache over budget (first)",
                NOW - 7200,
            ),
            alert(
                AlertKind::BudgetExceeded,
                Severity::Warning,
                "ml_cache",
                "Hugging Face cache over budget",
                NOW - 600,
            ),
            alert(
                AlertKind::WorktreeTtlExpired,
                Severity::Critical,
                "/Users/ada/code/.worktrees",
                "22 agent worktrees past their TTL",
                NOW - 600,
            ),
        ])
        .unwrap();
    }
    let before = snapshot(data.path());

    let st = session(data.path()).status(NOW);
    assert_eq!(st.state, StatusState::NoScan, "{st:?}");
    let plan = st.review_plan.as_ref().expect("a current plan");
    assert_eq!(plan.plan_id, "plan_01HZXMENUBAR");
    assert_eq!(plan.items, 14);
    assert_eq!(plan.reclaimable_bytes, 41_446_948_454);
    assert_eq!(
        plan.review_command,
        "scull clean --plan \"/Users/ada/scull data/review-plan.json\""
    );
    let messages: Vec<&str> = st.alerts.iter().map(|a| a.message.as_str()).collect();
    assert_eq!(
        messages,
        [
            "22 agent worktrees past their TTL",
            "Hugging Face cache over budget"
        ],
        "newest per key, most severe first, old ones left out"
    );
    validate(&serde_json::to_value(&st).unwrap());

    assert_eq!(snapshot(data.path()), before, "reading changed nothing");
}

#[test]
fn expired_or_withdrawn_plans_and_bad_payloads_are_left_out() {
    let data = tempfile::tempdir().unwrap();
    {
        let mut s = store(data.path());
        s.record_events(&[plan_event(NOW - 90_000, NOW - 1, "/tmp-free/plan.json")])
            .unwrap();
    }
    let st = session(data.path()).status(NOW);
    assert_eq!(st.review_plan, None, "expired");

    {
        let mut s = store(data.path());
        s.record_events(&[plan_event(NOW - 60, NOW + 86_400, "/x/plan.json")])
            .unwrap();
        // A later check found nothing eligible and withdrew it.
        s.record_events(&[Event {
            event_id: 0,
            at: NOW - 30,
            kind: EventKind::ReviewPlan,
            source: "check".into(),
            payload: json!({ "withdrawn_at": NOW - 30 }),
        }])
        .unwrap();
        // An alert payload this build doesn't understand is skipped, not an error.
        s.record_alerts(&[AlertEvent {
            event_id: 0,
            at: NOW - 30,
            source: "check".into(),
            key: "future_kind:/".into(),
            alert: json!({ "kind": "future_kind", "subject": "/" }),
        }])
        .unwrap();
    }
    let st = session(data.path()).status(NOW);
    assert_eq!(st.review_plan, None, "withdrawn");
    assert!(st.alerts.is_empty(), "{:?}", st.alerts);
    validate(&serde_json::to_value(&st).unwrap());
}

#[test]
fn at_most_five_alerts() {
    let data = tempfile::tempdir().unwrap();
    {
        let mut s = store(data.path());
        let alerts: Vec<AlertEvent> = (0..8)
            .map(|i| {
                alert(
                    AlertKind::Stale,
                    Severity::Info,
                    &format!("/Users/ada/code/p{i}"),
                    &format!("p{i} idle"),
                    NOW - 100 + i,
                )
            })
            .collect();
        s.record_alerts(&alerts).unwrap();
    }
    let st = session(data.path()).status(NOW);
    assert_eq!(st.alerts.len(), MAX_STATUS_ALERTS);
    // Same severity: newest first.
    assert_eq!(st.alerts[0].message, "p7 idle");
}

#[test]
fn no_database_has_no_plan_and_no_alerts() {
    let data = tempfile::tempdir().unwrap();
    let st = session(data.path()).status(NOW);
    assert_eq!(st.state, StatusState::NoDatabase);
    assert_eq!(st.review_plan, None);
    assert!(st.alerts.is_empty());
    assert!(
        std::fs::read_dir(data.path()).unwrap().next().is_none(),
        "status creates nothing"
    );
}
