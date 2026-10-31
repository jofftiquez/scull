//! Seeded demo history (feature `fixtures`): 60 days of believable scans for tests, CLI
//! screenshots and the desktop mock bridge. Writes **only** to the given [`Store`] (a database
//! in a temp dir or a throwaway `SCULL_HOME`); it never touches the filesystem it describes.
//!
//! The story, relative to `now` (all paths under `root`):
//!
//! * Daily completed scans at `now - d days - 1 h`, `d = 59..=0`.
//! * Five npm projects (`shop-api`, `web`, `admin`, `docs`, `blog`) each with installed
//!   dependencies; `shop-api/node_modules` is cleaned and rebuilt four times in the last 30
//!   days (scans at days 27, 20, 13 and 6 miss it; it is back the next day).
//! * Three Rust projects; `engine/target` grows from 2 to 9 GiB.
//! * `ml-lab/.venv` (Python) grows from 1 to 6 GiB over the last two weeks.
//! * An npm cache outside any project.
//! * Claude Code worktrees under `scull/.claude/worktrees/agent-<n>` created every three days,
//!   most living nine days; two Codex worktrees on `codex/` branches.
//! * Four cleanups (days 27, 20, 13, 6) with predicted vs. measured bytes; each removed
//!   `shop-api/node_modules`, which then came back.
//! * Free space on `/` (500 GiB) falling 2.5 GiB a day from 180 GiB, with jumps after each
//!   cleanup.

use scull_core::{
    Action, Candidate, Category, Confidence, FileIdentity, Flags, FsPath, ItemId, OperationId,
    Plan, PlanId, PlanItem, PlanOrigin, PlanParams, ProjectRef, ReasonCode, Reclaimable,
    Regeneration, RuleRef, ScanId, Sizes, Tier, TierReason,
};
use scull_storage::{
    ApprovalMethod, DefaultPermissions, ItemOutcome, ItemRecord, OperationEnd, OperationFinish,
    OperationStart, Result, ScanFinish, ScanRecord, ScanStart, ScanStatus, ScanTotals, Store,
    VolumeSample, rollups_from_candidates,
};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const DAY: i64 = 86_400;
const HOUR: i64 = 3_600;

/// Days of history [`seed_demo`] records.
pub const DEMO_DAYS: i64 = 60;
/// Scan root used by [`seed_demo`].
pub const DEMO_ROOT: &str = "/home/dev/work";
/// Volume sampled by [`seed_demo`].
pub const DEMO_VOLUME: &str = "/";
/// Days (before now) of the four demo cleanups.
pub const DEMO_CLEANUP_DAYS: [i64; 4] = [27, 20, 13, 6];
/// Predicted and measured GiB (×10) of the demo cleanups, in order.
const CLEANUPS: [(u64, u64); 4] = [(126, 119), (71, 28), (228, 219), (94, 90)];

/// A candidate for the demo history.
#[allow(clippy::too_many_arguments)]
pub fn candidate(
    path: &str,
    file: u128,
    category: Category,
    artifact: &str,
    project: Option<(&str, &str)>,
    tier: Tier,
    bytes: u64,
) -> Candidate {
    let identity = FileIdentity::new(42, file);
    let action = match tier {
        Tier::S0 => Action::Delete,
        Tier::S1 | Tier::S2 => Action::Quarantine,
        Tier::S3 => Action::None,
    };
    let project = project.map(|(root, eco)| {
        let root = FsPath::from(root);
        ProjectRef {
            name: root
                .as_str()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned(),
            root,
            ecosystem: eco.to_owned(),
        }
    });
    Candidate {
        id: ItemId::derive(identity, path.as_bytes()),
        path: FsPath::from(path),
        identity,
        category,
        artifact: artifact.to_owned(),
        rule: RuleRef {
            id: "demo.rule".into(),
            version: 1,
        },
        project,
        tier,
        reasons: vec![TierReason::new(tier, ReasonCode::Regenerable, "demo")],
        evidence: vec![],
        sizes: Sizes {
            logical: bytes,
            allocated: bytes,
            reclaimable: Reclaimable {
                bytes,
                confidence: Confidence::High,
                shared_bytes: 0,
            },
        },
        flags: Flags::default(),
        regeneration: Regeneration::default(),
        action,
    }
}

/// Record one completed scan of `roots` finished at `at` that saw `cands`, with `volumes`.
pub fn record_scan(
    store: &mut Store,
    scan: &ScanId,
    at: i64,
    roots: &[&str],
    cands: &[Candidate],
    volumes: &[VolumeSample],
) -> Result<()> {
    store.record_scan_start(&ScanStart {
        scan_id: scan.clone(),
        started_at: at - 60,
        roots: roots.iter().map(|r| FsPath::from(*r)).collect(),
    })?;
    let sum = |f: fn(&Candidate) -> u64| cands.iter().map(f).sum::<u64>();
    store.record_scan(&ScanRecord {
        finish: ScanFinish {
            scan_id: scan.clone(),
            finished_at: at,
            status: ScanStatus::Completed,
            totals: ScanTotals {
                entries_scanned: 10_000,
                candidates: cands.len() as u64,
                logical_bytes: sum(|c| c.sizes.logical),
                allocated_bytes: sum(|c| c.sizes.allocated),
                reclaimable_bytes: sum(|c| c.sizes.reclaimable.bytes),
                errors: 0,
            },
            rollups: rollups_from_candidates(cands)?,
        },
        candidates: cands,
        volumes,
    })?;
    Ok(())
}

/// Record a finished cleanup operation of `item` at `at` with predicted and measured bytes.
pub fn record_cleanup(
    store: &mut Store,
    n: u32,
    at: i64,
    item: &Candidate,
    scan: &ScanId,
    predicted: u64,
    measured: u64,
) -> Result<()> {
    let mut planned = item.clone();
    planned.sizes.reclaimable.bytes = predicted;
    let items = vec![PlanItem::from_candidate(&planned).map_err(core)?];
    let plan = Plan::new(
        PlanParams {
            id: PlanId::from_suffix(&format!("demo{n}")).map_err(bad_id)?,
            scan_id: scan.clone(),
            created_unix: at - 600,
            ttl_secs: None,
            machine_id: "demo-machine".into(),
            user: "dev".into(),
            origin: PlanOrigin::Cli,
        },
        items,
    )
    .map_err(core)?;
    store.insert_plan(&plan)?;
    let op = OperationId::from_suffix(&format!("demo{n}")).map_err(bad_id)?;
    let mut audit = store.begin_operation(
        &OperationStart {
            operation_id: op,
            plan_id: plan.id().clone(),
            content_hash: plan.content_hash().clone(),
            approver: "dev".into(),
            method: ApprovalMethod::CliTty,
            started_at: at,
        },
        &DefaultPermissions,
    )?;
    // The predicted bytes come from the plan item; the measured ones are what was freed.
    let mut rec = ItemRecord::new(item.id.clone(), at + 30, ItemOutcome::Removed);
    rec.freed_bytes = Some(measured);
    rec.entries_removed = Some(1);
    rec.entries_kept = Some(0);
    store.record_item(&mut audit, &rec)?;
    store.finish_operation(
        &mut audit,
        &OperationFinish {
            finished_at: at + 31,
            end: OperationEnd::Normal,
            measured_delta_bytes: None,
        },
    )?;
    Ok(())
}

fn bad_id(message: String) -> scull_storage::StoreError {
    scull_storage::StoreError::Rejected {
        kind: scull_core::ErrorKind::Internal,
        message,
    }
}

fn core(e: scull_core::ScullError) -> scull_storage::StoreError {
    scull_storage::StoreError::Rejected {
        kind: e.kind,
        message: e.message,
    }
}

fn lerp(from: u64, to: u64, step: i64, steps: i64) -> u64 {
    let step = step.clamp(0, steps) as u64;
    from + (to - from) * step / steps.max(1) as u64
}

/// The candidates seen on day `age` (days before now) plus the `shop-api/node_modules`
/// generation alive then (`None` while it is cleaned).
fn day_candidates(root: &str, age: i64) -> (Vec<Candidate>, Option<Candidate>) {
    let p = |rel: &str| format!("{root}/{rel}");
    let mut out = Vec::new();
    let elapsed = DEMO_DAYS - 1 - age;

    // npm projects; shop-api's node_modules is rebuilt after each cleanup (new identity).
    let generation = DEMO_CLEANUP_DAYS.iter().filter(|d| age < **d).count() as u128;
    let shop = (!DEMO_CLEANUP_DAYS.contains(&age)).then(|| {
        candidate(
            &p("shop-api/node_modules"),
            100 + generation,
            Category::ProjectArtifact,
            "Installed npm dependencies",
            Some((&p("shop-api"), "node")),
            Tier::S0,
            1_200 * MIB,
        )
    });
    out.extend(shop.clone());
    for (i, (name, mib)) in [("web", 900), ("admin", 700), ("docs", 450), ("blog", 380)]
        .into_iter()
        .enumerate()
    {
        out.push(candidate(
            &p(&format!("{name}/node_modules")),
            200 + i as u128,
            Category::ProjectArtifact,
            "Installed npm dependencies",
            Some((&p(name), "node")),
            Tier::S0,
            mib * MIB,
        ));
    }
    out.push(candidate(
        &p("web/.next"),
        210,
        Category::ProjectArtifact,
        "Next.js build output",
        Some((&p("web"), "node")),
        Tier::S0,
        lerp(200 * MIB, 1_400 * MIB, elapsed, DEMO_DAYS - 1),
    ));

    // Rust projects.
    for (i, (name, from, to)) in [
        ("engine", 2 * GIB, 9 * GIB),
        ("tools", 1_500 * MIB, 1_500 * MIB),
        ("cli", 3 * GIB, 3 * GIB),
    ]
    .into_iter()
    .enumerate()
    {
        out.push(candidate(
            &p(&format!("{name}/target")),
            300 + i as u128,
            Category::ProjectArtifact,
            "Cargo build output",
            Some((&p(name), "rust")),
            Tier::S0,
            lerp(from, to, elapsed, DEMO_DAYS - 1),
        ));
    }

    // Python: the virtual environment appears two weeks ago and grows.
    if age <= 14 {
        out.push(candidate(
            &p("ml-lab/.venv"),
            400,
            Category::ProjectArtifact,
            "Python virtual environment",
            Some((&p("ml-lab"), "python")),
            Tier::S0,
            lerp(GIB, 6 * GIB, 14 - age, 14),
        ));
    }

    // A package cache outside any project.
    out.push(candidate(
        "/home/dev/.npm/_cacache",
        500,
        Category::PackageCache,
        "npm cache",
        None,
        Tier::S0,
        lerp(2 * GIB, 3 * GIB, elapsed, DEMO_DAYS - 1),
    ));

    // Agent worktrees: one every three days from day 57; most live nine days, every fifth
    // one is never removed.
    for k in 0..19i64 {
        let born = 57 - 3 * k;
        let keeps = k % 5 == 0;
        if age <= born && (keeps || age > born - 9) {
            out.push(candidate(
                &p(&format!("scull/.claude/worktrees/agent-{k:02}")),
                600 + k as u128,
                Category::Worktree,
                &format!("Git worktree (claude/task-{k:02})"),
                Some((&p("scull"), "git")),
                Tier::S1,
                (250 + 10 * k as u64) * MIB,
            ));
        }
    }
    for k in 0..2i64 {
        if age <= 10 + 5 * k {
            out.push(candidate(
                &p(&format!("scull-codex-{k}")),
                700 + k as u128,
                Category::Worktree,
                &format!("Git worktree (codex/feature-{k})"),
                Some((&p("scull"), "git")),
                Tier::S1,
                400 * MIB,
            ));
        }
    }
    (out, shop)
}

/// Seed the demo history (see the module docs) ending at `now`, under `root`.
pub fn seed_demo_at(store: &mut Store, now: i64, root: &str) -> Result<()> {
    let total = 500 * GIB;
    let mut free = 180 * GIB;
    let mut previous_shop: Option<Candidate> = None;
    for (n, age) in (0..DEMO_DAYS).rev().enumerate() {
        let at = now - age * DAY - HOUR;
        let scan = ScanId::from_suffix(&format!("demo{n:03}")).map_err(bad_id)?;
        if let Some(i) = DEMO_CLEANUP_DAYS.iter().position(|d| *d == age)
            && let Some(item) = &previous_shop
        {
            let (pred, meas) = CLEANUPS[i];
            let (pred, meas) = (pred * GIB / 10, meas * GIB / 10);
            record_cleanup(store, i as u32 + 1, at - 2 * HOUR, item, &scan, pred, meas)?;
            free = (free + meas).min(total);
        }
        free = free.saturating_sub(2_560 * MIB); // 2.5 GiB a day
        let (cands, shop) = day_candidates(root, age);
        let volumes = [VolumeSample {
            volume_id: DEMO_VOLUME.into(),
            total,
            free,
            at,
        }];
        record_scan(store, &scan, at, &[root], &cands, &volumes)?;
        if shop.is_some() {
            previous_shop = shop;
        }
    }
    Ok(())
}

/// [`seed_demo_at`] under [`DEMO_ROOT`].
pub fn seed_demo(store: &mut Store, now: i64) -> Result<()> {
    seed_demo_at(store, now, DEMO_ROOT)
}
