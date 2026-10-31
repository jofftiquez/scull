//! Policies evaluated over history recorded through `scull-storage` (P4-PRO-02): budgets
//! from category rollups, category growth spikes, free-space drops.

use scull_core::config::{Budget, ProactiveConfig};
use scull_core::{Category, FsPath, ScanId, Tier};
use scull_proactive::alert::AlertKind;
use scull_proactive::forecast::GIB;
use scull_proactive::policy::category_spike_alert;
use scull_proactive::{budget_alerts, category_series, category_usage, free_space_drop_alerts};
use scull_storage::{
    DefaultPermissions, Rollup, ScanFinish, ScanRecord, ScanStart, ScanStatus, ScanTotals, Store,
    VolumeSample,
};

const NOW: i64 = 1_800_000_000;
const HOUR: i64 = 3_600;

/// Record a completed scan of the same root finishing `hours_ago`, with `container` and
/// `artifacts` GiB, and a free-space sample of `/`.
fn scan(store: &mut Store, n: u32, hours_ago: i64, container: u64, artifacts: u64, free: u64) {
    let at = NOW - hours_ago * HOUR;
    let scan_id = ScanId::from_suffix(&format!("p{n}")).expect("id");
    store
        .record_scan_start(&ScanStart {
            scan_id: scan_id.clone(),
            started_at: at - 60,
            roots: vec![FsPath::from("/home/u")],
        })
        .expect("start");
    let rollup = |category, gib: u64| Rollup {
        category,
        tier: Tier::S0,
        ecosystem: None,
        items: 1,
        logical_bytes: gib * GIB,
        allocated_bytes: gib * GIB,
        reclaimable_bytes: gib * GIB,
    };
    store
        .record_scan(&ScanRecord {
            finish: ScanFinish {
                scan_id,
                finished_at: at,
                status: ScanStatus::Completed,
                totals: ScanTotals {
                    entries_scanned: 1,
                    candidates: 2,
                    logical_bytes: 0,
                    allocated_bytes: 0,
                    reclaimable_bytes: 0,
                    errors: 0,
                },
                rollups: vec![
                    rollup(Category::Container, container),
                    rollup(Category::ProjectArtifact, artifacts),
                ],
            },
            candidates: &[],
            volumes: &[VolumeSample {
                volume_id: "/".into(),
                total: 1000 * GIB,
                free: free * GIB,
                at,
            }],
        })
        .expect("scan");
}

fn store(dir: &std::path::Path) -> Store {
    let mut s = Store::open(&dir.join("scull.db"), &DefaultPermissions).expect("open");
    scan(&mut s, 0, 72, 40, 60, 300);
    scan(&mut s, 1, 22, 42, 61, 296);
    scan(&mut s, 2, 1, 70, 62, 266);
    s
}

#[test]
fn alerts_fire_over_recorded_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let s = store(dir.path());
    let cfg = ProactiveConfig::default();

    // Budgets from the latest scan's category totals.
    let usage = category_usage(&s, NOW).expect("usage");
    assert_eq!(usage.len(), 2);
    let budgets = vec![
        Budget {
            category: Some(Category::Container),
            ecosystem: None,
            path: None,
            max_gib: 50,
        },
        Budget {
            category: Some(Category::ProjectArtifact),
            ecosystem: None,
            path: None,
            max_gib: 100,
        },
    ];
    let a = budget_alerts(&budgets, &usage, NOW);
    assert_eq!(a.len(), 1, "{a:#?}");
    assert_eq!(a[0].kind, AlertKind::BudgetExceeded);
    assert_eq!(
        a[0].message,
        "container uses 70.0 GiB, over its 50.0 GiB budget"
    );

    // Docker grew 28 GiB in the last 21 hours; project artifacts barely moved.
    let docker = category_series(&s, Category::Container, NOW - 7 * 24 * HOUR).expect("series");
    assert_eq!(docker.len(), 3);
    let a = category_spike_alert(Category::Container, &docker, Vec::new(), &cfg, NOW)
        .expect("docker spike");
    assert_eq!(a.kind, AlertKind::GrowthSpike);
    assert_eq!(a.bytes, Some(28 * GIB));
    let artifacts =
        category_series(&s, Category::ProjectArtifact, NOW - 7 * 24 * HOUR).expect("series");
    assert!(
        category_spike_alert(Category::ProjectArtifact, &artifacts, Vec::new(), &cfg, NOW)
            .is_none()
    );

    // The same 30 GiB showed up as a free-space drop on `/`.
    let a = free_space_drop_alerts(&s, &cfg, NOW).expect("drops");
    assert_eq!(a.len(), 1, "{a:#?}");
    assert_eq!(a[0].kind, AlertKind::FreeSpaceDrop);
    assert_eq!(a[0].subject, "/");
    assert_eq!(a[0].bytes, Some(30 * GIB));

    // A higher threshold silences both.
    let mut quiet = cfg.clone();
    quiet.spike_gib = 40;
    quiet.spike_percent = 0;
    assert!(
        free_space_drop_alerts(&s, &quiet, NOW)
            .expect("drops")
            .is_empty()
    );
    assert!(category_spike_alert(Category::Container, &docker, Vec::new(), &quiet, NOW).is_none());
}
