//! History recording (P1-STORE-02; INS-01, INS-06, INS-07, DATA-02, DATA-05, DATA-06).
//! All databases live in temp dirs.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use scull_core::{
    Action, Candidate, Category, Evidence, FileIdentity, FsPath, ItemId, ProjectRef, ReasonCode,
    ScanId, Tier, TierReason,
};
use scull_storage::{
    ItemEventKind, RetentionPolicy, ScanFinish, ScanRecord, ScanRecorded, ScanStart, ScanStatus,
    ScanTotals, Store, VolumeSample, rollups_from_candidates,
};
use std::time::{Duration, Instant};

const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
const MIB: u64 = 1024 * 1024;

/// A candidate at `path` with file ID `file` (a new file ID at the same path = a rebuilt
/// artifact) and `allocated` bytes.
fn cand(path: &str, file: u128, tier: Tier, allocated: u64) -> Candidate {
    let mut c = candidate(file, tier, Action::Delete);
    c.identity = FileIdentity::new(7, file);
    c.path = FsPath::from(path);
    c.id = ItemId::derive(c.identity, path.as_bytes());
    c.sizes.logical = allocated;
    c.sizes.allocated = allocated;
    c.sizes.reclaimable.bytes = allocated;
    c
}

fn vol(id: &str, free: u64, at: i64) -> VolumeSample {
    VolumeSample {
        volume_id: id.into(),
        total: 1000 * MIB,
        free,
        at,
    }
}

fn begin(s: &mut Store, n: u32, at: i64, roots: &[&str]) {
    s.record_scan_start(&ScanStart {
        scan_id: scan_id(n),
        started_at: at,
        roots: roots.iter().map(|r| FsPath::from(*r)).collect(),
    })
    .unwrap();
}

fn finish(
    s: &mut Store,
    n: u32,
    at: i64,
    status: ScanStatus,
    cands: &[Candidate],
    volumes: &[VolumeSample],
) -> ScanRecorded {
    let rollups = rollups_from_candidates(cands).unwrap();
    let sum = |f: fn(&Candidate) -> u64| cands.iter().map(f).sum::<u64>();
    s.record_scan(&ScanRecord {
        finish: ScanFinish {
            scan_id: scan_id(n),
            finished_at: at,
            status,
            totals: ScanTotals {
                entries_scanned: 100,
                candidates: cands.len() as u64,
                logical_bytes: sum(|c| c.sizes.logical),
                allocated_bytes: sum(|c| c.sizes.allocated),
                reclaimable_bytes: sum(|c| c.sizes.reclaimable.bytes),
                errors: u64::from(status == ScanStatus::Partial),
            },
            rollups,
        },
        candidates: cands,
        volumes,
    })
    .unwrap()
}

/// One full scan: start at `at`, finish 10 s later.
fn scan(
    s: &mut Store,
    n: u32,
    at: i64,
    roots: &[&str],
    status: ScanStatus,
    cands: &[Candidate],
) -> ScanRecorded {
    begin(s, n, at, roots);
    finish(
        s,
        n,
        at + 10,
        status,
        cands,
        &[vol("/", 500 * MIB, at + 10)],
    )
}

fn three() -> Vec<Candidate> {
    vec![
        cand("/work/a/target", 1, Tier::S0, 10 * MIB),
        cand("/work/b/node_modules", 2, Tier::S1, 20 * MIB),
        cand("/work/c/target", 3, Tier::S0, 30 * MIB),
    ]
}

fn kinds(s: &Store, c: &Candidate) -> Vec<ItemEventKind> {
    s.item_history(&c.id)
        .unwrap()
        .unwrap()
        .events
        .iter()
        .map(|e| e.kind)
        .collect()
}

// ---- recording ------------------------------------------------------------------------------

#[test]
fn first_scan_records_snapshot_items_free_space_and_footprint() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    // Nothing yet: status works on an empty store.
    let empty = s.status_summary().unwrap();
    assert!(empty.last_scan.is_none() && empty.volumes.is_empty());

    let cands = three();
    begin(&mut s, 1, T0, &["/work"]);
    let rec = finish(
        &mut s,
        1,
        T0 + 10,
        ScanStatus::Completed,
        &cands,
        &[
            vol("/", 500 * MIB, T0 + 10),
            vol("/data", 900 * MIB, T0 + 10),
        ],
    );
    assert_eq!(rec.new_items, 3);
    assert_eq!((rec.seen_again, rec.disappeared, rec.regrowth), (0, 0, 0));
    assert!(rec.footprint.database_bytes > 0, "{:?}", rec.footprint);

    // Scan snapshot.
    let latest = s.latest_scan(&FsPath::from("/work")).unwrap().unwrap();
    assert_eq!(latest.scan_id, scan_id(1));
    assert_eq!(latest.status, ScanStatus::Completed);
    assert_eq!((latest.started_at, latest.finished_at), (T0, T0 + 10));
    assert_eq!(latest.roots, vec![FsPath::from("/work")]);
    assert_eq!(latest.totals.candidates, 3);
    assert_eq!(latest.totals.allocated_bytes, 60 * MIB);
    assert!(s.latest_scan(&FsPath::from("/other")).unwrap().is_none());

    // Per category x tier rollups.
    let points = s.rollup_history(Category::ProjectArtifact, 0).unwrap();
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].at, T0 + 10);
    assert_eq!(points[0].items, 3);
    assert_eq!(points[0].allocated_bytes, 60 * MIB);
    let c = raw(s.path());
    let tiers: Vec<(String, i64, i64)> = c
        .prepare("SELECT tier, items, allocated_bytes FROM scan_rollups ORDER BY tier")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        tiers,
        vec![
            ("S0".into(), 2, (40 * MIB) as i64),
            ("S1".into(), 1, (20 * MIB) as i64)
        ]
    );

    // Free space per volume.
    assert_eq!(
        s.free_space_history("/", 0).unwrap(),
        vec![vol("/", 500 * MIB, T0 + 10)]
    );
    assert_eq!(s.free_space_history("/data", 0).unwrap().len(), 1);

    // Item lifecycle.
    for cnd in &cands {
        let h = s.item_history(&cnd.id).unwrap().unwrap();
        assert_eq!((h.first_seen_at, h.last_seen_at), (T0 + 10, T0 + 10));
        assert_eq!(h.disappeared_at, None);
        assert_eq!(h.path, cnd.path);
        assert_eq!(h.category, Category::ProjectArtifact);
        assert_eq!(h.events.len(), 1);
        assert_eq!(h.events[0].kind, ItemEventKind::FirstSeen);
        assert_eq!(h.events[0].scan_id, scan_id(1));
        assert_eq!(h.events[0].allocated_bytes, cnd.sizes.allocated);
    }

    // scull's own footprint (DATA-05).
    let fp = s.footprint_history(0).unwrap();
    assert_eq!(fp.len(), 1);
    assert_eq!(fp[0].scan_id, Some(scan_id(1)));
    assert_eq!(fp[0].at, T0 + 10);
    assert!(fp[0].footprint.database_bytes > 0);
    assert_eq!(s.footprint().backup_bytes, 0);

    // Status line.
    let st = s.status_summary().unwrap();
    let last = st.last_scan.unwrap();
    assert_eq!(last.scan_id, scan_id(1));
    assert_eq!(last.candidates, 3);
    assert_eq!(last.reclaimable_bytes, 60 * MIB);
    assert_eq!(last.safe_reclaimable_bytes, 40 * MIB, "S0 only");
    assert_eq!(
        st.volumes,
        vec![
            vol("/", 500 * MIB, T0 + 10),
            vol("/data", 900 * MIB, T0 + 10)
        ]
    );
}

#[test]
fn second_scan_updates_last_seen_and_marks_disappeared_items_under_its_roots() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let mut cands = three();
    let outside = cand("/elsewhere/x/target", 9, Tier::S0, MIB);
    cands.push(outside.clone());
    scan(
        &mut s,
        1,
        T0,
        &["/work", "/elsewhere"],
        ScanStatus::Completed,
        &cands,
    );

    // Scan 2 covers /work only and no longer sees /work/c/target.
    let rec = scan(
        &mut s,
        2,
        T0 + DAY,
        &["/work"],
        ScanStatus::Completed,
        &cands[..2],
    );
    assert_eq!((rec.new_items, rec.seen_again, rec.disappeared), (0, 2, 1));

    let a = s.item_history(&cands[0].id).unwrap().unwrap();
    assert_eq!((a.first_seen_at, a.last_seen_at), (T0 + 10, T0 + DAY + 10));
    assert_eq!(a.disappeared_at, None);
    assert_eq!(a.events.len(), 1, "no size change, no new event");

    let gone = s.item_history(&cands[2].id).unwrap().unwrap();
    assert_eq!(gone.disappeared_at, Some(T0 + DAY + 10));
    assert_eq!(gone.last_seen_at, T0 + 10);
    assert_eq!(
        kinds(&s, &cands[2]),
        vec![ItemEventKind::FirstSeen, ItemEventKind::Disappeared]
    );
    let last = gone.events.last().unwrap();
    assert_eq!(
        (last.scan_id.clone(), last.allocated_bytes),
        (scan_id(2), 30 * MIB)
    );

    // Outside the scan's roots: untouched.
    let o = s.item_history(&outside.id).unwrap().unwrap();
    assert_eq!(o.disappeared_at, None);

    // latest_scan per root.
    assert_eq!(
        s.latest_scan(&FsPath::from("/work"))
            .unwrap()
            .unwrap()
            .scan_id,
        scan_id(2)
    );
    assert_eq!(
        s.latest_scan(&FsPath::from("/elsewhere"))
            .unwrap()
            .unwrap()
            .scan_id,
        scan_id(1)
    );

    // A third completed scan does not mark it gone twice.
    let rec = scan(
        &mut s,
        3,
        T0 + 2 * DAY,
        &["/work"],
        ScanStatus::Completed,
        &cands[..2],
    );
    assert_eq!(rec.disappeared, 0);
    assert_eq!(kinds(&s, &cands[2]).len(), 2);
}

#[test]
fn incomplete_scans_never_mark_items_gone() {
    for status in [
        ScanStatus::Cancelled,
        ScanStatus::Partial,
        ScanStatus::Failed,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = open(&db_in(&dir));
        let cands = three();
        scan(&mut s, 1, T0, &["/work"], ScanStatus::Completed, &cands);
        // Sees nothing at all, but did not finish cleanly.
        let rec = scan(&mut s, 2, T0 + DAY, &["/work"], status, &[]);
        assert_eq!(rec.disappeared, 0, "{status:?}");
        for c in &cands {
            let h = s.item_history(&c.id).unwrap().unwrap();
            assert_eq!(h.disappeared_at, None, "{status:?}");
            assert_eq!(h.events.len(), 1, "{status:?}");
        }
        // Items it did see are still refreshed.
        let rec = scan(&mut s, 3, T0 + 2 * DAY, &["/work"], status, &cands[..1]);
        assert_eq!((rec.seen_again, rec.disappeared), (1, 0), "{status:?}");
        let h = s.item_history(&cands[0].id).unwrap().unwrap();
        assert_eq!(h.last_seen_at, T0 + 2 * DAY + 10, "{status:?}");
    }
}

#[test]
fn rebuilt_artifact_is_recorded_as_regrowth() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let old = cand("/work/b/node_modules", 2, Tier::S1, 20 * MIB);
    scan(
        &mut s,
        1,
        T0,
        &["/work"],
        ScanStatus::Completed,
        std::slice::from_ref(&old),
    );
    // Cleaned: gone after the next completed scan.
    scan(&mut s, 2, T0 + DAY, &["/work"], ScanStatus::Completed, &[]);
    // Rebuilt: same path, new file identity.
    let rebuilt = cand("/work/b/node_modules", 200, Tier::S1, 21 * MIB);
    let rec = scan(
        &mut s,
        3,
        T0 + 2 * DAY,
        &["/work"],
        ScanStatus::Completed,
        std::slice::from_ref(&rebuilt),
    );
    assert_eq!((rec.new_items, rec.regrowth), (1, 1));

    let events = s.regrowth(Category::ProjectArtifact, 0).unwrap();
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(e.path, old.path);
    assert_eq!(e.item_id, rebuilt.id);
    assert_eq!(e.previous_item_id, old.id);
    assert!(!e.same_item());
    assert_eq!(e.previous_last_seen_at, T0 + 10);
    assert_eq!(e.disappeared_at, T0 + DAY + 10);
    assert_eq!(e.reappeared_at, T0 + 2 * DAY + 10);
    assert_eq!(e.scan_id, scan_id(3));
    assert!(s.regrowth(Category::PackageCache, 0).unwrap().is_empty());
    assert!(
        s.regrowth(Category::ProjectArtifact, T0 + 3 * DAY)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn same_item_coming_back_is_reappeared_and_regrowth() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let item = cand("/work/a/target", 1, Tier::S0, 10 * MIB);
    let one = std::slice::from_ref(&item);
    scan(&mut s, 1, T0, &["/work"], ScanStatus::Completed, one);
    scan(&mut s, 2, T0 + DAY, &["/work"], ScanStatus::Completed, &[]);
    let rec = scan(
        &mut s,
        3,
        T0 + 2 * DAY,
        &["/work"],
        ScanStatus::Completed,
        one,
    );
    assert_eq!((rec.reappeared, rec.regrowth, rec.new_items), (1, 1, 0));
    let h = s.item_history(&item.id).unwrap().unwrap();
    assert_eq!(h.disappeared_at, None);
    assert_eq!(h.first_seen_at, T0 + 10);
    assert_eq!(
        kinds(&s, &item),
        vec![
            ItemEventKind::FirstSeen,
            ItemEventKind::Disappeared,
            ItemEventKind::Reappeared
        ]
    );
    let r = s.regrowth(Category::ProjectArtifact, 0).unwrap();
    assert_eq!(r.len(), 1);
    assert!(r[0].same_item(), "restored, not rebuilt");
}

#[test]
fn replacement_between_scans_marks_the_old_item_gone_even_in_a_partial_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let old = cand("/work/b/node_modules", 2, Tier::S1, 20 * MIB);
    scan(
        &mut s,
        1,
        T0,
        &["/work"],
        ScanStatus::Completed,
        std::slice::from_ref(&old),
    );
    // `rm -rf node_modules && npm i` between two scans: a different object at the same path.
    let new = cand("/work/b/node_modules", 201, Tier::S1, 22 * MIB);
    let rec = scan(
        &mut s,
        2,
        T0 + DAY,
        &["/work"],
        ScanStatus::Partial,
        std::slice::from_ref(&new),
    );
    assert_eq!((rec.new_items, rec.disappeared, rec.regrowth), (1, 1, 1));
    let h = s.item_history(&old.id).unwrap().unwrap();
    assert_eq!(h.disappeared_at, Some(T0 + DAY + 10));
    let r = s.regrowth(Category::ProjectArtifact, 0).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].previous_item_id, old.id);
    assert_eq!(r[0].previous_last_seen_at, T0 + 10);
}

#[test]
fn item_events_record_only_tier_changes_and_significant_size_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let base = cand("/work/a/target", 1, Tier::S0, 100 * MIB);
    let at = |n: i64| T0 + n * DAY;
    scan(
        &mut s,
        1,
        at(0),
        &["/work"],
        ScanStatus::Completed,
        std::slice::from_ref(&base),
    );
    // +1% : below the 5% threshold.
    let small = cand("/work/a/target", 1, Tier::S0, 101 * MIB);
    scan(
        &mut s,
        2,
        at(1),
        &["/work"],
        ScanStatus::Completed,
        std::slice::from_ref(&small),
    );
    // Creeping +1% per scan accumulates against the last *recorded* size: 105 MiB triggers.
    let creep = cand("/work/a/target", 1, Tier::S0, 105 * MIB);
    scan(
        &mut s,
        3,
        at(2),
        &["/work"],
        ScanStatus::Completed,
        std::slice::from_ref(&creep),
    );
    // Same size, tier change.
    let tier = cand("/work/a/target", 1, Tier::S1, 105 * MIB);
    scan(
        &mut s,
        4,
        at(3),
        &["/work"],
        ScanStatus::Completed,
        std::slice::from_ref(&tier),
    );
    // A tiny item changing by a large fraction but less than 1 MiB: no event.
    let h = s.item_history(&base.id).unwrap().unwrap();
    let got: Vec<(ItemEventKind, Tier, u64)> = h
        .events
        .iter()
        .map(|e| (e.kind, e.tier, e.allocated_bytes))
        .collect();
    assert_eq!(
        got,
        vec![
            (ItemEventKind::FirstSeen, Tier::S0, 100 * MIB),
            (ItemEventKind::Changed, Tier::S0, 105 * MIB),
            (ItemEventKind::Changed, Tier::S1, 105 * MIB),
        ]
    );
    assert_eq!(h.tier, Tier::S1);

    let tiny = cand("/work/t/target", 5, Tier::S0, 1000);
    scan(
        &mut s,
        5,
        at(4),
        &["/work"],
        ScanStatus::Completed,
        &[tier.clone(), tiny.clone()],
    );
    let tiny2 = cand("/work/t/target", 5, Tier::S0, 500_000);
    scan(
        &mut s,
        6,
        at(5),
        &["/work"],
        ScanStatus::Completed,
        &[tier, tiny2],
    );
    assert_eq!(kinds(&s, &tiny), vec![ItemEventKind::FirstSeen]);
}

// ---- queries --------------------------------------------------------------------------------

#[test]
fn rollup_history_is_ordered_by_finish_time_and_skips_unusable_scans() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let c = |mib: u64| vec![cand("/work/a/target", 1, Tier::S0, mib * MIB)];
    // Recorded out of order: scan 1 finishes last.
    begin(&mut s, 1, T0, &["/work"]);
    begin(&mut s, 2, T0 + HOUR, &["/work"]);
    finish(&mut s, 2, T0 + 2 * HOUR, ScanStatus::Completed, &c(20), &[]);
    finish(&mut s, 1, T0 + 3 * HOUR, ScanStatus::Partial, &c(30), &[]);
    scan(
        &mut s,
        3,
        T0 + DAY,
        &["/work"],
        ScanStatus::Cancelled,
        &c(1),
    );
    scan(
        &mut s,
        4,
        T0 + 2 * DAY,
        &["/work"],
        ScanStatus::Failed,
        &c(1),
    );
    // Cleaned: no project artifacts at all → a zero point, not a gap.
    scan(
        &mut s,
        5,
        T0 + 3 * DAY,
        &["/work"],
        ScanStatus::Completed,
        &[],
    );
    scan(
        &mut s,
        6,
        T0 + 4 * DAY,
        &["/work"],
        ScanStatus::Completed,
        &c(5),
    );

    let pts = s.rollup_history(Category::ProjectArtifact, 0).unwrap();
    let got: Vec<(ScanId, i64, u64)> = pts
        .iter()
        .map(|p| (p.scan_id.clone(), p.at, p.allocated_bytes / MIB))
        .collect();
    assert_eq!(
        got,
        vec![
            (scan_id(2), T0 + 2 * HOUR, 20),
            (scan_id(1), T0 + 3 * HOUR, 30),
            (scan_id(5), T0 + 3 * DAY + 10, 0),
            (scan_id(6), T0 + 4 * DAY + 10, 5),
        ]
    );
    let since = s
        .rollup_history(Category::ProjectArtifact, T0 + 3 * DAY)
        .unwrap();
    assert_eq!(since.len(), 2);
    assert!(
        s.rollup_history(Category::Container, 0)
            .unwrap()
            .iter()
            .all(|p| p.allocated_bytes == 0 && p.items == 0)
    );
    // latest_scan skips cancelled/failed; the latest start wins.
    assert_eq!(
        s.latest_scan(&FsPath::from("/work"))
            .unwrap()
            .unwrap()
            .scan_id,
        scan_id(6)
    );
}

#[test]
fn free_space_history_per_volume_is_ordered_and_filtered() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    begin(&mut s, 1, T0, &["/work"]);
    finish(
        &mut s,
        1,
        T0 + 10,
        ScanStatus::Completed,
        &[],
        &[vol("/", 500 * MIB, T0 + 10), vol("D:\\", 50 * MIB, T0 + 10)],
    );
    // Scheduled checks between scans, recorded out of order.
    s.record_free_space(&[vol("/", 480 * MIB, T0 + 2 * HOUR)], None)
        .unwrap();
    s.record_free_space(&[vol("/", 490 * MIB, T0 + HOUR)], None)
        .unwrap();

    let frees: Vec<(i64, u64)> = s
        .free_space_history("/", 0)
        .unwrap()
        .iter()
        .map(|v| (v.at, v.free / MIB))
        .collect();
    assert_eq!(
        frees,
        vec![(T0 + 10, 500), (T0 + HOUR, 490), (T0 + 2 * HOUR, 480)]
    );
    assert_eq!(s.free_space_history("/", T0 + HOUR).unwrap().len(), 2);
    assert_eq!(s.free_space_history("D:\\", 0).unwrap().len(), 1);
    assert!(s.free_space_history("/nope", 0).unwrap().is_empty());

    // The status cache keeps the newest sample per volume, even when an older one arrives
    // later.
    let st = s.status_summary().unwrap();
    assert_eq!(
        st.volumes,
        vec![
            vol("/", 480 * MIB, T0 + 2 * HOUR),
            vol("D:\\", 50 * MIB, T0 + 10)
        ]
    );

    // Invalid samples are refused and nothing is written.
    for bad in [
        vol("", MIB, T0),
        VolumeSample {
            volume_id: "/".into(),
            total: MIB,
            free: 2 * MIB,
            at: T0,
        },
    ] {
        let err = s.record_free_space(&[bad], None).unwrap_err();
        assert_eq!(err.kind(), scull_core::ErrorKind::Usage);
    }
    assert_eq!(s.free_space_history("/", 0).unwrap().len(), 3);
}

#[test]
fn record_scan_is_atomic_and_requires_a_running_scan() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let cands = three();
    let rec = ScanRecord {
        finish: ScanFinish {
            scan_id: scan_id(1),
            finished_at: T0,
            status: ScanStatus::Completed,
            totals: ScanTotals::default(),
            rollups: vec![],
        },
        candidates: &cands,
        volumes: &[vol("/", MIB, T0)],
    };
    assert!(s.record_scan(&rec).is_err(), "scan was never started");
    assert!(s.item_history(&cands[0].id).unwrap().is_none());
    assert!(s.free_space_history("/", 0).unwrap().is_empty());
    assert!(s.footprint_history(0).unwrap().is_empty());

    begin(&mut s, 1, T0, &["/work"]);
    s.record_scan(&rec).unwrap();
    assert!(s.record_scan(&rec).is_err(), "already finished");
    assert_eq!(s.free_space_history("/", 0).unwrap().len(), 1);
}

#[test]
fn status_summary_is_fast_with_10k_scans() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    begin(&mut s, 0, T0, &["/work"]);
    finish(
        &mut s,
        0,
        T0 + 1,
        ScanStatus::Completed,
        &three(),
        &[vol("/", 500 * MIB, T0 + 1)],
    );
    {
        // Bulk history in one transaction (the public API would fsync 10k times).
        let mut c = raw(&path);
        let tx = c.transaction().unwrap();
        {
            let mut scan = tx
                .prepare(
                    "INSERT INTO scans (scan_id, started_at, finished_at, roots_json, status, \
                     entries_scanned, candidates, logical_bytes, allocated_bytes, \
                     reclaimable_bytes, errors) \
                     VALUES (?1, ?2, ?2 + 5, '[\"/work\"]', 'completed', 100, 2, 3, 3, ?3, 0)",
                )
                .unwrap();
            let mut root = tx
                .prepare(
                    "INSERT INTO scan_roots (root, started_at, scan_id) VALUES ('/work', ?1, ?2)",
                )
                .unwrap();
            let mut roll = tx
                .prepare(
                    "INSERT INTO scan_rollups (scan_id, category, tier, ecosystem, items, \
                     logical_bytes, allocated_bytes, reclaimable_bytes) \
                     VALUES (?1, 'project_artifact', ?2, '', 1, 1, 1, ?3)",
                )
                .unwrap();
            let mut free = tx
                .prepare(
                    "INSERT INTO free_space_samples (sampled_at, scan_id, volume, total_bytes, \
                     free_bytes) VALUES (?1, ?2, '/', 1000, 400)",
                )
                .unwrap();
            for n in 1..=10_000u32 {
                let id = scan_id(n).as_str().to_owned();
                let at = T0 + i64::from(n) * HOUR;
                scan.execute(rusqlite::params![id, at, i64::from(n) * 3])
                    .unwrap();
                root.execute(rusqlite::params![at, id]).unwrap();
                roll.execute(rusqlite::params![id, "S0", i64::from(n)])
                    .unwrap();
                roll.execute(rusqlite::params![id, "S2", i64::from(n) * 2])
                    .unwrap();
                free.execute(rusqlite::params![at, id]).unwrap();
            }
        }
        // The newest scans were cancelled: status shows the latest usable one.
        tx.execute_batch(&format!(
            "INSERT INTO scans (scan_id, started_at, finished_at, roots_json, status) \
             VALUES ('scan_X1', {a}, {a}, '[]', 'cancelled'), \
                    ('scan_X2', {b}, {b}, '[]', 'cancelled');",
            a = T0 + 20_000 * HOUR,
            b = T0 + 20_001 * HOUR
        ))
        .unwrap();
        tx.commit().unwrap();
    }

    let mut times = Vec::new();
    let mut summary = None;
    for _ in 0..7 {
        let started = Instant::now();
        summary = Some(s.status_summary().unwrap());
        times.push(started.elapsed());
    }
    times.sort();
    let median = times[times.len() / 2];
    assert!(
        median < Duration::from_millis(50),
        "status_summary took {median:?} (all: {times:?})"
    );
    let last = summary.unwrap().last_scan.unwrap();
    assert_eq!(last.scan_id, scan_id(10_000));
    assert_eq!(last.reclaimable_bytes, 30_000);
    assert_eq!(last.safe_reclaimable_bytes, 10_000);
    assert_eq!(last.status, ScanStatus::Completed);

    // latest_scan is an index walk too.
    let started = Instant::now();
    let l = s.latest_scan(&FsPath::from("/work")).unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(l.scan_id, scan_id(10_000));
}

// ---- retention (DATA-06) --------------------------------------------------------------------

#[test]
fn fx_data_02_old_free_space_samples_are_downsampled_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    // Safe on an empty store.
    let policy = RetentionPolicy::default();
    assert_eq!(
        s.maintain(&policy, T0).unwrap().free_space_samples_removed,
        0
    );

    let day0 = T0 - T0.rem_euclid(DAY); // UTC midnight
    let now = day0 + 400 * DAY;
    let mut samples = Vec::new();
    for d in 0..3 {
        for h in 0..24 {
            for v in ["/", "/data"] {
                samples.push(vol(v, (100 + h) as u64 * MIB, day0 + d * DAY + h * HOUR));
            }
        }
    }
    // Recent samples (inside the raw window) are kept as they are.
    for h in 0..24 {
        samples.push(vol("/", 300 * MIB, now - DAY + h * HOUR));
    }
    s.record_free_space(&samples, None).unwrap();

    let report = s.maintain(&policy, now).unwrap();
    assert_eq!(report.free_space_samples_removed, 2 * 3 * 23);
    let root = s.free_space_history("/", 0).unwrap();
    let old: Vec<_> = root.iter().filter(|v| v.at < now - 365 * DAY).collect();
    assert_eq!(old.len(), 3, "one per day");
    for (d, v) in old.iter().enumerate() {
        assert_eq!(
            v.at,
            day0 + d as i64 * DAY + 23 * HOUR,
            "last sample of the day"
        );
        assert_eq!(v.free, 123 * MIB);
    }
    assert_eq!(root.len(), 3 + 24);
    assert_eq!(s.free_space_history("/data", 0).unwrap().len(), 3);
    // Idempotent.
    assert_eq!(
        s.maintain(&policy, now).unwrap().free_space_samples_removed,
        0
    );
    // The status cache is untouched.
    assert_eq!(s.status_summary().unwrap().volumes.len(), 2);
}

#[test]
fn maintenance_never_touches_append_only_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    s.insert_plan(&plan(1)).unwrap();
    scan(&mut s, 1, T0, &["/work"], ScanStatus::Completed, &three());
    s.maintain(
        &RetentionPolicy {
            raw_free_space_secs: 0,
        },
        T0 + 1000 * DAY,
    )
    .unwrap();
    assert_eq!(s.get_plan(&plan_id(1)).unwrap().unwrap(), plan(1));
    // Append-only triggers are still in force on the v2 schema.
    let c = raw(&path);
    for sql in ["UPDATE plans SET origin = 'cli'", "DELETE FROM plans"] {
        let err = c.execute(sql, []).unwrap_err();
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
    }
}

// ---- privacy (INS-07) -----------------------------------------------------------------------

#[test]
fn fx_data_07_history_never_records_contents_evidence_or_environment_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let secret = "SECRET_TOKEN_9f8e7d6c";
    let mut c = cand("/work/a/node_modules", 1, Tier::S1, 10 * MIB);
    c.reasons = vec![TierReason::new(
        Tier::S1,
        ReasonCode::Regenerable,
        format!("env NPM_TOKEN={secret}"),
    )];
    c.evidence = vec![
        Evidence::KnownArtifact {
            artifact: format!("contents: {secret}"),
        },
        Evidence::InUse {
            signal: format!("cmdline --token {secret}"),
            by: Some(secret.into()),
        },
    ];
    c.project = Some(ProjectRef {
        root: FsPath::from("/work/a"),
        name: format!("pkg-{secret}"),
        ecosystem: "node".into(),
    });
    {
        let mut s = open(&path);
        scan(
            &mut s,
            1,
            T0,
            &["/work"],
            ScanStatus::Completed,
            &[c.clone()],
        );
        // Stored as rows we can read back, without the secret.
        assert!(s.item_history(&c.id).unwrap().is_some());
    }
    for f in scull_storage::footprint_files(&path) {
        let bytes = std::fs::read(&f).unwrap();
        let hay = String::from_utf8_lossy(&bytes);
        assert!(!hay.contains(secret), "{} contains the secret", f.display());
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        String::from_utf8_lossy(&bytes).contains("/work/a/node_modules"),
        "the path itself is recorded"
    );
}

// ---- helpers --------------------------------------------------------------------------------

#[test]
fn rollups_from_candidates_groups_by_category_tier_and_ecosystem() {
    let mut cands = three();
    cands[0].project = Some(ProjectRef {
        root: FsPath::from("/work/a"),
        name: "a".into(),
        ecosystem: "rust".into(),
    });
    cands.push(cand("/work/d/target", 4, Tier::S0, 5 * MIB));
    let r = rollups_from_candidates(&cands).unwrap();
    let got: Vec<(Tier, Option<&str>, u64, u64)> = r
        .iter()
        .map(|r| {
            (
                r.tier,
                r.ecosystem.as_deref(),
                r.items,
                r.allocated_bytes / MIB,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (Tier::S0, None, 2, 35),
            (Tier::S0, Some("rust"), 1, 10),
            (Tier::S1, None, 1, 20),
        ]
    );
    assert!(rollups_from_candidates(&[]).unwrap().is_empty());
}

#[test]
fn footprint_counts_database_wal_and_backups() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    scan(&mut s, 1, T0, &["/work"], ScanStatus::Completed, &three());
    std::fs::write(
        dir.path().join(format!(
            "{}.backup-20260101T000000Z-v1",
            scull_storage::DB_FILE_NAME
        )),
        vec![0u8; 4096],
    )
    .unwrap();
    let fp = s.footprint();
    assert!(fp.database_bytes > 0);
    assert!(fp.wal_bytes > 0, "WAL mode keeps a -wal file while open");
    assert_eq!(fp.backup_bytes, 4096);
    assert_eq!(
        fp.total(),
        fp.database_bytes + fp.wal_bytes + fp.backup_bytes + fp.damaged_bytes
    );
}
