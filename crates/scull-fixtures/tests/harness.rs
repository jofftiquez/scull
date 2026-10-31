//! Self-tests for the fixture harness (P1-FX-01): builders, canaries, snapshots and the
//! invariants I-1…I-8. Every test checks the invariants of every fixture it builds.

use scull_core::{
    Action, Candidate, Category, FileIdentity, Flags, FsPath, ItemId, ReasonCode, Regeneration,
    RuleRef, Sizes, Tier, TierReason,
};
use scull_fixtures::{
    EntryKind, Fixture, FixtureError, InvariantId, InvariantInputs, NodeLock, Outcome,
    TreeSnapshot, days_ago, invariants,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A small, realistic tree used by several tests.
fn sample() -> Fixture {
    fn build(fx: &mut Fixture) -> scull_fixtures::Result<()> {
        fx.cargo_project("api", "api")?
            .cargo_target("api", "api", 10_000)?
            .npm_project("web", "web", NodeLock::Npm)?
            .node_modules("web", &["left-pad", "is-odd"])?
            .file("notes/readme.txt", "hello")?
            .file_with_mtime("notes/old.txt", "old", days_ago(3))?
            .dir("empty/dir")?;
        Ok(())
    }
    let mut fx = Fixture::new().expect("fixture");
    build(&mut fx).expect("build sample fixture");
    fx
}

fn id(n: u8) -> ItemId {
    ItemId::derive(FileIdentity::new(1, u128::from(n)), &[n])
}

fn assert_only_failure(report: &scull_fixtures::InvariantReport, which: InvariantId) {
    let failed: Vec<InvariantId> = report.failures().iter().map(|(i, _)| *i).collect();
    assert_eq!(failed, vec![which], "report:\n{report}");
}

#[test]
fn canary_is_beside_root_and_populated() {
    let mut fx = Fixture::new().unwrap();
    fx.checkpoint().unwrap();
    let c = fx.canary();
    assert!(!c.dir.starts_with(fx.root()), "canary must be outside root");
    assert!(!fx.root().starts_with(&c.dir));
    assert_eq!(c.dir.parent(), fx.root().parent(), "canary is a sibling");
    for f in [&c.file, &c.nested_file, &c.symlink_target_file] {
        assert!(f.is_file(), "{}", f.display());
    }
    assert!(c.symlink_target_dir.join("keep.txt").is_file());
    assert!(c.junction_target.join("keep.txt").is_file());
    let base = fx.canary_baseline();
    assert!(base.get("nested/deeper/data.bin").is_some());
    assert!(base.get("").is_some(), "canary root itself is recorded");
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn untouched_fixture_passes_all_implemented_invariants() {
    let mut fx = sample();
    fx.checkpoint().unwrap();
    let report = fx.check_invariants(&InvariantInputs::new());
    println!("{report}");
    report.assert_ok();
    // I-6 runs in-process here: the best-effort socket detector, or a verified block if the
    // whole run was started inside a sandbox (`scull_fixtures::net::SANDBOX_ENV`).
    assert_eq!(
        report.ran(),
        vec![InvariantId::I2, InvariantId::I3, InvariantId::I6]
    );
    let expected = if scull_fixtures::net::active_sandbox().is_some() {
        "blocked:"
    } else {
        "best-effort:"
    };
    assert!(
        matches!(report.outcome(InvariantId::I6), Outcome::PassedWith(n) if n.starts_with(expected)),
        "{report}"
    );
    assert!(matches!(
        report.outcome(InvariantId::I1),
        Outcome::NotApplicable(_)
    ));
    assert!(matches!(
        report.outcome(InvariantId::I8),
        Outcome::NotApplicable(_)
    ));
    for i in [InvariantId::I4, InvariantId::I5, InvariantId::I7] {
        assert!(
            matches!(report.outcome(i), Outcome::Unimplemented { .. }),
            "{i} must not claim to pass"
        );
    }
}

#[test]
fn all_eight_invariants_reported_and_five_run_with_plan_and_tiers() {
    let mut fx = sample();
    fx.checkpoint().unwrap();
    let inputs = InvariantInputs::new()
        .plan([
            (id(1), Tier::S0, Action::Delete),
            (id(2), Tier::S1, Action::Quarantine),
        ])
        .tiers(
            BTreeMap::from([(id(1), Tier::S0), (id(2), Tier::S1)]),
            BTreeMap::from([(id(1), Tier::S0), (id(2), Tier::S2)]),
        );
    let report = fx.check_invariants(&inputs);
    println!("{report}");
    report.assert_ok();
    assert_eq!(report.results().len(), 8);
    assert_eq!(
        report.ran(),
        vec![
            InvariantId::I1,
            InvariantId::I2,
            InvariantId::I3,
            InvariantId::I6,
            InvariantId::I8
        ]
    );
}

#[test]
fn i3_detects_content_change_inside_root() {
    let mut fx = sample();
    fx.checkpoint().unwrap();
    fs::write(fx.path("notes/readme.txt").unwrap(), "HELLO").unwrap();
    let report = fx.check_invariants(&InvariantInputs::new());
    assert_only_failure(&report, InvariantId::I3);
    assert!(report.failures()[0].1.contains("readme.txt"));
}

#[test]
fn i3_detects_added_removed_and_mtime_only_changes() {
    type Mutation = fn(&Fixture);
    let cases: [(&str, Mutation); 4] = [
        ("added", |fx| {
            fs::write(fx.path("notes/new.txt").unwrap(), "x").unwrap();
        }),
        ("removed", |fx| {
            fs::remove_file(fx.path("notes/old.txt").unwrap()).unwrap();
        }),
        ("removed dir", |fx| {
            fs::remove_dir(fx.path("empty/dir").unwrap()).unwrap();
        }),
        ("mtime", |fx| {
            let f = fs::File::options()
                .write(true)
                .open(fx.path("notes/readme.txt").unwrap())
                .unwrap();
            f.set_modified(days_ago(30)).unwrap();
        }),
    ];
    for (name, mutate) in cases {
        let mut fx = sample();
        fx.checkpoint().unwrap();
        mutate(&fx);
        let report = fx.check_invariants(&InvariantInputs::new());
        assert_eq!(
            report
                .failures()
                .iter()
                .map(|(i, _)| *i)
                .collect::<Vec<_>>(),
            vec![InvariantId::I3],
            "{name}: {report}"
        );
    }
}

#[test]
fn i3_detects_permission_change() {
    let mut fx = sample();
    fx.checkpoint().unwrap();
    let p = fx.path("notes/readme.txt").unwrap();
    let original = fs::metadata(&p).unwrap().permissions();
    let mut perms = original.clone();
    perms.set_readonly(true);
    fs::set_permissions(&p, perms).unwrap();
    let report = fx.check_invariants(&InvariantInputs::new());
    fs::set_permissions(&p, original).unwrap();
    assert_only_failure(&report, InvariantId::I3);
}

#[test]
fn i3_allows_changes_at_or_below_allowed_paths_only() {
    let mut fx = sample();
    fx.checkpoint().unwrap();
    fs::remove_dir_all(fx.path("api/target").unwrap()).unwrap();
    let ok = fx.check_invariants(&InvariantInputs::new().allow("api/target"));
    ok.assert_ok();
    // A sibling with a shared name prefix is not covered by the allowance.
    fs::write(fx.path("api/targets").unwrap(), "x").unwrap();
    let bad = fx.check_invariants(&InvariantInputs::new().allow("api/target"));
    assert_only_failure(&bad, InvariantId::I3);
}

#[test]
fn i3_fails_without_a_checkpoint() {
    let fx = sample();
    let report = fx.check_invariants(&InvariantInputs::new());
    assert_only_failure(&report, InvariantId::I3);
    assert!(report.failures()[0].1.contains("checkpoint"));
}

#[test]
fn i2_detects_canary_mutations() {
    type Mutation = fn(&Fixture);
    let cases: [(&str, Mutation); 4] = [
        ("content", |fx| {
            fs::write(&fx.canary().file, "changed").unwrap();
        }),
        ("removed", |fx| {
            fs::remove_file(&fx.canary().nested_file).unwrap();
        }),
        ("added", |fx| {
            fs::write(fx.canary().symlink_target_dir.join("new.txt"), "x").unwrap();
        }),
        ("mtime", |fx| {
            let f = fs::File::options()
                .write(true)
                .open(&fx.canary().symlink_target_file)
                .unwrap();
            f.set_modified(days_ago(10)).unwrap();
        }),
    ];
    for (name, mutate) in cases {
        let mut fx = sample();
        fx.checkpoint().unwrap();
        mutate(&fx);
        let report = fx.check_invariants(&InvariantInputs::new());
        assert_eq!(
            report
                .failures()
                .iter()
                .map(|(i, _)| *i)
                .collect::<Vec<_>>(),
            vec![InvariantId::I2],
            "{name}: {report}"
        );
    }
}

#[test]
fn snapshot_does_not_follow_symlinks() {
    let mut fx = sample();
    let target = fx.canary().symlink_target_dir.clone();
    let outcome = fx.symlink("api/target/escape", &target).unwrap();
    let file_link = fx
        .symlink("notes/link.txt", fx.canary().symlink_target_file.clone())
        .unwrap();
    if outcome.is_created() {
        let snap = fx.snapshot_root().unwrap();
        let e = snap.get("api/target/escape").expect("link recorded");
        // Windows `read_link` reports absolute targets in verbatim (`\\?\`) form.
        let strip = |p: &Path| p.to_string_lossy().trim_start_matches(r"\\?\").to_owned();
        match &e.kind {
            EntryKind::Symlink { target: t } => assert_eq!(strip(t), strip(&target)),
            other => panic!("expected a symlink entry, got {other:?}"),
        }
        assert_eq!(e.sha256, None, "link target content is not read");
        assert!(
            snap.entries()
                .keys()
                .all(|k| k == Path::new("api/target/escape")
                    || !k.starts_with("api/target/escape")),
            "snapshot descended into the symlink"
        );
        assert!(file_link.is_created());
        assert!(matches!(
            snap.get("notes/link.txt").unwrap().kind,
            EntryKind::Symlink { .. }
        ));
    } else {
        println!("symlinks skipped here: {:?}", fx.skipped());
        assert!(!fx.skipped().is_empty(), "a skip must be recorded");
    }
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn builder_refuses_to_write_through_symlinks() {
    let mut fx = Fixture::new().unwrap();
    let target = fx.canary().symlink_target_dir.clone();
    if fx.symlink("esc", &target).unwrap().is_created() {
        for r in [
            fx.file("esc/evil.txt", "x").map(|_| ()),
            fx.dir("esc/sub").map(|_| ()),
            fx.file("esc", "x").map(|_| ()),
            fx.set_mtime("esc/keep.txt", days_ago(1)).map(|_| ()),
        ] {
            assert!(matches!(r, Err(FixtureError::UnsafePath { .. })), "{r:?}");
        }
    }
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn builder_rejects_paths_outside_the_root() {
    let mut fx = Fixture::new().unwrap();
    let outside = std::env::temp_dir().join("scull-fx-must-not-exist.txt");
    let canary_file = fx.canary().file.clone();
    let rel_escape = Path::new("..").join("canary").join("canary.txt");
    let attempts = [
        fx.file(&outside, "x").map(|_| ()),
        fx.file(&rel_escape, "x").map(|_| ()),
        fx.file("a/../../escape.txt", "x").map(|_| ()),
        fx.file("", "x").map(|_| ()),
        fx.dir(&canary_file).map(|_| ()),
        fx.hardlink("h", &outside).map(|_| ()),
        fx.hardlink("h", &rel_escape).map(|_| ()),
    ];
    for r in attempts {
        assert!(matches!(r, Err(FixtureError::UnsafePath { .. })), "{r:?}");
    }
    assert!(!outside.exists());
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn hardlinked_files_share_identity_on_unix() {
    let mut fx = Fixture::new().unwrap();
    fx.file("a/original.bin", "same bytes")
        .unwrap()
        .hardlink("b/link.bin", "a/original.bin")
        .unwrap()
        .file("c/other.bin", "same bytes")
        .unwrap();
    let snap = fx.snapshot_root().unwrap();
    let (a, b, c) = (
        snap.get("a/original.bin").unwrap(),
        snap.get("b/link.bin").unwrap(),
        snap.get("c/other.bin").unwrap(),
    );
    assert_eq!(a.sha256, b.sha256);
    assert_eq!(a.sha256, c.sha256);
    if cfg!(unix) {
        assert!(a.identity.is_some());
        assert_eq!(a.identity, b.identity, "hardlinks are one object");
        assert_ne!(a.identity, c.identity, "a copy is a different object");
    } else {
        // Stable std has no file ID on Windows; tracked by #32 (P1-PLAT-02).
        assert_eq!(a.identity, None);
    }
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn hardlink_to_canary_keeps_i2_green_until_canary_changes() {
    let mut fx = Fixture::new().unwrap();
    let canary_file = fx.canary().symlink_target_file.clone();
    fx.hardlink("cand/shared.txt", &canary_file).unwrap();
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
    // Writing through the in-root hardlink changes the canary: I-2 must catch it.
    fs::write(fx.path("cand/shared.txt").unwrap(), "clobbered").unwrap();
    let report = fx.check_invariants(&InvariantInputs::new().allow("cand"));
    assert_only_failure(&report, InvariantId::I2);
}

#[test]
fn junction_is_recorded_not_followed() {
    let mut fx = Fixture::new().unwrap();
    let target = fx.canary().junction_target.clone();
    let outcome = fx.junction("j", &target).unwrap();
    if cfg!(windows) {
        assert!(outcome.is_created(), "{outcome:?}");
        let snap = fx.snapshot_root().unwrap();
        assert!(snap.get("j").is_some());
        assert!(
            snap.get(Path::new("j").join("keep.txt")).is_none(),
            "snapshot descended into the junction"
        );
    } else {
        assert!(!outcome.is_created());
        assert_eq!(fx.skipped().len(), 1, "skip is reported");
    }
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn builders_are_deterministic() {
    let hash = |size: u64| {
        let mut fx = Fixture::new().unwrap();
        fx.file_of_size("data/blob.bin", size).unwrap();
        let snap = fx.snapshot_root().unwrap();
        let e = snap.get("data/blob.bin").unwrap().clone();
        fx.checkpoint().unwrap();
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        e
    };
    let (a, b) = (hash(200_000), hash(200_000));
    assert_eq!(a.size, 200_000);
    assert_eq!(a.sha256, b.sha256);
    assert_ne!(hash(10).sha256, hash(11).sha256);
}

#[test]
fn file_with_mtime_sets_mtime() {
    let mut fx = Fixture::new().unwrap();
    let want = days_ago(3);
    fx.file_with_mtime("t/old.o", "obj", want).unwrap();
    let got = fs::metadata(fx.path("t/old.o").unwrap())
        .unwrap()
        .modified()
        .unwrap();
    let delta = got.duration_since(want).unwrap_or_else(|e| e.duration());
    assert!(delta < Duration::from_secs(2), "mtime off by {delta:?}");
    assert!(got < SystemTime::now() - Duration::from_secs(2 * 24 * 3600));
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn project_helpers_write_markers_and_lockfiles() {
    let mut fx = sample();
    fx.npm_project("nolock", "nolock", NodeLock::None)
        .unwrap()
        .pnpm_workspace("mono", "mono", &["a", "b"])
        .unwrap();
    for f in [
        "api/Cargo.toml",
        "api/Cargo.lock",
        "api/src/main.rs",
        "api/target/CACHEDIR.TAG",
        "api/target/debug/api",
        "web/package.json",
        "web/package-lock.json",
        "web/node_modules/left-pad/package.json",
        "nolock/package.json",
        "mono/pnpm-lock.yaml",
        "mono/pnpm-workspace.yaml",
        "mono/packages/a/package.json",
        "mono/packages/b/package.json",
    ] {
        assert!(fx.path(f).unwrap().is_file(), "{f}");
    }
    assert!(!fx.path("nolock/package-lock.json").unwrap().exists());
    assert!(!fx.path("nolock/pnpm-lock.yaml").unwrap().exists());
    assert_eq!(
        fs::metadata(fx.path("api/target/debug/api").unwrap())
            .unwrap()
            .len(),
        10_000
    );
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
fn git_repo_is_isolated_and_deterministic() {
    let mut fx = Fixture::new().unwrap();
    let repo = fx.git_repo("repo").unwrap();
    fx.file("repo/src/lib.rs", "pub fn f() {}\n")
        .unwrap()
        .file("repo/.gitignore", "target/\n")
        .unwrap();
    let head = repo.commit_all("initial").unwrap();
    assert_eq!(head.len(), 40);
    assert_eq!(repo.run(["branch", "--show-current"]).unwrap(), "main");
    assert_eq!(repo.run(["status", "--porcelain"]).unwrap(), "");
    let origin = repo
        .run(["config", "--show-origin", "--get", "user.email"])
        .unwrap();
    assert!(origin.starts_with("command line:"), "{origin}");
    assert!(origin.ends_with(scull_fixtures::GIT_USER_EMAIL));
    // Same inputs, same commit ID (fixed identity and dates).
    let mut fx2 = Fixture::new().unwrap();
    let repo2 = fx2.git_repo("repo").unwrap();
    fx2.file("repo/src/lib.rs", "pub fn f() {}\n")
        .unwrap()
        .file("repo/.gitignore", "target/\n")
        .unwrap();
    assert_eq!(repo2.commit_all("initial").unwrap(), head);
    // Git's state stays inside the fixture's temp directory.
    assert!(repo.path().starts_with(fx.root()));
    for f in [&mut fx, &mut fx2] {
        f.checkpoint().unwrap();
        f.check_invariants(&InvariantInputs::new()).assert_ok();
    }
}

#[test]
fn i1_rejects_s3_in_a_plan() {
    assert_eq!(
        invariants::no_s3_in_plan([(Tier::S0, Action::Delete), (Tier::S2, Action::Quarantine)]),
        Outcome::Passed
    );
    for action in [Action::None, Action::Delete, Action::Quarantine] {
        assert!(invariants::no_s3_in_plan([(Tier::S3, action)]).is_failure());
    }
    let mut fx = sample();
    fx.checkpoint().unwrap();
    let report =
        fx.check_invariants(&InvariantInputs::new().plan([(id(9), Tier::S3, Action::Trash)]));
    assert_only_failure(&report, InvariantId::I1);
    assert!(report.failures()[0].1.contains(id(9).as_str()));
}

#[test]
fn i1_accepts_candidates() {
    let reasons = vec![TierReason::new(
        Tier::S0,
        ReasonCode::Regenerable,
        "lockfile",
    )];
    let identity = FileIdentity::new(1, 2);
    let mut c = Candidate {
        id: ItemId::derive(identity, b"/x/target"),
        path: FsPath::from("/x/target"),
        identity,
        category: Category::ProjectArtifact,
        artifact: "Cargo build output".into(),
        rule: RuleRef {
            id: "rust.target".into(),
            version: 1,
        },
        project: None,
        tier: Candidate::tier_from_reasons(&reasons),
        reasons,
        evidence: vec![],
        sizes: Sizes::default(),
        flags: Flags::default(),
        regeneration: Regeneration::default(),
        action: Action::Delete,
    };
    assert_eq!(invariants::no_s3_in_plan([&c]), Outcome::Passed);
    c.raise(TierReason::new(Tier::S3, ReasonCode::GitTracked, "tracked"));
    assert!(invariants::no_s3_in_plan([&c]).is_failure());
}

#[test]
fn i8_rejects_tier_decrease() {
    let scan = BTreeMap::from([(id(1), Tier::S1), (id(2), Tier::S2), (id(3), Tier::S0)]);
    let raised = BTreeMap::from([(id(1), Tier::S3), (id(2), Tier::S2)]);
    assert_eq!(
        invariants::tiers_never_decrease(&scan, &raised),
        Outcome::Passed
    );
    let lowered = BTreeMap::from([(id(1), Tier::S1), (id(2), Tier::S1)]);
    let o = invariants::tiers_never_decrease(&scan, &lowered);
    match o {
        Outcome::Failed(m) => assert!(m.contains("S2 -> S1"), "{m}"),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[test]
fn snapshot_diff_reports_each_kind_of_change() {
    let mut fx = sample();
    let before = fx.snapshot_root().unwrap();
    fs::write(fx.path("notes/readme.txt").unwrap(), "changed!").unwrap();
    fs::write(fx.path("notes/added.txt").unwrap(), "a").unwrap();
    fs::remove_file(fx.path("notes/old.txt").unwrap()).unwrap();
    let after = TreeSnapshot::capture(fx.root()).unwrap();
    let d = scull_fixtures::diff(&before, &after);
    assert_eq!(d.added, vec![PathBuf::from("notes/added.txt")]);
    assert_eq!(d.removed, vec![PathBuf::from("notes/old.txt")]);
    assert_eq!(d.changed.len(), 1);
    assert_eq!(d.changed[0].path, PathBuf::from("notes/readme.txt"));
    use scull_fixtures::ChangedField::{Content, Size};
    assert!(d.changed[0].fields.contains(&Content));
    assert!(d.changed[0].fields.contains(&Size));
    assert!(before.diff(&before).is_empty());
    fx.checkpoint().unwrap();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

#[test]
#[should_panic(expected = "dropped without running invariants")]
fn dropping_an_unchecked_fixture_panics() {
    let _fx = Fixture::new().unwrap();
}
