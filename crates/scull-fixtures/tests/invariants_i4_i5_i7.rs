//! Self-tests for invariants I-4 (audit matches disk), I-5 (predicted and measured bytes
//! recorded) and I-7 (only allowlisted tools spawned, from trusted paths): each one passes on a
//! consistent run and fails on a deliberate violation, and is "not applicable" (never passed)
//! without its input.
//!
//! The audit is a real `scull-storage` store beside the fixture root, with a real plan of items
//! inside the root; the "execution" is done by hand so every kind of disagreement can be staged.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::{
    Action, Category, FsPath, ItemId, OperationId, Plan, PlanId, PlanItem, PlanOrigin, PlanParams,
    RuleRef, ScanId, Tier,
};
use scull_fixtures::{
    Fixture, InvariantId, InvariantInputs, InvariantReport, Outcome, SpawnRecord,
};
use scull_platform::{Native, Platform, native_path_bytes};
use scull_storage::{
    ApprovalMethod, DefaultPermissions, ItemOutcome, ItemRecord, OperationAudit, OperationEnd,
    OperationFinish, OperationStart, Store,
};
use std::fs;
use std::path::{Path, PathBuf};

const T0: i64 = 1_800_000_000;
const PREDICTED: u64 = 4096;

/// Three candidate folders `a`, `b`, `c` (two files each) and a sibling `keep`.
fn fixture() -> Fixture {
    let mut fx = Fixture::new().unwrap();
    for d in ["a", "b", "c"] {
        fx.file_of_size(format!("{d}/one.bin"), PREDICTED / 2)
            .unwrap()
            .file_of_size(format!("{d}/sub/two.bin"), PREDICTED / 2)
            .unwrap();
    }
    fx.file("keep/notes.txt", "stays").unwrap();
    fx.checkpoint().unwrap();
    fx
}

fn item_at(path: &Path) -> PlanItem {
    let identity = Native.metadata(path).unwrap().identity.expect("identity");
    PlanItem {
        item_id: ItemId::derive(identity, &native_path_bytes(path)),
        identity,
        path: FsPath::from_path(path),
        category: Category::ProjectArtifact,
        rule: RuleRef {
            id: "test.invariants".into(),
            version: 1,
        },
        tier: Tier::S0,
        action: Action::Delete,
        reclaimable_bytes: PREDICTED,
    }
}

/// A recorded plan and a started operation in a store beside the root.
struct Run {
    store: Store,
    audit: OperationAudit,
    op: OperationId,
    items: Vec<PlanItem>,
}

impl Run {
    fn begin(fx: &Fixture, paths: &[PathBuf]) -> Self {
        let mut store = Store::open(
            &fx.base().join("data").join("scull.db"),
            &DefaultPermissions,
        )
        .unwrap();
        let items: Vec<PlanItem> = paths.iter().map(|p| item_at(p)).collect();
        let plan = Plan::new(
            PlanParams {
                id: PlanId::from_suffix("PINV1").unwrap(),
                scan_id: ScanId::from_suffix("SINV1").unwrap(),
                created_unix: T0,
                ttl_secs: None,
                machine_id: "machine-test".into(),
                user: "tester".into(),
                origin: PlanOrigin::Cli,
            },
            items.clone(),
        )
        .unwrap();
        store.insert_plan(&plan).unwrap();
        let op = OperationId::from_suffix("OINV1").unwrap();
        let audit = store
            .begin_operation(
                &OperationStart {
                    operation_id: op.clone(),
                    plan_id: plan.id().clone(),
                    content_hash: plan.content_hash().clone(),
                    approver: "tester".into(),
                    method: ApprovalMethod::CliInteractive,
                    started_at: T0 + 1,
                },
                &DefaultPermissions,
            )
            .unwrap();
        // Plan::new may reorder; look items up by path below.
        Self {
            store,
            audit,
            op,
            items,
        }
    }

    fn id(&self, path: &Path) -> ItemId {
        let p = FsPath::from_path(path);
        self.items
            .iter()
            .find(|i| i.path == p)
            .unwrap()
            .item_id
            .clone()
    }

    fn record(&mut self, path: &Path, outcome: ItemOutcome) {
        let mut rec = ItemRecord::new(self.id(path), T0 + 2, outcome);
        match outcome {
            ItemOutcome::Removed => rec.freed_bytes = Some(PREDICTED),
            ItemOutcome::Partial => {
                rec.freed_bytes = Some(PREDICTED / 2);
                rec.reason = Some("incomplete".into());
            }
            ItemOutcome::Skipped | ItemOutcome::Failed | ItemOutcome::Refused => {
                rec.reason = Some("changed_since_plan".into());
            }
            _ => {}
        }
        self.store.record_item(&mut self.audit, &rec).unwrap();
    }

    fn finish(&mut self, end: OperationEnd, measured: Option<i64>) {
        self.store
            .finish_operation(
                &mut self.audit,
                &OperationFinish {
                    finished_at: T0 + 3,
                    end,
                    measured_delta_bytes: measured,
                },
            )
            .unwrap();
    }

    fn inputs(&self) -> InvariantInputs {
        InvariantInputs::new()
            .plan(
                self.items
                    .iter()
                    .map(|i| (i.item_id.clone(), i.tier, i.action.clone())),
            )
            .audit(&self.store, &self.op)
            .operation_bytes(&self.store, &self.op)
    }
}

fn failures(report: &InvariantReport) -> Vec<InvariantId> {
    report.failures().iter().map(|(i, _)| *i).collect()
}

fn failure_text(report: &InvariantReport, id: InvariantId) -> String {
    match report.outcome(id) {
        Outcome::Failed(m) => m.clone(),
        other => panic!("{id} did not fail: {other:?}\n{report}"),
    }
}

// ---- I-4 ------------------------------------------------------------------------------------

/// Removed → gone, skipped → unchanged, partial → still there with something below gone: the
/// audit matches the disk, and I-5 holds (predicted from the plan, measured recorded).
#[test]
fn i4_i5_pass_when_every_outcome_matches_the_disk() {
    let fx = fixture();
    let (a, b, c) = (
        fx.root().join("a"),
        fx.root().join("b"),
        fx.root().join("c"),
    );
    let mut run = Run::begin(&fx, &[a.clone(), b.clone(), c.clone()]);
    fs::remove_dir_all(&a).unwrap();
    run.record(&a, ItemOutcome::Removed);
    run.record(&b, ItemOutcome::Skipped);
    fs::remove_file(c.join("sub/two.bin")).unwrap();
    run.record(&c, ItemOutcome::Partial);
    run.finish(OperationEnd::Normal, Some(6000));

    let report = fx.check_invariants(&run.inputs().allow("a").allow("c"));
    println!("{report}");
    report.assert_ok();
    for id in [
        InvariantId::I1,
        InvariantId::I3,
        InvariantId::I4,
        InvariantId::I5,
    ] {
        assert_eq!(report.outcome(id), &Outcome::Passed, "{id}: {report}");
    }
}

/// The audit says "removed" but the folder is still on disk.
#[test]
fn i4_fails_when_audit_says_removed_but_path_still_exists() {
    let fx = fixture();
    let a = fx.root().join("a");
    let mut run = Run::begin(&fx, std::slice::from_ref(&a));
    run.record(&a, ItemOutcome::Removed);
    run.finish(OperationEnd::Normal, Some(0));
    let report = fx.check_invariants(&run.inputs());
    println!("{report}");
    assert_eq!(failures(&report), vec![InvariantId::I4]);
    let m = failure_text(&report, InvariantId::I4);
    assert!(
        m.contains("audit says removed") && m.contains("Unchanged"),
        "{m}"
    );
}

/// The audit says "skipped" but something below the item changed (I-3 is told the change is
/// allowed, so only I-4 can catch it).
#[test]
fn i4_fails_when_skipped_item_changed_on_disk() {
    let fx = fixture();
    let b = fx.root().join("b");
    let mut run = Run::begin(&fx, std::slice::from_ref(&b));
    fs::remove_file(b.join("sub/two.bin")).unwrap();
    run.record(&b, ItemOutcome::Skipped);
    run.finish(OperationEnd::Normal, Some(2048));
    let report = fx.check_invariants(&run.inputs().allow("b"));
    println!("{report}");
    assert_eq!(failures(&report), vec![InvariantId::I4]);
    let m = failure_text(&report, InvariantId::I4);
    assert!(
        m.contains("audit says skipped") && m.contains("Changed"),
        "{m}"
    );
}

/// The audit says "removed" for a folder that is only partly gone.
#[test]
fn i4_fails_when_removed_item_is_only_partly_gone() {
    let fx = fixture();
    let a = fx.root().join("a");
    let mut run = Run::begin(&fx, std::slice::from_ref(&a));
    fs::remove_file(a.join("one.bin")).unwrap();
    run.record(&a, ItemOutcome::Removed);
    run.finish(OperationEnd::Normal, Some(2048));
    let report = fx.check_invariants(&run.inputs().allow("a"));
    assert_eq!(failures(&report), vec![InvariantId::I4], "{report}");
}

/// A plan item with no outcome (the operation never finished): a violation, and I-5 fails too
/// because no totals were recorded.
#[test]
fn i4_fails_on_plan_item_without_outcome() {
    let fx = fixture();
    let (a, b) = (fx.root().join("a"), fx.root().join("b"));
    let mut run = Run::begin(&fx, &[a.clone(), b.clone()]);
    fs::remove_dir_all(&a).unwrap();
    run.record(&a, ItemOutcome::Removed);
    let report = fx.check_invariants(&run.inputs().allow("a"));
    println!("{report}");
    assert_eq!(failures(&report), vec![InvariantId::I4, InvariantId::I5]);
    assert!(failure_text(&report, InvariantId::I4).contains("no audit outcome"));
    assert!(failure_text(&report, InvariantId::I5).contains("not finished"));
}

/// A cancelled operation may leave items without an outcome, as long as they were not
/// touched.
#[test]
fn i4_cancelled_operation_untouched_items_without_outcome_pass() {
    let fx = fixture();
    let (a, b) = (fx.root().join("a"), fx.root().join("b"));
    let mut run = Run::begin(&fx, &[a.clone(), b.clone()]);
    fs::remove_dir_all(&a).unwrap();
    run.record(&a, ItemOutcome::Removed);
    run.finish(OperationEnd::Cancelled, Some(4096));
    let report = fx.check_invariants(&run.inputs().allow("a"));
    println!("{report}");
    report.assert_ok();
    assert_eq!(report.outcome(InvariantId::I4), &Outcome::Passed);
}

/// ... but an item without an outcome that changed on disk is a violation even then.
#[test]
fn i4_fails_when_cancelled_operation_touched_an_item_without_outcome() {
    let fx = fixture();
    let (a, b) = (fx.root().join("a"), fx.root().join("b"));
    let mut run = Run::begin(&fx, &[a.clone(), b.clone()]);
    fs::remove_dir_all(&a).unwrap();
    run.record(&a, ItemOutcome::Removed);
    fs::remove_dir_all(&b).unwrap();
    run.finish(OperationEnd::Cancelled, Some(8192));
    let report = fx.check_invariants(&run.inputs().allow("a").allow("b"));
    println!("{report}");
    assert_eq!(failures(&report), vec![InvariantId::I4]);
    assert!(failure_text(&report, InvariantId::I4).contains("stopped operation"));
}

/// An item outside the fixture root can't be vouched for.
#[test]
fn i4_fails_for_item_outside_the_fixture_root() {
    let fx = fixture();
    let outside = fx.canary().file.clone();
    let mut run = Run::begin(&fx, std::slice::from_ref(&outside));
    run.record(&outside, ItemOutcome::Skipped);
    run.finish(OperationEnd::Normal, Some(0));
    let report = fx.check_invariants(&run.inputs());
    assert_eq!(failures(&report), vec![InvariantId::I4], "{report}");
    assert!(failure_text(&report, InvariantId::I4).contains("outside the fixture root"));
}

/// An operation the store doesn't know: I-4 and I-5 fail, they don't pass.
#[test]
fn i4_i5_fail_for_an_unknown_operation() {
    let fx = fixture();
    let store = Store::open(
        &fx.base().join("data").join("scull.db"),
        &DefaultPermissions,
    )
    .unwrap();
    let op = OperationId::from_suffix("ONOPE").unwrap();
    let report = fx.check_invariants(
        &InvariantInputs::new()
            .audit(&store, &op)
            .operation_bytes(&store, &op),
    );
    println!("{report}");
    assert_eq!(failures(&report), vec![InvariantId::I4, InvariantId::I5]);
}

// ---- I-5 ------------------------------------------------------------------------------------

/// The operation finished without a measured free-space delta.
#[test]
fn i5_fails_when_measured_delta_not_recorded() {
    let fx = fixture();
    let a = fx.root().join("a");
    let mut run = Run::begin(&fx, std::slice::from_ref(&a));
    fs::remove_dir_all(&a).unwrap();
    run.record(&a, ItemOutcome::Removed);
    run.finish(OperationEnd::Normal, None);
    let report = fx.check_invariants(&run.inputs().allow("a"));
    println!("{report}");
    assert_eq!(failures(&report), vec![InvariantId::I5]);
    let m = failure_text(&report, InvariantId::I5);
    assert!(m.contains("predicted 4096 B, measured not recorded"), "{m}");
}

/// Explicit byte records (an action without an operation, e.g. one Quick Clean tool).
#[test]
fn i5_explicit_bytes_pass_only_when_both_recorded() {
    let fx = fixture();
    let ok = fx.check_invariants(&InvariantInputs::new().bytes("npm", Some(65536), Some(-12)));
    assert_eq!(ok.outcome(InvariantId::I5), &Outcome::Passed, "{ok}");
    let bad = fx.check_invariants(
        &InvariantInputs::new()
            .bytes("npm", Some(65536), Some(60000))
            .bytes("pnpm", None, Some(0)),
    );
    println!("{bad}");
    assert_eq!(failures(&bad), vec![InvariantId::I5]);
    assert!(failure_text(&bad, InvariantId::I5).contains("pnpm: predicted not recorded"));
}

// ---- I-7 ------------------------------------------------------------------------------------

/// The fake trusted tools folder beside the root (outside root and canary).
fn tools(fx: &Fixture) -> PathBuf {
    let t = fx.base().join("tools");
    fs::create_dir_all(&t).unwrap();
    t
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

#[test]
fn i7_passes_for_allowlisted_tools_from_the_trusted_folder() {
    let fx = fixture();
    let t = tools(&fx);
    let report = fx.check_invariants(
        &InvariantInputs::new()
            .spawned([
                SpawnRecord::new(exe(&t, "npm"), ["cache", "verify"]),
                SpawnRecord::new(exe(&t, "pnpm"), ["store", "prune"]),
                SpawnRecord::from(t.join("yarn.EXE")),
            ])
            .trusted_dir(&t),
    );
    println!("{report}");
    assert_eq!(
        report.outcome(InvariantId::I7),
        &Outcome::Passed,
        "{report}"
    );
}

/// "Ran and started nothing" is checked and passes; no records at all is not applicable.
#[test]
fn i7_empty_records_pass_and_missing_records_are_not_applicable() {
    let fx = fixture();
    let empty: [SpawnRecord; 0] = [];
    let ran = fx.check_invariants(&InvariantInputs::new().spawned(empty));
    assert_eq!(ran.outcome(InvariantId::I7), &Outcome::Passed);
    let none = fx.check_invariants(&InvariantInputs::new());
    assert!(matches!(
        none.outcome(InvariantId::I7),
        Outcome::NotApplicable(_)
    ));
}

#[test]
fn i7_fails_for_a_tool_that_is_not_allowlisted() {
    let fx = fixture();
    let t = tools(&fx);
    let report = fx.check_invariants(
        &InvariantInputs::new()
            .spawned([SpawnRecord::new(exe(&t, "sh"), ["-c", "rm -rf ~"])])
            .trusted_dir(&t),
    );
    assert_eq!(failures(&report), vec![InvariantId::I7], "{report}");
    assert!(failure_text(&report, InvariantId::I7).contains("not an allowlisted tool"));
}

/// `ollama rm` (tool-native ML removal, P4-DET-01) is never run by scull: a spawn of it fails
/// I-7 even from a trusted folder, while the Quick Clean package managers pass.
#[test]
fn i7_allowlists_quick_clean_tools_but_not_ollama() {
    let fx = fixture();
    let t = tools(&fx);
    let report = fx.check_invariants(
        &InvariantInputs::new()
            .spawned([SpawnRecord::new(exe(&t, "ollama"), ["rm", "llama3:latest"])])
            .trusted_dir(&t),
    );
    assert_eq!(failures(&report), vec![InvariantId::I7], "{report}");
    for tool in ["pip", "uv", "conda"] {
        let report = fx.check_invariants(
            &InvariantInputs::new()
                .spawned([SpawnRecord::new(exe(&t, tool), ["cache"])])
                .trusted_dir(&t),
        );
        assert_eq!(
            report.outcome(InvariantId::I7),
            &Outcome::Passed,
            "{tool}: {report}"
        );
    }
}

/// FX-QC-06's attack: a fake `npm` in a project's `node_modules/.bin` (inside the scanned
/// root), or earlier in `PATH` from an untrusted folder.
#[test]
fn i7_fails_for_untrusted_locations() {
    let fx = fixture();
    let t = tools(&fx);
    let planted = exe(&fx.root().join("web/node_modules/.bin"), "npm");
    let report = fx.check_invariants(
        &InvariantInputs::new()
            .spawned([SpawnRecord::from(planted)])
            .trusted_dir(&t),
    );
    println!("{report}");
    assert_eq!(failures(&report), vec![InvariantId::I7]);
    let m = failure_text(&report, InvariantId::I7);
    assert!(
        m.contains("node_modules") && m.contains("inside the scanned root"),
        "{m}"
    );
    assert!(m.contains("not in a trusted directory"), "{m}");

    let earlier = exe(&fx.base().join("evil-bin"), "npm");
    let report = fx.check_invariants(
        &InvariantInputs::new()
            .spawned([SpawnRecord::from(earlier)])
            .trusted_dir(&t),
    );
    assert_eq!(failures(&report), vec![InvariantId::I7], "{report}");
}

#[test]
fn i7_fails_for_a_relative_binary_or_without_trusted_dirs() {
    let fx = fixture();
    let report = fx.check_invariants(
        &InvariantInputs::new().spawned([SpawnRecord::from(PathBuf::from("npm"))]),
    );
    println!("{report}");
    assert_eq!(failures(&report), vec![InvariantId::I7]);
    let m = failure_text(&report, InvariantId::I7);
    assert!(m.contains("not an absolute path"), "{m}");
    assert!(m.contains("no trusted directory declared"), "{m}");
}

/// P2-QC-02: Node is allowlisted only while it runs an allowlisted tool's script from a
/// trusted folder (a Windows `.cmd` shim resolved by Quick Clean).
#[test]
fn i7_node_only_runs_a_tool_script_from_the_trusted_folder() {
    let fx = fixture();
    let t = tools(&fx);
    let node = exe(&t, "node");
    let script = |rel: &str| {
        let p = t.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "// entry point\n").unwrap();
        p
    };
    let npm_cli = script("node_modules/npm/bin/npm-cli.js");
    let yarn_js = script("yarn.js");
    let other = script("node_modules/evil/index.js");
    let in_root = fx.root().join("web/node_modules/npm/bin/npm-cli.js");
    let ok = fx.check_invariants(
        &InvariantInputs::new()
            .spawned([
                SpawnRecord::new(&node, [npm_cli.as_os_str(), "cache".as_ref()]),
                SpawnRecord::new(&node, [yarn_js.as_os_str(), "--version".as_ref()]),
            ])
            .trusted_dir(&t),
    );
    assert_eq!(ok.outcome(InvariantId::I7), &Outcome::Passed, "{ok}");
    for (args, why) in [
        (vec![], "without a script"),
        (vec![other.into_os_string()], "not an allowlisted tool's"),
        (vec![in_root.into_os_string()], "inside the scanned root"),
        (
            vec!["-e".into(), "x".into()],
            "not an absolute JavaScript file",
        ),
    ] {
        let bad = fx.check_invariants(
            &InvariantInputs::new()
                .spawned([SpawnRecord::new(&node, args)])
                .trusted_dir(&t),
        );
        assert_eq!(failures(&bad), vec![InvariantId::I7], "{why}: {bad}");
        assert!(
            failure_text(&bad, InvariantId::I7).contains(why),
            "{why}: {bad}"
        );
    }
}

/// The hardened Git runner's resolved binary (the only binary it ever starts) is `git` from an
/// absolute `PATH` entry outside the fixture.
#[test]
fn i7_git_runner_resolved_binary_is_allowlisted() {
    let fx = fixture();
    let Ok(runner) = scull_git::GitRunner::new() else {
        println!("SKIP: no trusted git on this machine");
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut inputs = InvariantInputs::new().spawned([runner.resolved_binary()]);
    for dir in std::env::split_paths(&path).filter(|d| d.is_absolute()) {
        inputs = inputs.trusted_dir(dir);
    }
    let report = fx.check_invariants(&inputs);
    println!("{}: {report}", runner.resolved_binary().display());
    assert_eq!(
        report.outcome(InvariantId::I7),
        &Outcome::Passed,
        "{report}"
    );
}

// ---------------------------------------------------------------------------------------------
// I-2, P2-EXEC-13: nothing quarantined outside the fixture's temp dir.
// ---------------------------------------------------------------------------------------------

/// A recorded quarantine path outside the fixture's temp dir makes I-2 fail; one inside passes.
/// Only manifest rows are written (into the fixture's own store): nothing on disk outside.
#[test]
fn i2_fails_when_a_recorded_quarantine_path_is_outside_the_fixture() {
    let fx = fixture();
    // A recorded operation for the manifest rows to belong to.
    let run = Run::begin(&fx, &[fx.root().join("a")]);
    let (mut store, op) = (run.store, run.op);
    let entry = |q: &Path| scull_storage::NewQuarantineEntry {
        operation_id: op.clone(),
        item_id: ItemId::derive(scull_core::FileIdentity::new(1, 2), b"/x"),
        original_path: FsPath::from_path(&fx.root().join("a")),
        quarantine_path: FsPath::from_path(q),
        entry_identity: scull_core::FileIdentity::new(1, 2),
        entry_kind: "dir".into(),
        parent_identity: scull_core::FileIdentity::new(1, 3),
        bytes: 1,
        logical_bytes: 1,
        quarantined_at: T0,
        expires_at: None,
    };
    let inside = fx.base().join(".scull-quarantine").join("op").join("0001");
    store.insert_quarantine_entry(&entry(&inside)).unwrap();
    let report = fx.check_invariants(&InvariantInputs::new().quarantine(&store));
    assert_eq!(
        report.outcome(InvariantId::I2),
        &Outcome::Passed,
        "{report}"
    );

    // The automatic location on a CI runner: the folder above the temp dir.
    let outside = fx
        .base()
        .parent()
        .unwrap()
        .join(".scull-quarantine")
        .join("op")
        .join("0002");
    store.insert_quarantine_entry(&entry(&outside)).unwrap();
    let report = fx.check_invariants(&InvariantInputs::new().quarantine(&store));
    println!("{report}");
    match report.outcome(InvariantId::I2) {
        Outcome::Failed(m) => assert!(m.contains("quarantined outside"), "{m}"),
        other => panic!("I-2 must fail: {other:?}"),
    }
}
