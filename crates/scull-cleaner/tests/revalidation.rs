//! Revalidation before every item (P2-EXEC-03, SAFE-05): plans are made from a real engine scan
//! of a `scull-fixtures` tree, the tree changes between planning and execution, and
//! [`Executor::execute_plan`] must skip (and report) every item that no longer qualifies while
//! still executing the others. Every test checks the invariants (I-1, I-2, I-3, I-8).
//!
//! Fixture IDs (docs/FIXTURES.md): FX-RACE-01, FX-RACE-03, FX-RACE-04, FX-EXEC-10. Docker items
//! (P5-DET-03, read-only) are refused before revalidation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/planning.rs"]
mod planning;

use planning::*;
use scull_cleaner::{ItemOutcome, PlanContext, Revalidation, Skip};
use scull_core::{DropReason, ErrorKind, Plan, PlanId, PlanItem, PlanOrigin, PlanParams, Tier};
use scull_engine::{CancelToken, now_unix};
use scull_fixtures::{Fixture, InvariantInputs};
use std::sync::atomic::Ordering;

// ------------------------------------------------------------------------------------------

/// FX-EXEC-10: a plan item became S3 since planning (a `.scull-protect` marker appeared in its
/// project) → skipped with reason `now_protected`; the other item still runs.
#[test]
fn fx_exec_10_item_became_s3_since_planning_is_skipped_with_reason() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    assert_eq!(at(&fx, &scan, "app/target").tier, Tier::S0);
    assert_eq!(at(&fx, &scan, "lib/target").tier, Tier::S0);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);

    // After the review, the user protects `app`.
    std::fs::write(fx.root().join("app/.scull-protect"), "").unwrap();
    let mut fx = fx;
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    let skip = skip_of(&fx, &r, "app/target");
    assert_eq!(
        skip,
        &Skip::Dropped {
            reason: DropReason::NowProtected,
            fresh_tier: Some(Tier::S3)
        }
    );
    assert_eq!(skip.code(), "now_protected");
    assert!(fx.root().join("app/target/debug/app").is_file());
    assert_executed(&fx, &r, "lib/target");
    assert_eq!(r.error_kind(), Some(ErrorKind::PartialFailure));
    check(&fx, &plan, &r, &["lib/target"]);
}

/// FX-RACE-04: a build starts after the scan (Cargo's lock is held) → the fresh in-use probe
/// sees it at revalidation; the item is skipped ("in use"), the idle project still runs.
#[test]
fn fx_race_04_build_started_after_scan_item_skipped_in_use() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);

    // `cargo build` starts in `app` only: its lock is held from now on.
    h.probe.build_running.store(true, Ordering::SeqCst);
    std::fs::remove_file(fx.root().join("lib/target/debug/.cargo-lock")).unwrap();
    let mut fx = fx;
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    let skip = skip_of(&fx, &r, "app/target");
    match skip {
        Skip::Dropped {
            reason: DropReason::TierRaised { planned, fresh },
            fresh_tier,
        } => {
            assert_eq!(*planned, Tier::S0);
            assert!(*fresh > Tier::S0);
            assert_eq!(*fresh_tier, Some(*fresh));
        }
        Skip::Dropped {
            reason: DropReason::NowProtected,
            ..
        } => {}
        other => panic!("expected an in-use skip, got {other:?}"),
    }
    assert!(fx.root().join("app/target/debug/app").is_file());
    assert_executed(&fx, &r, "lib/target");
    check(&fx, &plan, &r, &["lib/target"]);
}

/// FX-RACE-01: the candidate is replaced by a symlink to the canary directory between plan and
/// execute → skipped (it is no longer the planned object); the canary is intact.
#[test]
fn fx_race_01_candidate_replaced_by_symlink_skipped_canary_intact() {
    let mut fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);

    // The attacker moves `app/target` away and links the canary in its place.
    std::fs::rename(fx.root().join("app/target"), fx.root().join("app/moved")).unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    if !scull_fixtures::skip::require(
        fx.symlink("app/target", &canary).unwrap().is_created(),
        "fx_race_01",
        "symlinks are not permitted here (covered by the walker tests)",
    ) {
        return;
    }
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    let skip = skip_of(&fx, &r, "app/target");
    assert!(
        matches!(
            skip,
            Skip::Dropped {
                reason: DropReason::Missing | DropReason::IdentityChanged,
                ..
            }
        ),
        "{skip:?}"
    );
    assert!(std::fs::symlink_metadata(fx.root().join("app/target")).is_ok());
    assert!(fx.root().join("app/moved/debug/app").is_file());
    assert!(canary.join("keep.txt").is_file());
    assert_executed(&fx, &r, "lib/target");
    check(&fx, &plan, &r, &["lib/target"]);
}

/// A directory of the same name but another identity (rebuilt or swapped) is not the planned
/// object: skipped.
#[test]
fn replaced_directory_with_new_identity_is_skipped() {
    let mut fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target"]);

    std::fs::rename(
        fx.root().join("app/target"),
        fx.root().join("app/old-target"),
    )
    .unwrap();
    fx.cargo_target("app", "app", 4096).unwrap();
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    assert!(matches!(
        skip_of(&fx, &r, "app/target"),
        Skip::Dropped {
            reason: DropReason::Missing | DropReason::IdentityChanged,
            ..
        }
    ));
    assert!(fx.root().join("app/target/debug/app").is_file());
    check(&fx, &plan, &r, &[]);
}

/// FX-RACE-03: a new untracked file appears in a linked worktree after the scan → the fresh
/// Git state raises its tier at revalidation; the item is skipped.
#[test]
fn fx_race_03_new_untracked_file_in_worktree_raises_tier_item_skipped() {
    if !scull_fixtures::skip::require(
        git_available(),
        "fx_race_03",
        "git is not installed (FX-RACE-03 needs a repository)",
    ) {
        return;
    }
    let mut fx = Fixture::new().unwrap();
    let repo = fx.git_repo("repo").unwrap();
    fx.file("repo/README.md", "hello\n").unwrap();
    repo.commit_all("initial").unwrap();
    repo.run(["worktree", "add", "--quiet", "-b", "feature", "../repo-wt"])
        .unwrap();
    fx.checkpoint().unwrap();

    let h = harness(true);
    let scan = h.scan(&fx);
    let wt = at(&fx, &scan, "repo-wt");
    println!("planned worktree: {} {:?}", wt.tier, wt.action);
    assert!(wt.tier.is_actionable(), "{wt:#?}");
    let plan = plan(&fx, &scan, &["repo-wt"]);

    // Someone starts working in the worktree.
    std::fs::write(fx.root().join("repo-wt/notes.txt"), "uncommitted work\n").unwrap();
    fx.checkpoint().unwrap();

    let r = h.execute(&fx, &plan);
    let skip = skip_of(&fx, &r, "repo-wt");
    let fresh = skip.fresh_tier().expect("re-derived");
    assert!(fresh > wt.tier, "{skip:?}");
    assert!(matches!(
        skip,
        Skip::Dropped {
            reason: DropReason::TierRaised { .. } | DropReason::NowProtected,
            ..
        }
    ));
    assert_eq!(
        std::fs::read(fx.root().join("repo-wt/notes.txt")).unwrap(),
        b"uncommitted work\n"
    );
    check(&fx, &plan, &r, &[]);
}

/// Nothing changed: every item passes revalidation and is executed under its planned tier.
#[test]
fn unchanged_items_pass_revalidation() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);
    let r = h.execute(&fx, &plan);
    for i in &r.items {
        assert_eq!(i.fresh_tier, Some(Tier::S0));
        assert_eq!(i.executed_tier(), Tier::S0);
    }
    assert_executed(&fx, &r, "app/target");
    assert_executed(&fx, &r, "lib/target");
    assert_eq!(r.error_kind(), None);
    check(&fx, &plan, &r, &["app/target", "lib/target"]);
}

/// An expired plan is rejected (exit 4) before anything is revalidated or touched.
#[test]
fn expired_or_foreign_plan_is_rejected_before_any_item() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target"]);
    let reval = h.revalidation(&fx);
    let late = PlanContext {
        now_unix: plan.expires_unix(),
        ..ctx()
    };
    let e = locked(&fx)
        .execute_plan(&plan, &late, &reval, None, &CancelToken::new())
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanExpired);
    let foreign = PlanContext {
        user: "someone-else",
        ..ctx()
    };
    let e = locked(&fx)
        .execute_plan(&plan, &foreign, &reval, None, &CancelToken::new())
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanForeign);
    assert_eq!(e.exit_code().as_i32(), 4);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Items outside the trusted roots are skipped; nothing is touched.
#[test]
fn item_outside_the_trusted_roots_is_skipped() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target"]);
    let reval = Revalidation::new(&h.engine, &[fx.root().join("lib")]).with_request(request());
    let r = locked(&fx)
        .execute_plan(&plan, &ctx(), &reval, None, &CancelToken::new())
        .unwrap();
    print_report(&fx, &r);
    assert_eq!(skip_of(&fx, &r, "app/target"), &Skip::OutsideRoots);
    check(&fx, &plan, &r, &[]);
}

/// A cancelled operation attempts nothing further and says so.
#[test]
fn cancelled_operation_runs_no_item() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["app/target", "lib/target"]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let r = locked(&fx)
        .execute_plan(&plan, &ctx(), &h.revalidation(&fx), None, &cancel)
        .unwrap();
    assert!(r.cancelled);
    assert!(r.items.iter().all(|i| i.outcome == ItemOutcome::NotRun));
    assert_eq!(r.error_kind(), Some(ErrorKind::Cancelled));
    check(&fx, &plan, &r, &[]);
}

/// Docker's proposed prunes (P5-DET-03: `docker builder prune`, `docker image prune`) never
/// run: a Docker item that reaches `execute_plan` is refused with the Docker reason before
/// revalidation, nothing behind it is touched, and the other item still runs.
#[test]
fn docker_items_are_refused_with_the_docker_reason() {
    let fx = two_cargo_projects();
    let h = harness(false);
    let scan = h.scan(&fx);
    let mut items: Vec<PlanItem> = ["app/target", "lib/target"]
        .iter()
        .map(|p| PlanItem::from_candidate(at(&fx, &scan, p)).unwrap())
        .collect();
    assert!(!scull_cleaner::is_quick_clean_tool("docker"));
    for kind in [
        scull_detectors::DockerKind::BuildCache,
        scull_detectors::DockerKind::DanglingImages,
    ] {
        assert!(scull_cleaner::is_docker_command(&kind.action()), "{kind:?}");
    }
    // The S0 build-cache action (S0 allows any action), on `app/target`.
    items[0].action = scull_detectors::DockerKind::BuildCache.action();
    let plan = Plan::new(
        PlanParams {
            id: PlanId::from_suffix("01JDOCKER").unwrap(),
            scan_id: scan.scan_id.clone(),
            created_unix: now_unix(),
            ttl_secs: None,
            machine_id: MACHINE.into(),
            user: USER.into(),
            origin: PlanOrigin::Cli,
            operations_watermark: None,
        },
        items,
    )
    .unwrap();
    let r = h.execute(&fx, &plan);
    let docker = r
        .items
        .iter()
        .find(|i| rel(&fx, &i.path.to_path_buf()) == "app/target")
        .unwrap();
    match &docker.outcome {
        ItemOutcome::Refused(e) => {
            assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e}");
            assert_eq!(e.message, scull_cleaner::DOCKER_UNSUPPORTED);
        }
        other => panic!("the Docker item was not refused: {other:?}"),
    }
    assert_eq!(docker.fresh_tier, None, "refused before revalidation");
    assert!(fx.root().join("app/target/debug/app").is_file());
    assert_executed(&fx, &r, "lib/target");
    check(&fx, &plan, &r, &["lib/target"]);
}
