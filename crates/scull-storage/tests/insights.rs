//! Storage behind local insights (P4-INS-01): the storage budget and downsampling
//! (FX-DATA-02, INS-02, DATA-05/06), the typed reads (item timelines, operation accuracy) and
//! `purge_insights`. All databases live in temp dirs.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use scull_core::{
    Action, Candidate, FileIdentity, FsPath, ItemId, OperationId, Plan, PlanItem, PlanOrigin,
    PlanParams, Tier,
};
use scull_storage::{
    ApprovalMethod, ItemEventKind, ItemOutcome, ItemRecord, OperationEnd, OperationFinish,
    OperationStart, RetentionPolicy, ScanFinish, ScanRecord, ScanStart, ScanStatus, ScanTotals,
    Store, VolumeSample, rollups_from_candidates,
};

const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
const MIB: u64 = 1024 * 1024;

fn cand(path: &str, file: u128, allocated: u64) -> Candidate {
    let mut c = candidate(file, Tier::S0, Action::Delete);
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

/// One completed scan of `/work` finished at `at`.
fn scan(s: &mut Store, n: u32, at: i64, cands: &[Candidate]) {
    s.record_scan_start(&ScanStart {
        scan_id: scan_id(n),
        started_at: at - 10,
        roots: vec![FsPath::from("/work")],
    })
    .unwrap();
    s.record_scan(&ScanRecord {
        finish: ScanFinish {
            scan_id: scan_id(n),
            finished_at: at,
            status: ScanStatus::Completed,
            totals: ScanTotals {
                candidates: cands.len() as u64,
                ..ScanTotals::default()
            },
            rollups: rollups_from_candidates(cands).unwrap(),
        },
        candidates: cands,
        volumes: &[vol("/", 500 * MIB, at)],
    })
    .unwrap();
}

fn count(path: &std::path::Path, sql: &str) -> i64 {
    raw(path).query_row(sql, [], |r| r.get(0)).unwrap()
}

fn utc_midnight(t: i64) -> i64 {
    t - t.rem_euclid(DAY)
}

// ---- storage budget (FX-DATA-02) ------------------------------------------------------------

/// Hourly samples of two volumes over `days` days starting at `day0`.
fn hourly(day0: i64, days: i64) -> Vec<VolumeSample> {
    let mut out = Vec::new();
    for d in 0..days {
        for h in 0..24 {
            for v in ["/", "/data"] {
                out.push(vol(
                    v,
                    ((d * 24 + h) as u64 % 900) * MIB,
                    day0 + d * DAY + h * HOUR,
                ));
            }
        }
    }
    out
}

#[test]
fn fx_data_02_budget_reached_downsamples_oldest_raw_data_first_and_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let day0 = utc_midnight(T0);
    let days = 240; // all inside the 12-month raw window: only the budget can thin them
    let now = day0 + days * DAY;
    s.record_free_space(&hourly(day0, days), None).unwrap();
    s.insert_plan(&plan(1)).unwrap();

    let before = s.budget_bytes_used().unwrap();
    assert!(
        before > 512 * 1024,
        "seeded history is large enough: {before}"
    );
    let budget = before * 6 / 10;
    let policy = RetentionPolicy {
        budget_bytes: Some(budget),
        ..RetentionPolicy::default()
    };
    let report = s.maintain(&policy, now).unwrap();

    // Budget respected, and nothing was needed beyond daily downsampling.
    assert!(!report.over_budget, "{report:?}");
    assert_eq!(report.bytes_before, before);
    assert!(report.bytes_after <= budget, "{report:?}");
    assert!(s.budget_bytes_used().unwrap() <= budget);
    assert!(report.free_space_samples_removed > 0);
    let cutoff = report.budget_cutoff.unwrap();
    assert!(cutoff > day0 && cutoff < now, "{report:?}");

    // Oldest first: before the cutoff one sample per volume per day (the last of the day);
    // after it every hourly sample is still there.
    for v in ["/", "/data"] {
        let h = s.free_space_history(v, 0).unwrap();
        let old: Vec<_> = h.iter().filter(|x| x.at < cutoff).collect();
        let new: Vec<_> = h.iter().filter(|x| x.at >= cutoff).collect();
        assert_eq!(old.len() as i64, (cutoff - day0) / DAY, "{v}: one per day");
        assert!(
            old.iter()
                .all(|x| (x.at - day0).rem_euclid(DAY) == 23 * HOUR)
        );
        assert_eq!(
            new.len() as i64,
            (now - cutoff) / HOUR,
            "{v}: recent data stays raw"
        );
    }
    // Append-only records and the status cache are untouched.
    assert_eq!(s.get_plan(&plan_id(1)).unwrap().unwrap(), plan(1));
    assert_eq!(s.status_summary().unwrap().volumes.len(), 2);

    // Idempotent once within budget.
    let again = s.maintain(&policy, now).unwrap();
    assert_eq!(again.free_space_samples_removed, 0);
    assert!(!again.over_budget);
}

#[test]
fn fx_data_02_unreachable_budget_thins_to_monthly_and_reports_over_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let day0 = utc_midnight(T0);
    let now = day0 + 90 * DAY;
    s.record_free_space(&hourly(day0, 90), None).unwrap();
    let policy = RetentionPolicy {
        budget_bytes: Some(1),
        ..RetentionPolicy::default()
    };
    let report = s.maintain(&policy, now).unwrap();
    assert!(report.over_budget);
    // Everything is down to one sample per volume per UTC month, never to nothing.
    let n = count(&path, "SELECT count(*) FROM free_space_samples");
    let months = count(
        &path,
        "SELECT count(DISTINCT volume || strftime('%Y-%m', sampled_at, 'unixepoch')) \
         FROM free_space_samples",
    );
    assert_eq!(n, months);
    assert!((6..=10).contains(&n), "{n}");
    assert_eq!(s.status_summary().unwrap().volumes.len(), 2);
}

#[test]
fn fx_data_02_no_budget_and_recent_data_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let day0 = utc_midnight(T0);
    s.record_free_space(&hourly(day0, 10), None).unwrap();
    let policy = RetentionPolicy {
        budget_bytes: None,
        ..RetentionPolicy::default()
    };
    let r = s.maintain(&policy, day0 + 10 * DAY).unwrap();
    assert_eq!(r.free_space_samples_removed, 0);
    assert!(!r.over_budget);
    assert_eq!(r.budget_cutoff, None);
    assert_eq!(
        count(&path, "SELECT count(*) FROM free_space_samples"),
        10 * 24 * 2
    );
}

#[test]
fn age_tiers_thin_item_changes_and_footprint_but_keep_lifecycle_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let t = utc_midnight(T0);
    // One item growing by 10 MiB every 6 hours for 4 days: a `changed` event per scan.
    let mut n = 0;
    for step in 0..16 {
        n += 1;
        scan(
            &mut s,
            n,
            t + step * 6 * HOUR + 60,
            &[cand(
                "/work/a/node_modules",
                1,
                (10 + 10 * step as u64) * MIB,
            )],
        );
    }
    let id = cand("/work/a/node_modules", 1, 0).id;
    let events = s.item_history(&id).unwrap().unwrap().events;
    assert_eq!(events.len(), 16);
    let footprints = count(&path, "SELECT count(*) FROM footprint_samples");
    assert_eq!(footprints, 16);

    // Everything is older than the raw window: daily points remain.
    let policy = RetentionPolicy {
        raw_secs: DAY,
        budget_bytes: None,
        ..RetentionPolicy::default()
    };
    let r = s.maintain(&policy, t + 30 * DAY).unwrap();
    let events = s.item_history(&id).unwrap().unwrap().events;
    assert_eq!(events[0].kind, ItemEventKind::FirstSeen, "lifecycle kept");
    let changed: Vec<_> = events
        .iter()
        .filter(|e| e.kind == ItemEventKind::Changed)
        .collect();
    // Day 0: first_seen + 3 changes -> last change kept; days 1..3: last change each.
    assert_eq!(changed.len(), 4);
    assert_eq!(changed.last().unwrap().allocated_bytes, 160 * MIB);
    assert_eq!(r.item_events_removed, 15 - 4);
    assert_eq!(count(&path, "SELECT count(*) FROM footprint_samples"), 4);
    assert_eq!(r.footprint_samples_removed, 12);
}

// ---- item timelines ----------------------------------------------------------------------------

#[test]
fn fx_ins_01_item_timelines_give_sizes_at_any_time_in_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let t = T0;
    let a = |mib| cand("/work/a/node_modules", 1, mib * MIB);
    let gone_early = cand("/work/old/target", 2, 50 * MIB);
    let b = cand("/work/b/target", 3, 40 * MIB);
    scan(&mut s, 1, t, &[a(100), gone_early.clone()]);
    scan(&mut s, 2, t + 10 * DAY, &[a(100)]); // `old/target` gone at day 10
    scan(&mut s, 3, t + 20 * DAY, &[a(200), b.clone()]);
    scan(&mut s, 4, t + 30 * DAY, &[a(300)]); // `b/target` gone at day 30

    let since = t + 15 * DAY;
    let tl = s.item_timelines(since).unwrap();
    let paths: Vec<_> = tl.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(
        paths,
        ["/work/a/node_modules", "/work/b/target"],
        "gone before `since` excluded"
    );
    let ta = &tl[0];
    // Baseline event (day 0, 100 MiB) plus the later changes.
    assert_eq!(ta.events.first().unwrap().at, t);
    assert_eq!(ta.allocated_at(since), 100 * MIB);
    assert_eq!(ta.allocated_at(t + 25 * DAY), 200 * MIB);
    assert_eq!(ta.allocated_at(t + 31 * DAY), 300 * MIB);
    assert_eq!(ta.allocated_at(t - 1), 0, "before first seen");
    let tb = &tl[1];
    assert_eq!(tb.allocated_at(since), 0);
    assert_eq!(tb.allocated_at(t + 25 * DAY), 40 * MIB);
    assert_eq!(tb.allocated_at(t + 30 * DAY), 0, "gone");
    assert!(tb.alive_at(t + 20 * DAY) && !tb.alive_at(t + 30 * DAY));
    assert_eq!(tb.disappeared_at, Some(t + 30 * DAY));
}

// ---- operation accuracy ------------------------------------------------------------------------

/// Records operation `n` over `rows` (candidate, predicted bytes, freed bytes): each item is
/// planned with its predicted bytes and removed, freeing the given bytes (`None`: a move to
/// quarantine, which frees nothing yet and is not compared).
fn operation(s: &mut Store, n: u32, at: i64, rows: &[(Candidate, u64, Option<u64>)]) {
    let items: Vec<PlanItem> = rows
        .iter()
        .map(|(c, pred, freed)| {
            let mut c = c.clone();
            c.sizes.reclaimable.bytes = *pred;
            if freed.is_none() {
                c.tier = Tier::S1;
                c.reasons = vec![scull_core::TierReason::new(
                    Tier::S1,
                    scull_core::ReasonCode::Regenerable,
                    "test",
                )];
                c.action = Action::Quarantine;
            }
            PlanItem::from_candidate(&c).unwrap()
        })
        .collect();
    let p = Plan::new(
        PlanParams {
            id: plan_id(n),
            scan_id: scan_id(1),
            created_unix: at - 10,
            ttl_secs: None,
            machine_id: "machine-a".into(),
            user: "dev".into(),
            origin: PlanOrigin::Cli,
        },
        items,
    )
    .unwrap();
    s.insert_plan(&p).unwrap();
    let mut audit = s
        .begin_operation(
            &OperationStart {
                operation_id: OperationId::from_suffix(&format!("O{n}")).unwrap(),
                plan_id: p.id().clone(),
                content_hash: p.content_hash().clone(),
                approver: "dev".into(),
                method: ApprovalMethod::CliTty,
                started_at: at,
            },
            &TestPerms::default(),
        )
        .unwrap();
    for (i, (c, pred, freed)) in rows.iter().enumerate() {
        let at = at + 1 + i as i64;
        let rec = match freed {
            Some(f) => {
                let mut r = ItemRecord::new(c.id.clone(), at, ItemOutcome::Removed);
                r.freed_bytes = Some(*f);
                r.entries_removed = Some(1);
                r.entries_kept = Some(0);
                r
            }
            None => {
                let mut r = ItemRecord::new(c.id.clone(), at, ItemOutcome::Quarantined);
                r.moved_bytes = Some(*pred);
                r
            }
        };
        s.record_item(&mut audit, &rec).unwrap();
    }
    s.finish_operation(
        &mut audit,
        &OperationFinish {
            finished_at: at + 100,
            end: OperationEnd::Normal,
            measured_delta_bytes: None,
        },
    )
    .unwrap();
}

#[test]
fn fx_ins_02_operation_accuracy_compares_predicted_and_measured_and_counts_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    let nm = |file| cand("/work/a/node_modules", file, 100 * MIB);
    scan(&mut s, 1, T0, &[nm(1)]);
    operation(
        &mut s,
        1,
        T0 + 100,
        &[
            (nm(1), 100 * MIB, Some(90 * MIB)),
            (nm(9), 5, None), // moved, nothing freed: not compared
        ],
    );
    // Cleaned, then rebuilt (new file identity at the same path).
    scan(&mut s, 2, T0 + DAY, &[]);
    scan(&mut s, 3, T0 + 2 * DAY, &[nm(2)]);
    operation(
        &mut s,
        2,
        T0 + 3 * DAY,
        &[(nm(2), 10 * MIB, Some(10 * MIB))],
    );

    let ops = s.operation_accuracy(0).unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0].operation_id, "op_O1");
    assert_eq!(ops[0].items_compared, 1);
    assert_eq!(ops[0].predicted_bytes, 100 * MIB);
    assert_eq!(ops[0].measured_bytes, 90 * MIB);
    assert_eq!(
        ops[0].regrown_items, 1,
        "node_modules came back after the cleanup"
    );
    assert_eq!(ops[1].regrown_items, 0);
    assert_eq!(
        s.operation_accuracy(T0 + DAY).unwrap().len(),
        1,
        "since filter"
    );
}

// ---- purge -------------------------------------------------------------------------------------

#[test]
fn fx_ins_03_purge_forgets_history_but_keeps_live_items_scans_and_append_only_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_in(&dir);
    let mut s = open(&path);
    let nm = |file, mib| cand("/work/a/node_modules", file, mib * MIB);
    scan(
        &mut s,
        1,
        T0,
        &[nm(1, 100), cand("/work/x/target", 5, 10 * MIB)],
    );
    scan(&mut s, 2, T0 + DAY, &[]);
    scan(&mut s, 3, T0 + 2 * DAY, &[nm(2, 120)]);
    operation(&mut s, 1, T0 + 3 * DAY, &[(nm(2, 0), 1, Some(1))]);
    let audit_before = count(&path, "SELECT count(*) FROM audit");

    // Older-than: only rows before the cutoff go; the gone items' rows go with their events.
    let purged = s.purge_insights(Some(T0 + DAY + 1)).unwrap();
    assert_eq!(purged.free_space_samples, 2);
    assert_eq!(purged.footprint_samples, 2);
    assert_eq!(
        purged.gone_items, 2,
        "node_modules #1 and x/target, gone at day 1"
    );
    assert_eq!(purged.regrowth_events, 1, "it named a purged item");
    assert_eq!(count(&path, "SELECT count(*) FROM free_space_samples"), 1);

    // Everything.
    let purged = s.purge_insights(None).unwrap();
    assert!(purged.total() > 0);
    for t in [
        "free_space_samples",
        "footprint_samples",
        "item_events",
        "regrowth_events",
    ] {
        assert_eq!(count(&path, &format!("SELECT count(*) FROM {t}")), 0, "{t}");
    }
    // Kept: live items, scans and rollups, the volumes cache, plans and audit.
    assert_eq!(count(&path, "SELECT count(*) FROM items"), 1);
    assert_eq!(count(&path, "SELECT count(*) FROM scans"), 3);
    assert!(count(&path, "SELECT count(*) FROM scan_rollups") > 0);
    assert_eq!(s.status_summary().unwrap().volumes.len(), 1);
    assert_eq!(count(&path, "SELECT count(*) FROM audit"), audit_before);
    assert!(s.get_plan(&plan_id(1)).unwrap().is_some());
    assert_eq!(
        count(&path, "SELECT count(*) FROM pragma_foreign_key_check"),
        0
    );

    // A live item with no events left still has a size, and recording carries on: the next
    // sighting records a fresh baseline.
    let tl = s.item_timelines(0).unwrap();
    assert_eq!(tl[0].allocated_at(T0 + 2 * DAY), 120 * MIB);
    scan(&mut s, 4, T0 + 4 * DAY, &[nm(2, 120)]);
    let h = s.item_history(&nm(2, 0).id).unwrap().unwrap();
    assert_eq!(h.events.len(), 1);
    assert_eq!(h.events[0].kind, ItemEventKind::Changed);
}

#[test]
fn volume_ids_lists_every_sampled_volume() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = open(&db_in(&dir));
    assert!(s.volume_ids().unwrap().is_empty());
    s.record_free_space(
        &[vol("/b", 1, T0), vol("/a", 1, T0), vol("/b", 2, T0 + 1)],
        None,
    )
    .unwrap();
    assert_eq!(s.volume_ids().unwrap(), ["/a", "/b"]);
}
