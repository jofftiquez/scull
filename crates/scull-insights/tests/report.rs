//! Insights over seeded histories (P4-INS-01; INS-02, INS-03, DATA-05). Every database lives
//! in a temp dir; the demo history describes paths that do not exist and nothing reads them.
#![allow(clippy::unwrap_used)]

use scull_core::{Category, FsPath, ScanId};
use scull_insights::fixtures::{DEMO_ROOT, candidate, record_scan, seed_demo};
use scull_insights::time::{DAY, iso_month};
use scull_insights::{InsightsReport, agent_of, compute, csv, read, report};
use scull_storage::{DEFAULT_BUDGET_BYTES, DefaultPermissions, Store, VolumeSample};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// 2027-01-15 08:00 UTC.
const NOW: i64 = 1_800_000_000;

fn store(dir: &tempfile::TempDir) -> Store {
    Store::open(
        &dir.path().join(scull_storage::DB_FILE_NAME),
        &DefaultPermissions,
    )
    .unwrap()
}

fn demo(window_days: i64) -> (tempfile::TempDir, Store, InsightsReport) {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(&dir);
    seed_demo(&mut s, NOW).unwrap();
    let r = report(&s, NOW, NOW - window_days * DAY, Some(DEFAULT_BUDGET_BYTES)).unwrap();
    (dir, s, r)
}

fn p(rel: &str) -> String {
    format!("{DEMO_ROOT}/{rel}")
}

#[test]
fn fx_ins_04_top_growers_per_category_and_project_over_7_and_30_days() {
    let (_d, _s, r) = demo(30);
    assert_eq!(r.window_days, 30);
    assert!(r.coverage.covers_window);
    assert_eq!(r.growth.iter().map(|g| g.days).collect::<Vec<_>>(), [7, 30]);

    let w30 = &r.growth[1];
    assert_eq!(w30.categories[0].key, "project_artifact");
    let projects: Vec<&str> = w30.projects.iter().map(|g| g.label.as_str()).collect();
    // ml-lab's .venv appeared in the window (6 GiB), engine/target grew ~3.6 GiB.
    assert_eq!(projects[0], "ml-lab", "{projects:?}");
    assert_eq!(projects[1], "engine", "{projects:?}");
    assert_eq!(w30.projects[0].start_bytes, 0);
    assert_eq!(w30.projects[0].end_bytes, 6 * GIB);
    assert_eq!(w30.projects[0].ecosystem.as_deref(), Some("python"));
    assert_eq!(w30.projects[0].key, p("ml-lab"));
    // Shares are of all growth, and add up to at most 1.
    let share: f64 = w30.categories.iter().map(|g| g.share_of_growth).sum();
    assert!((0.999..=1.001).contains(&share), "{share}");
    // Stable projects never show up as growers.
    assert!(!projects.contains(&"tools") && !projects.contains(&"admin"));

    let w7 = &r.growth[0];
    assert_eq!(w7.projects[0].label, "ml-lab");
    assert!(
        w7.projects[0].growth_bytes < 6 * GIB,
        "only part of its growth is in 7 days"
    );
}

#[test]
fn fx_ins_05_regrowth_counts_rebuilds_and_suggests_excluding_the_artifact() {
    let (_d, _s, r) = demo(30);
    let shop = &r.regrowth[0];
    assert_eq!(shop.path.as_str(), p("shop-api/node_modules"));
    assert_eq!(shop.category, Category::ProjectArtifact);
    assert_eq!(
        shop.rebuilds, 4,
        "cleaned and rebuilt four times this month"
    );
    assert_eq!(shop.restores, 0);
    assert_eq!(shop.project.as_deref(), Some("shop-api"));
    // Last seen two days before it was seen again, every time.
    assert_eq!(shop.median_back_within_secs, Some(2 * DAY));
    assert!(shop.suggest_exclude);
    let s = shop.suggestion.as_deref().unwrap();
    assert!(s.contains("4 times") && s.contains("pnpm"), "{s}");
    // A 7-day window holds one rebuild only: no suggestion.
    let (_d, _s, r7) = demo(7);
    assert_eq!(r7.regrowth[0].rebuilds, 1);
    assert!(!r7.regrowth[0].suggest_exclude);
}

#[test]
fn fx_ins_06_duplication_across_projects_with_store_once_suggestions() {
    let (_d, _s, r) = demo(30);
    let kinds: Vec<(&str, &str, u32)> = r
        .duplication
        .iter()
        .map(|d| (d.ecosystem.as_str(), d.artifact.as_str(), d.projects))
        .collect();
    assert_eq!(
        kinds,
        [
            ("rust", "Cargo build output", 3),
            ("node", "Installed npm dependencies", 5),
        ],
        "a single .venv is not duplication"
    );
    let node = &r.duplication[1];
    assert_eq!(node.copies, 5);
    assert_eq!(node.total_bytes, (1_200 + 900 + 700 + 450 + 380) * MIB);
    assert_eq!(node.largest_bytes, 1_200 * MIB);
    assert!(node.suggestion.as_deref().unwrap().contains("pnpm"));
    assert!(
        r.duplication[0]
            .suggestion
            .as_deref()
            .unwrap()
            .contains("CARGO_TARGET_DIR")
    );
}

#[test]
fn fx_ins_07_agent_footprint_weeks_lifetime_and_agents() {
    let (_d, _s, r) = demo(30);
    let a = &r.agent_footprint;
    assert_eq!(a.weeks.len(), 8);
    assert!(
        a.weeks
            .windows(2)
            .all(|w| w[1].week_start - w[0].week_start == 7 * DAY)
    );
    // Every worktree in the demo belongs to an agent.
    assert_eq!(a.agent_worktrees, a.worktrees);
    assert_eq!(a.agent_bytes, a.worktree_bytes);
    let agents: Vec<(&str, u32)> = a
        .by_agent
        .iter()
        .map(|x| (x.agent.as_str(), x.worktrees))
        .collect();
    assert_eq!(agents[0].0, "claude-code");
    assert_eq!(agents[1], ("codex", 2));
    // Claude worktrees alive now: the three born in the last nine days plus every fifth one
    // that was never removed (k = 0, 5, 10, 15).
    assert_eq!(agents[0].1, 3 + 4 - 1, "k = 15 is both recent and kept");
    // Removed ones lived nine days.
    assert_eq!(a.average_agent_lifetime_secs, Some(9 * DAY));
    let created: u32 = a.weeks.iter().map(|w| w.created).sum();
    assert!(created >= 18, "{created}");
    assert!(a.weeks.iter().all(|w| w.agent_created == w.created));
}

#[test]
fn fx_ins_08_accuracy_predicted_vs_measured_and_rebuild_cost() {
    let (_d, _s, r) = demo(30);
    let acc = &r.accuracy;
    assert_eq!(acc.operations.len(), 4);
    assert!(acc.operations.windows(2).all(|w| w[0].at < w[1].at));
    let gib10 = |x: u64| x * GIB / 10;
    assert_eq!(acc.operations[1].predicted_bytes, gib10(71));
    assert_eq!(acc.operations[1].measured_bytes, gib10(28));
    let e = acc.operations[1].error.unwrap();
    assert!((e - (28.0 - 71.0) / 71.0).abs() < 1e-3, "{e}");
    // |errors|: 5.6%, 60.6%, 3.9%, 4.3% -> median of 4.3% and 5.6%.
    let m = acc.median_abs_error.unwrap();
    assert!((m - 0.0494).abs() < 0.001, "{m}");
    // Every cleaned node_modules was rebuilt afterwards.
    assert_eq!(acc.regrown_items, 4);
    assert_eq!(
        acc.predicted_bytes,
        gib10(126) + gib10(71) + gib10(228) + gib10(94)
    );
}

#[test]
fn fx_ins_09_ecosystem_mix_by_month() {
    let (_d, _s, r) = demo(30);
    assert_eq!(r.ecosystem_mix.len(), 6);
    let months: Vec<&str> = r.ecosystem_mix.iter().map(|m| m.month.as_str()).collect();
    assert_eq!(
        months,
        [
            "2026-08", "2026-09", "2026-10", "2026-11", "2026-12", "2027-01"
        ]
    );
    assert_eq!(
        r.ecosystem_mix[0].total_bytes, 0,
        "before the history starts"
    );
    let now = r.ecosystem_mix.last().unwrap();
    assert_eq!(now.at, NOW);
    let names: Vec<&str> = now
        .ecosystems
        .iter()
        .map(|e| e.ecosystem.as_str())
        .collect();
    for e in ["rust", "node", "python", "git", "other"] {
        assert!(names.contains(&e), "{e} in {names:?}");
    }
    let share: f64 = now.ecosystems.iter().map(|e| e.share).sum();
    assert!((0.999..=1.001).contains(&share));
    // Python appeared this month only.
    let dec = &r.ecosystem_mix[4];
    assert!(!dec.ecosystems.iter().any(|e| e.ecosystem == "python"));
    assert_eq!(iso_month(dec.at), "2026-12");
}

#[test]
fn fx_ins_10_free_space_trend_and_own_footprint_against_the_budget() {
    let (_d, s, r) = demo(30);
    assert_eq!(r.free_space.len(), 1);
    let v = &r.free_space[0];
    assert_eq!(v.volume_id, "/");
    assert_eq!(v.total_bytes, 500 * GIB);
    assert_eq!(v.points.len(), 30, "one point per day");
    assert!(v.points.windows(2).all(|w| w[0].at < w[1].at));
    // Falling 2.5 GiB a day, partly offset by the four cleanups in the window.
    assert!(v.change_bytes < 0);
    // The forecast is scull-proactive's (PRO-01): filling, with cleanups levelled out.
    let fc = &v.forecast;
    assert_eq!(fc.volume_id, "/");
    assert_eq!(fc.state, scull_proactive::ForecastState::Filling, "{fc:?}");
    let days = fc.days_until_threshold.unwrap();
    assert!((15.0..40.0).contains(&days), "{fc:?}");
    assert!(fc.jumps_ignored >= 3, "{fc:?}");
    let f = &r.own_footprint;
    assert!(f.database_bytes > 0);
    assert_eq!(
        f.total_bytes,
        f.database_bytes + f.wal_bytes + f.backup_bytes + f.damaged_bytes
    );
    assert_eq!(f.budget_bytes, Some(DEFAULT_BUDGET_BYTES));
    assert_eq!(f.budget_used_bytes, s.budget_bytes_used().unwrap());
    assert_eq!(f.points.len(), 30);
}

#[test]
fn fx_ins_11_empty_history_gives_an_empty_report() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(&dir);
    let r = report(&s, NOW, NOW - 30 * DAY, None).unwrap();
    assert!(!r.coverage.covers_window);
    assert_eq!(r.coverage.items, 0);
    assert!(
        r.growth
            .iter()
            .all(|g| g.projects.is_empty() && g.net_bytes == 0)
    );
    assert!(r.regrowth.is_empty() && r.duplication.is_empty());
    assert!(r.accuracy.operations.is_empty() && r.accuracy.median_abs_error.is_none());
    assert_eq!(r.agent_footprint.worktrees, 0);
    assert!(r.free_space.is_empty(), "no volume, no forecast");
    assert!(r.notes.iter().any(|n| n.contains("does not reach")));
}

#[test]
fn fx_ins_12_csv_export_is_one_tidy_table_and_formula_safe() {
    let (_d, _s, r) = demo(30);
    let out = csv::to_csv(&r);
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some(csv::HEADER));
    assert!(lines.clone().all(|l| !l.is_empty()));
    assert!(out.contains(&format!(
        "growth_project,30d,{},end_bytes,{}\n",
        p("ml-lab"),
        6 * GIB
    )));
    assert!(out.contains("regrowth,30d,/home/dev/work/shop-api/node_modules,rebuilds,4\n"));
    assert!(out.contains("duplication,,node/Installed npm dependencies,projects,5\n"));
    assert!(out.contains("agent_footprint,,codex,worktrees,2\n"));
    assert!(out.contains("ecosystem_mix,2027-01,python,bytes,"));
    assert!(out.contains("free_space,2027-01-15,/,free_bytes,"));

    // A crafted folder name is never a formula in a spreadsheet.
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(&dir);
    let evil = "=HYPERLINK(\"http://x\",\"y\")";
    for (n, (age, file)) in [(2, 1), (1, 2), (0, 3)].into_iter().enumerate() {
        let c = if age == 1 {
            vec![]
        } else {
            vec![candidate(
                evil,
                file,
                Category::ProjectArtifact,
                "x",
                None,
                scull_core::Tier::S0,
                MIB,
            )]
        };
        let scan = ScanId::from_suffix(&format!("e{n}")).unwrap();
        record_scan(&mut s, &scan, NOW - age * DAY, &["="], &c, &[]).unwrap();
    }
    let r = report(&s, NOW, NOW - 30 * DAY, None).unwrap();
    assert_eq!(r.regrowth[0].path.as_str(), evil);
    let out = csv::to_csv(&r);
    assert!(
        out.contains("regrowth,30d,\"'=HYPERLINK(\"\"http://x\"\",\"\"y\"\")\",rebuilds,1\n"),
        "{out}"
    );
}

#[test]
fn fx_ins_13_reports_are_read_only_and_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(&dir);
    seed_demo(&mut s, NOW).unwrap();
    let db = dir.path().join(scull_storage::DB_FILE_NAME);
    let snapshot = || {
        let wal = db.with_file_name(format!("{}-wal", scull_storage::DB_FILE_NAME));
        (
            std::fs::read(&db).unwrap(),
            std::fs::read(&wal).unwrap_or_default(),
        )
    };
    let before = snapshot();
    let input = read(&s, NOW, NOW - 30 * DAY, None).unwrap();
    assert_eq!(snapshot(), before, "reading changes nothing");
    let a = compute(&input);
    assert_eq!(compute(&input), a, "pure");
    // JSON round trip (the schema's shape).
    let json = serde_json::to_string(&a).unwrap();
    let back: InsightsReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back, a);
}

#[test]
fn fx_ins_14_window_without_history_start_is_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = store(&dir);
    let c = candidate(
        &p("a/target"),
        1,
        Category::ProjectArtifact,
        "Cargo build output",
        Some((&p("a"), "rust")),
        scull_core::Tier::S0,
        GIB,
    );
    let vol = |at| VolumeSample {
        volume_id: "/".into(),
        total: 10 * GIB,
        free: GIB,
        at,
    };
    record_scan(
        &mut s,
        &ScanId::from_suffix("x1").unwrap(),
        NOW - 3 * DAY,
        &[DEMO_ROOT],
        std::slice::from_ref(&c),
        &[vol(NOW - 3 * DAY)],
    )
    .unwrap();
    let r = report(&s, NOW, NOW - 30 * DAY, None).unwrap();
    assert!(!r.coverage.covers_window);
    assert_eq!(r.coverage.history_starts_at, Some(NOW - 3 * DAY));
    // The item appeared inside the window: it is growth.
    assert_eq!(r.growth[1].projects[0].growth_bytes, GIB);
}

#[test]
fn agent_markers_in_paths_and_branches() {
    let f = |p: &str, a: &str| agent_of(&FsPath::from(p), a);
    assert_eq!(
        f("/r/.claude/worktrees/agent-1", "Git worktree (main)"),
        Some("claude-code")
    );
    assert_eq!(f("C:\\r\\.claude\\worktrees\\x", ""), Some("claude-code"));
    assert_eq!(f("/r/wt", "Git worktree (codex/fix)"), Some("codex"));
    assert_eq!(f("/r/.cursor/wt", ""), Some("cursor"));
    assert_eq!(
        f("/r/claude-notes/wt", "Git worktree (feature/claude)"),
        None
    );
    assert_eq!(f("/r/wt", "Git worktree (detached HEAD)"), None);
}
