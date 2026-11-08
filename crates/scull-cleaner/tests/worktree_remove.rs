//! Real Git removal in temporary fixture repositories only. Branches and other worktrees stay.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "common/ops.rs"]
mod ops;
#[path = "common/planning.rs"]
mod planning;

use planning::*;
use scull_cleaner::{ExecutionReport, ItemOutcome, StepHook, StepPoint};
use scull_core::{Action, OperationId, Plan, Tier};
use scull_engine::{CancelToken, now_unix};
use scull_fixtures::{Fixture, InvariantInputs};
use scull_safety::{NativeActivityProbe, Tri, check_in_use};
use scull_storage::{OperationEnd, Store};
use std::sync::Arc;

fn fixture() -> Option<Fixture> {
    if !git_available() || scull_platform::is_elevated().blocks_destructive() {
        return None;
    }
    let mut fx = Fixture::new().unwrap();
    let repo = fx.git_repo("repo").unwrap();
    fx.file("repo/README.md", "committed content\n")
        .unwrap()
        .file("repo/.gitignore", "secret.env\n")
        .unwrap()
        .file("repo/.gitattributes", "README.md filter=latefilter\n")
        .unwrap();
    repo.commit_all("initial").unwrap();
    repo.run([
        "worktree",
        "add",
        "--quiet",
        "-b",
        "feature",
        "../box/work tree",
        "main",
    ])
    .unwrap();
    repo.run([
        "worktree", "add", "--quiet", "-b", "other", "../other", "main",
    ])
    .unwrap();
    fx.checkpoint().unwrap();
    let probe = NativeActivityProbe::capture(scull_platform::activity::Limits::default());
    if check_in_use(&fx.root().join("box/work tree"), &[], &probe).in_use != Tri::No {
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        println!("SKIPPED: native activity inspection is unavailable in this environment");
        return None;
    }
    Some(fx)
}

fn execute(
    fx: &Fixture,
    h: &Harness,
    plan: &Plan,
    hook: Option<StepHook>,
) -> (ExecutionReport, Store, OperationId) {
    let mut store = ops::store(fx);
    store.insert_plan(plan).unwrap();
    let mut exec = locked(fx);
    if let Some(hook) = hook {
        exec = exec.with_step_hook(hook);
    }
    let start = ops::start(plan, "WorktreeTest");
    let id = start.operation_id.clone();
    let mut op = exec.begin_operation(&mut store, plan, &start).unwrap();
    let report = exec
        .execute_plan(
            plan,
            &ctx(),
            &h.revalidation(fx),
            Some(&mut op),
            &CancelToken::new(),
        )
        .unwrap();
    op.finish(OperationEnd::Normal, now_unix()).unwrap();
    (report, store, id)
}

#[test]
fn clean_linked_worktree_is_removed_and_audited_without_deleting_its_branch() {
    let Some(fx) = fixture() else { return };
    let h = harness(true);
    let scan = h.scan(&fx);
    let candidate = at(&fx, &scan, "box/work tree");
    assert_eq!(
        (candidate.tier, &candidate.action),
        (Tier::S1, &Action::GitWorktreeRemove),
        "{candidate:#?}"
    );
    let plan = plan(&fx, &scan, &["box/work tree"]);
    let before_refs = fx
        .git_command("repo", ["show-ref"])
        .unwrap()
        .output()
        .unwrap()
        .stdout;
    let (report, store, id) = execute(&fx, &h, &plan, None);
    assert!(
        matches!(report.items[0].outcome, ItemOutcome::WorktreeRemoved { .. }),
        "{report:#?}"
    );
    assert!(!fx.root().join("box/work tree").exists());
    assert!(fx.root().join("other/README.md").is_file());
    let after_refs = fx
        .git_command("repo", ["show-ref"])
        .unwrap()
        .output()
        .unwrap()
        .stdout;
    assert_eq!(
        before_refs, after_refs,
        "all branches and refs are preserved"
    );
    let listed = scull_git::discover(&fx.root().join("repo")).unwrap();
    assert!(
        !listed
            .worktrees
            .iter()
            .any(|w| w.path.to_path_buf() == fx.root().join("box/work tree"))
    );
    assert!(listed.problems.is_empty());
    let steps = store.journal_steps(&id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].intent.source_path, plan.items()[0].path);
    assert!(steps[0].close.is_some());
    let runner = scull_git::GitRunner::new().unwrap();
    fx.check_invariants(
        &InvariantInputs::new()
            .plan(
                plan.items()
                    .iter()
                    .map(|i| (i.item_id.clone(), i.tier, i.action.clone())),
            )
            .allow("box/work tree")
            .allow("repo/.git/worktrees")
            .audit(&store, &id)
            .operation_bytes(&store, &id)
            .spawned([runner.resolved_binary()])
            .trusted_dir(runner.resolved_binary().parent().unwrap()),
    )
    .assert_ok();
}

#[test]
fn changes_after_revalidation_are_refused_before_git_can_remove_anything() {
    let mut changes = vec![
        "dirty",
        "untracked",
        "ignored",
        "locked",
        "index_lock",
        "registration",
        "identity",
        "admin_identity",
    ];
    #[cfg(unix)]
    changes.push("symlink_parent");
    for late in [false, true] {
        for change in changes.clone() {
            let Some(fx) = fixture() else { return };
            let h = harness(true);
            let scan = h.scan(&fx);
            let plan = plan_named(&fx, &scan, &["box/work tree"], &change.replace('_', ""));
            let root = fx.root().to_path_buf();
            let hook = Arc::new(move |point: &StepPoint<'_>| {
                if (!late && matches!(point, StepPoint::AfterIntent(_)))
                    || (late && matches!(point, StepPoint::BeforeWorktreeRemove(_)))
                {
                    let target = root.join("box/work tree");
                    let admin = scull_git::discover(&target)
                        .unwrap()
                        .worktrees
                        .into_iter()
                        .find(|w| w.path.to_path_buf() == target)
                        .unwrap()
                        .git_dir
                        .to_path_buf();
                    match change {
                        "dirty" => {
                            std::fs::write(target.join("README.md"), "local changes\n").unwrap()
                        }
                        "untracked" => {
                            std::fs::write(target.join("notes.txt"), "unique work\n").unwrap()
                        }
                        "ignored" => {
                            std::fs::write(target.join("secret.env"), "fake secret\n").unwrap()
                        }
                        "locked" => std::fs::write(admin.join("locked"), "active work\n").unwrap(),
                        "index_lock" => std::fs::write(admin.join("index.lock"), "").unwrap(),
                        "registration" => std::fs::write(
                            admin.join("gitdir"),
                            root.join("other/.git").to_string_lossy().as_bytes(),
                        )
                        .unwrap(),
                        "admin_identity" => {
                            std::fs::rename(&admin, admin.with_extension("saved")).unwrap();
                            std::fs::create_dir(&admin).unwrap();
                        }
                        #[cfg(unix)]
                        "symlink_parent" => {
                            std::fs::rename(root.join("box"), root.join("saved-box")).unwrap();
                            std::os::unix::fs::symlink(root.join("saved-box"), root.join("box"))
                                .unwrap();
                        }
                        "identity" => {
                            std::fs::rename(&target, root.join("saved")).unwrap();
                            std::fs::create_dir(&target).unwrap();
                        }
                        _ => unreachable!(),
                    }
                }
            });
            let (report, store, id) = execute(&fx, &h, &plan, Some(hook));
            assert!(!report.items[0].succeeded(), "{change}: {report:#?}");
            assert!(fx.root().join("box/work tree").is_dir(), "{change}");
            assert!(fx.root().join("other/README.md").is_file());
            let audit = store.disk_expectations(&id).unwrap();
            assert!(audit.without_outcome.is_empty());
            assert_eq!(audit.items.len(), 1);
            assert_eq!(audit.items[0].outcome, scull_storage::ItemOutcome::Refused);
            assert!(store.verify_chain(&id).unwrap().is_intact());
            match change {
                "dirty" => assert_eq!(
                    std::fs::read_to_string(fx.root().join("box/work tree/README.md")).unwrap(),
                    "local changes\n"
                ),
                "untracked" => assert_eq!(
                    std::fs::read_to_string(fx.root().join("box/work tree/notes.txt")).unwrap(),
                    "unique work\n"
                ),
                "ignored" => assert_eq!(
                    std::fs::read_to_string(fx.root().join("box/work tree/secret.env")).unwrap(),
                    "fake secret\n"
                ),
                "identity" => assert!(fx.root().join("saved/README.md").is_file()),
                "symlink_parent" => {
                    assert!(fx.root().join("saved-box/work tree/README.md").is_file())
                }
                _ => {}
            }
            // These changes were deliberately introduced by the race hook, never by the Executor.
            fx.check_invariants(
                &InvariantInputs::new()
                    .allow("box/work tree")
                    .allow("saved")
                    .allow("saved-box")
                    .allow("box")
                    .allow("repo/.git/worktrees"),
            )
            .assert_ok();
        }
    }
}

#[test]
fn an_item_outside_execution_roots_is_never_removed() {
    let Some(fx) = fixture() else { return };
    let h = harness(true);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["box/work tree"]);
    let mut store = ops::store(&fx);
    store.insert_plan(&plan).unwrap();
    let exec = locked(&fx);
    let start = ops::start(&plan, "OutsideRoots");
    let id = start.operation_id.clone();
    let mut op = exec.begin_operation(&mut store, &plan, &start).unwrap();
    let report = exec
        .execute_plan(
            &plan,
            &ctx(),
            &scull_cleaner::Revalidation::new(&h.engine, &[fx.root().join("repo")]),
            Some(&mut op),
            &CancelToken::new(),
        )
        .unwrap();
    op.finish(OperationEnd::Normal, now_unix()).unwrap();
    assert!(!report.items[0].succeeded());
    assert!(fx.root().join("box/work tree/README.md").is_file());
    fx.check_invariants(&InvariantInputs::new().audit(&store, &id))
        .assert_ok();
}

#[cfg(unix)]
#[test]
fn a_process_using_the_worktree_blocks_removal() {
    let Some(fx) = fixture() else { return };
    let h = harness(true);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["box/work tree"]);
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .current_dir(fx.root().join("box/work tree"))
        .spawn()
        .unwrap();
    let (report, store, id) = execute(&fx, &h, &plan, None);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(!report.items[0].succeeded(), "{report:#?}");
    assert!(fx.root().join("box/work tree/README.md").is_file());
    fx.check_invariants(&InvariantInputs::new().audit(&store, &id))
        .assert_ok();
}

#[test]
fn hidden_index_flags_never_allow_local_contents_to_be_removed() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let Some(mut fx) = fixture() else { return };
        let h = harness(true);
        let scan = h.scan(&fx);
        let plan = plan(&fx, &scan, &["box/work tree"]);
        assert!(
            fx.git_command("box/work tree", ["update-index", flag, "README.md"])
                .unwrap()
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            fx.root().join("box/work tree/README.md"),
            "valuable local contents\n",
        )
        .unwrap();
        fx.checkpoint().unwrap();
        let (report, store, id) = execute(&fx, &h, &plan, None);
        assert!(!report.items[0].succeeded(), "{flag}: {report:#?}");
        assert_eq!(
            std::fs::read_to_string(fx.root().join("box/work tree/README.md")).unwrap(),
            "valuable local contents\n"
        );
        fx.check_invariants(&InvariantInputs::new().audit(&store, &id))
            .assert_ok();
    }
}

#[test]
fn late_pointer_swap_to_another_valid_repository_preserves_the_folder() {
    for ignored in [false, true] {
        let Some(mut fx) = fixture() else { return };
        let replacement = fx.git_repo("replacement-repo").unwrap();
        fx.file("replacement-repo/README.md", "committed content\n")
            .unwrap()
            .file("replacement-repo/.gitignore", "secret.env\n")
            .unwrap()
            .file(
                "replacement-repo/.gitattributes",
                "README.md filter=latefilter\n",
            )
            .unwrap();
        replacement.commit_all("initial").unwrap();
        replacement
            .run([
                "worktree",
                "add",
                "--quiet",
                "-b",
                "replacement",
                "../replacement-tree",
                "main",
            ])
            .unwrap();
        let replacement_tree = fx.root().join("replacement-tree");
        let replacement_admin = scull_git::discover(&replacement_tree)
            .unwrap()
            .worktrees
            .into_iter()
            .find(|w| w.path.to_path_buf() == replacement_tree)
            .unwrap()
            .git_dir
            .to_path_buf();
        fx.checkpoint().unwrap();
        let h = harness(true);
        let scan = h.scan(&fx);
        let plan = plan(&fx, &scan, &["box/work tree"]);
        let target = fx.root().join("box/work tree");
        let marker = std::fs::read(replacement_tree.join(".git")).unwrap();
        let root = fx.root().to_path_buf();
        let hook = Arc::new(move |point: &StepPoint<'_>| {
            if matches!(point, StepPoint::BeforeWorktreeRemove(_)) {
                std::fs::write(target.join(".git"), &marker).unwrap();
                std::fs::write(
                    replacement_admin.join("gitdir"),
                    target.join(".git").to_string_lossy().as_bytes(),
                )
                .unwrap();
                if ignored {
                    std::fs::write(target.join("secret.env"), "valuable ignored contents\n")
                        .unwrap();
                }
                let valid = scull_git::discover(&target).unwrap();
                assert_eq!(
                    valid.common_dir.to_path_buf(),
                    root.join("replacement-repo/.git")
                );
            }
        });
        let (report, store, id) = execute(&fx, &h, &plan, Some(hook));
        assert!(!report.items[0].succeeded(), "{report:#?}");
        assert!(fx.root().join("box/work tree/README.md").is_file());
        if ignored {
            assert_eq!(
                std::fs::read_to_string(fx.root().join("box/work tree/secret.env")).unwrap(),
                "valuable ignored contents\n"
            );
        }
        assert_eq!(
            store.disk_expectations(&id).unwrap().items[0].outcome,
            scull_storage::ItemOutcome::Refused
        );
        assert!(store.verify_chain(&id).unwrap().is_intact());
        fx.check_invariants(
            &InvariantInputs::new()
                .allow("box/work tree")
                .allow("replacement-repo/.git/worktrees"),
        )
        .assert_ok();
    }
}

#[test]
fn a_late_filter_configuration_cannot_escape_the_prepared_hardening() {
    let Some(fx) = fixture() else { return };
    let h = harness(true);
    let scan = h.scan(&fx);
    let plan = plan(&fx, &scan, &["box/work tree"]);
    let sentinel = fx.base().join("filter-ran");
    let filter = format!("touch '{}'; cat", sentinel.display());
    let config = std::sync::Mutex::new(
        fx.git_command(
            "box/work tree",
            ["config", "filter.latefilter.clean", filter.as_str()],
        )
        .unwrap(),
    );
    let target = fx.root().join("box/work tree");
    let hook = Arc::new(move |point: &StepPoint<'_>| {
        if matches!(point, StepPoint::BeforeWorktreeRemove(_)) {
            assert!(config.lock().unwrap().status().unwrap().success());
            std::fs::write(target.join("README.md"), "committed content\n").unwrap();
        }
    });
    let (report, store, id) = execute(&fx, &h, &plan, Some(hook));
    assert!(!report.items[0].succeeded(), "{report:#?}");
    assert!(
        !sentinel.exists(),
        "the newly configured filter must never execute"
    );
    assert_eq!(
        std::fs::read_to_string(fx.root().join("box/work tree/README.md")).unwrap(),
        "committed content\n"
    );
    assert_eq!(
        store.disk_expectations(&id).unwrap().items[0].outcome,
        scull_storage::ItemOutcome::Refused
    );
    fx.check_invariants(
        &InvariantInputs::new()
            .allow("box/work tree")
            .allow("repo/.git/config"),
    )
    .assert_ok();
}
