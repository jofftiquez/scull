//! The Insights history commands (P3-UI-10) over a real history database in a temp dir:
//! `free_space_history`, `rollup_history`, `regrowth`, `footprint_history`. They only read:
//! a missing database stays missing, and the database file is not modified by a read.
//!
//! The recorded history follows design 05 (free space 96 GiB → 41.2 GiB over 30 days with two
//! cleanups and −3.5 GiB/day since the last one, `shop-api/node_modules` rebuilt four times, six months of category rollups). With
//! `SCULL_UPDATE_MOCKS=1` the payloads are written to `apps/desktop/src/bridge/fixtures/` for
//! the mock bridge, which shifts their timestamps so the latest sample is "now". Paths are
//! imagined (`/Users/ada/...`): the store records strings, nothing is touched.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{
    Action, Candidate, Category, FileIdentity, FsPath, ItemId, ReasonCode, Reclaimable,
    Regeneration, RuleRef, ScanId, Sizes, Tier, TierReason,
};
use scull_desktop_bridge::insights::HistoryRead;
use scull_desktop_bridge::{Session, Setup};
use scull_storage::{
    ScanFinish, ScanRecord, ScanStart, ScanStatus, ScanTotals, Store, VolumeSample,
    rollups_from_candidates,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DAY: i64 = 86_400;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// "Today" of the recorded history (2026-10-31 12:00 UTC).
const NOW: i64 = 1_793_448_000;
const VOLUME: &str = "/";

fn desktop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn validate(stem: &str, v: &Value) {
    let path = desktop()
        .join("schemas")
        .join(format!("{stem}.schema.json"));
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator.iter_errors(v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{stem}: {errors:?}\n{v:#}");
}

fn setup(data: &Path) -> Setup {
    Setup {
        home: None,
        config_file: None,
        data_dir: Some(data.to_path_buf()),
        env: Arc::new(|_| None),
        record_history: true,
        git: false,
        detect_synced_roots: false,
        allow_network_roots: true,
        in_use: false,
    }
}

fn gib(x: f64) -> u64 {
    (x * GIB).round() as u64
}

/// A candidate at `path` with file ID `file` (a new file ID at the same path is a rebuild).
fn cand(path: &str, file: u128, category: Category, tier: Tier, bytes: u64) -> Candidate {
    let identity = FileIdentity::new(7, file);
    Candidate {
        id: ItemId::derive(identity, path.as_bytes()),
        path: FsPath::from(path),
        identity,
        category,
        artifact: match category {
            Category::ProjectArtifact => "npm dependencies",
            Category::MlCache => "Hugging Face cache",
            Category::PackageCache => "package cache",
            _ => "other",
        }
        .into(),
        rule: RuleRef {
            id: "insights.fixture".into(),
            version: 1,
        },
        project: None,
        tier,
        reasons: vec![TierReason::new(tier, ReasonCode::Regenerable, "fixture")],
        evidence: vec![],
        sizes: Sizes {
            logical: bytes,
            allocated: bytes,
            reclaimable: Reclaimable {
                bytes,
                confidence: scull_core::Confidence::High,
                shared_bytes: 0,
            },
        },
        regeneration: Regeneration::default(),
        action: Action::Delete,
        flags: Default::default(),
    }
}

fn record(s: &mut Store, n: u32, at: i64, cands: &[Candidate], free: Option<u64>) {
    let id = ScanId::from_suffix(&format!("I{n}")).unwrap();
    s.record_scan_start(&ScanStart {
        scan_id: id.clone(),
        started_at: at - 60,
        roots: vec![FsPath::from("/Users/ada/code")],
    })
    .unwrap();
    let sum = |f: fn(&Candidate) -> u64| cands.iter().map(f).sum::<u64>();
    let volumes: Vec<VolumeSample> = free
        .map(|free| VolumeSample {
            volume_id: VOLUME.into(),
            total: gib(494.0),
            free,
            at,
        })
        .into_iter()
        .collect();
    s.record_scan(&ScanRecord {
        finish: ScanFinish {
            scan_id: id,
            finished_at: at,
            status: ScanStatus::Completed,
            totals: ScanTotals {
                entries_scanned: 1000,
                candidates: cands.len() as u64,
                logical_bytes: sum(|c| c.sizes.logical),
                allocated_bytes: sum(|c| c.sizes.allocated),
                reclaimable_bytes: sum(|c| c.sizes.reclaimable.bytes),
                errors: 0,
            },
            rollups: rollups_from_candidates(cands).unwrap(),
        },
        candidates: cands,
        volumes: &volumes,
    })
    .unwrap();
}

/// Category sizes (GiB) at month `m` (0 = six months ago, 5 = this month): ML grows,
/// build outputs shrink, like design 05's mix.
fn month_mix(m: usize) -> [(Category, &'static str, f64); 4] {
    let ml = [12.0, 15.0, 19.0, 24.0, 29.0, 34.0];
    let js = [41.0, 39.0, 35.0, 31.0, 27.0, 24.0];
    let caches = [27.0, 27.0, 27.0, 26.0, 25.0, 24.0];
    let other = [20.0, 19.0, 19.0, 19.0, 19.0, 18.0];
    let scale = [0.62, 0.66, 0.72, 0.8, 0.9, 1.0][m];
    [
        (
            Category::MlCache,
            "/Users/ada/.cache/huggingface",
            ml[m] * scale,
        ),
        (
            Category::ProjectArtifact,
            "/Users/ada/code/web/node_modules",
            js[m] * scale,
        ),
        (
            Category::PackageCache,
            "/Users/ada/.cargo/registry",
            caches[m] * scale,
        ),
        (
            Category::Other,
            "/Users/ada/code/misc/.tox",
            other[m] * scale,
        ),
    ]
}

/// Six monthly scans, then 31 daily scans with free-space samples. `shop-api/node_modules`
/// alternates gone / rebuilt (new file ID) four times in the last 30 days.
fn seed(data: &Path) {
    std::fs::create_dir_all(data).unwrap();
    let mut s = scull_engine::open_history(data).unwrap();
    let mut n = 0;
    for m in 0..5 {
        n += 1;
        // May 18 … Sep 20: one scan in each of the five months before October.
        let at = NOW - (5 - m as i64) * 31 * DAY - 10 * DAY;
        let cands: Vec<_> = month_mix(m)
            .iter()
            .enumerate()
            .map(|(i, (c, p, g))| cand(p, 1 + i as u128, *c, Tier::S1, gib(*g)))
            .collect();
        record(&mut s, n, at, &cands, None);
    }
    let shop = "/Users/ada/code/shop-api/node_modules";
    let mut free = 96.0;
    for day in 0..=30i64 {
        n += 1;
        if day == 8 {
            free += 22.0;
        }
        if day == 19 {
            free += 9.0;
        }
        let at = NOW - (30 - day) * DAY;
        let mut cands: Vec<_> = month_mix(5)
            .iter()
            .enumerate()
            .map(|(i, (c, p, g))| {
                // ML caches drive most of the growth (design 05: "71% of growth").
                let per_day = [0.25, 0.05, 0.03, 0.02][i];
                let g = g - (30 - day) as f64 * per_day;
                cand(p, 1 + i as u128, *c, Tier::S1, gib(g.max(1.0)))
            })
            .collect();
        // Present on days 0-1, gone on 2, back on 4 (rebuild), gone on 8, back on 10, ...
        let cycle = [(2, 4), (8, 10), (15, 17), (22, 24)];
        let gone = cycle.iter().any(|&(g, b)| day >= g && day < b);
        if !gone {
            let generation = cycle.iter().filter(|&&(_, b)| day >= b).count() as u128;
            cands.push(cand(
                shop,
                100 + generation,
                Category::ProjectArtifact,
                Tier::S0,
                gib(1.2),
            ));
        }
        record(&mut s, n, at, &cands, Some(gib(free)));
        // 96 GiB → 41.2 GiB today: slower before the last cleanup, 3.5 GiB/day after it.
        free -= if day < 19 { 2.49 } else { 3.5 };
    }
}

fn json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap()
}

#[test]
fn missing_database_reads_no_database_and_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let s = Session::new(setup(&data));
    let f = s.free_space_history(0, None);
    assert_eq!(f.state, HistoryRead::NoDatabase);
    assert!(f.samples.is_empty());
    assert_eq!(s.rollup_history(0).state, HistoryRead::NoDatabase);
    assert_eq!(s.regrowth(0).state, HistoryRead::NoDatabase);
    assert_eq!(s.footprint_history(0).state, HistoryRead::NoDatabase);
    assert!(!data.exists(), "a read must not create the data directory");
    for (stem, v) in [
        ("free-space-history", json(&f)),
        ("rollup-history", json(&s.rollup_history(0))),
        ("regrowth", json(&s.regrowth(0))),
        ("footprint-history", json(&s.footprint_history(0))),
    ] {
        validate(stem, &v);
    }
}

#[test]
fn history_commands_read_the_recorded_series() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    seed(&data);
    let db = scull_engine::history_db_path(&data);
    let before = std::fs::read(&db).unwrap();
    let s = Session::new(setup(&data));
    let since = NOW - 30 * DAY;

    let free = s.free_space_history(since, None);
    assert_eq!(free.state, HistoryRead::Ok);
    assert_eq!(free.volume_id.as_deref(), Some(VOLUME));
    assert_eq!(free.samples.len(), 31);
    assert_eq!(free.samples[0].free_bytes, gib(96.0));
    let today = free.samples[30].free_bytes as f64 / GIB;
    assert!((today - 41.2).abs() < 0.02, "{today}");
    assert!(
        free.samples
            .windows(2)
            .all(|w| w[0].sampled_at < w[1].sampled_at)
    );
    // An explicit volume that was never sampled reads empty, not an error.
    let other = s.free_space_history(since, Some("/Volumes/None"));
    assert_eq!((other.state, other.samples.len()), (HistoryRead::Ok, 0));

    let roll = s.rollup_history(NOW - 200 * DAY);
    assert_eq!(roll.state, HistoryRead::Ok);
    let cats: Vec<_> = roll.series.iter().map(|c| c.category).collect();
    assert_eq!(
        cats,
        [
            Category::ProjectArtifact,
            Category::PackageCache,
            Category::MlCache,
            Category::Other
        ]
    );
    assert!(roll.series.iter().all(|c| c.points.len() == 36));

    let regrowth = s.regrowth(since);
    assert_eq!(regrowth.state, HistoryRead::Ok);
    let shop: Vec<_> = regrowth
        .events
        .iter()
        .filter(|e| e.path.as_str().ends_with("shop-api/node_modules"))
        .collect();
    assert_eq!(shop.len(), 4);
    assert!(shop.iter().all(|e| !e.same_item
        && e.category == Category::ProjectArtifact
        && e.reappeared_at - e.disappeared_at == 2 * DAY));

    let fp = s.footprint_history(since);
    assert_eq!(fp.state, HistoryRead::Ok);
    assert_eq!(fp.samples.len(), 31);
    assert!(fp.samples.iter().all(|x| x.total_bytes > 0
        && x.total_bytes == x.database_bytes + x.wal_bytes + x.backup_bytes + x.damaged_bytes));

    // The weekly digest (INS-04) of the same history: 41.2 GiB free on /, shrinking.
    let digest = s.weekly_digest(NOW);
    assert_eq!(digest.state, HistoryRead::Ok);
    let d = digest.digest.as_ref().unwrap();
    assert!(d.has_history);
    assert_eq!(d.since, NOW - 7 * DAY);
    assert_eq!(d.volumes[0].volume_id, VOLUME);
    assert!(d.volumes[0].change_bytes < 0);
    assert!(
        d.lines[0].starts_with("41.2 GiB free on / (−24.5 GiB this week)"),
        "{:?}",
        d.lines
    );
    assert!(
        d.lines
            .contains(&"Grew most: ML model caches (+1.8 GiB)".to_owned()),
        "{:?}",
        d.lines
    );

    let docs = [
        ("free-space-history", json(&free)),
        ("rollup-history", json(&roll)),
        ("regrowth", json(&regrowth)),
        ("footprint-history", json(&fp)),
        ("weekly-digest", json(&digest)),
    ];
    for (stem, v) in &docs {
        validate(stem, v);
    }
    drop(s);
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "reads leave the database file unchanged"
    );

    if std::env::var_os("SCULL_UPDATE_MOCKS").is_some() {
        let dir = desktop().join("src/bridge/fixtures");
        for (stem, v) in &docs {
            let text = serde_json::to_string_pretty(v).unwrap();
            std::fs::write(dir.join(format!("{stem}.json")), text + "\n").unwrap();
        }
    }
}
