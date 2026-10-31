//! FX-CLS end-to-end fixtures (docs/FIXTURES.md §3.1, P1-FX-02): each builds its tree, runs
//! the real scanner → detectors → Git/repo-policy/activity signals → rubric (see
//! `common::Pipeline`), twice (scan, then revalidation), and checks the universal invariants
//! I-1 (plan), I-2 (canary), I-3 (tree unchanged), I-6 (no network) and I-8 (tiers never
//! decrease).
//!
//! The per-layer versions of the same scenarios live with their crates; the registry
//! (`scull_fixtures::registry`) lists both.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Pipeline, check_all, rel, run_checked};
use scull_core::config::{RootConcern, validate_roots};
use scull_core::{Action, Evidence, ReasonCode, Tier};
use scull_detectors::rules::RuleSet;
use scull_fixtures::{CACHEDIR_TAG, Fixture, NodeLock, ago, days_ago};
use std::fs;
use std::path::Path;
use std::time::Duration;

fn reasons(c: &scull_core::Candidate) -> Vec<ReasonCode> {
    c.reasons.iter().map(|r| r.code).collect()
}

fn logical_bytes(path: &Path) -> u64 {
    let md = fs::symlink_metadata(path).unwrap();
    if md.is_dir() {
        fs::read_dir(path)
            .unwrap()
            .map(|e| logical_bytes(&e.unwrap().path()))
            .sum()
    } else if md.is_file() {
        md.len()
    } else {
        0
    }
}

// ---------------------------------------------------------------------------------------------

/// FX-CLS-01: `target/` in a Cargo project with `Cargo.toml` and `Cargo.lock`, idle 3 days →
/// S0 with marker and lockfile evidence.
#[test]
fn fx_cls_01_idle_cargo_target_with_lockfile_is_s0_in_plan() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 64 * 1024)
        .unwrap()
        .age("app/target", days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    assert_eq!(run.rel_paths(), ["app/target"]);
    let item = run.at("app/target");
    let c = &item.candidate;
    assert_eq!(c.rule.id, "rust.target");
    assert_eq!(c.tier, Tier::S0, "{:?}", c.reasons);
    assert_eq!(c.action, Action::Delete);
    assert!(reasons(c).contains(&ReasonCode::Regenerable));
    let root = fx.root();
    assert!(c.evidence.iter().any(
        |e| matches!(e, Evidence::Marker { path } if rel(root, &path.to_path_buf()) == "app/Cargo.toml")
    ));
    assert!(c.evidence.iter().any(
        |e| matches!(e, Evidence::Lockfile { path } if rel(root, &path.to_path_buf()) == "app/Cargo.lock")
    ));
    // Sizes come from the scan and include the 64 KiB binary.
    assert!(c.sizes.logical >= 64 * 1024, "{:?}", c.sizes);
    assert_eq!(run.plan().len(), 1);
}

/// FX-CLS-02: a folder named `target/` with photos and no `Cargo.toml` anywhere above → not a
/// candidate.
#[test]
fn fx_cls_02_target_full_of_photos_is_not_a_candidate() {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("Pictures/target/IMG_0001.jpg", 4096)
        .unwrap()
        .file_of_size("Pictures/target/IMG_0002.jpg", 4096)
        .unwrap()
        .file("Pictures/target/CACHEDIR.TAG", CACHEDIR_TAG)
        .unwrap()
        .age("Pictures", days_ago(30))
        .unwrap()
        .checkpoint()
        .unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    assert!(run.items.is_empty(), "{:?}", run.rel_paths());
    assert!(run.plan().is_empty());
    // The scanner did see (and size) the photos: they were looked at, not skipped.
    assert!(run.outcome.totals.files >= 3);
}

/// FX-CLS-03: `dist/` committed to Git in a JS project → S3, reason "git-tracked"; never in a
/// plan. `dist/` has no bundled rule, so a user rule (the way a team would add one) detects
/// it; the same rule on an ignored `dist/` proves it is the Git signal that blocks.
#[test]
fn fx_cls_03_git_tracked_dist_is_s3_and_never_planned() {
    const DIST_RULE: &str = r#"
schema_version = 1

[[rule]]
id = "team.js-dist"
version = 1
artifact = "JavaScript bundle output"
ecosystem = "node"
category = "project_artifact"
platforms = ["macos", "linux", "windows"]
base_tier = "S1"
strategy = "filesystem"
evidence = ["marker", "lockfile"]

[rule.match]
names = ["dist"]

[rule.markers]
all = ["../package.json"]

[rule.lockfiles]
any = ["../package-lock.json"]
"#;
    let mut fx = Fixture::new().unwrap();
    for project in ["committed", "ignored"] {
        let repo = fx.git_repo(project).unwrap();
        fx.npm_project(project, project, NodeLock::Npm)
            .unwrap()
            .file(format!("{project}/src/index.js"), "export default 1;\n")
            .unwrap()
            .file(format!("{project}/dist/app.js"), "console.log(1);\n")
            .unwrap()
            .file(format!("{project}/dist/app.js.map"), "{}\n")
            .unwrap();
        if project == "ignored" {
            fx.file(format!("{project}/.gitignore"), "dist/\n").unwrap();
        }
        repo.commit_all("initial").unwrap();
        fx.age(format!("{project}/dist"), days_ago(3)).unwrap();
    }
    fx.checkpoint().unwrap();

    let mut rules = RuleSet::bundled().unwrap();
    rules.add_user_file(DIST_RULE, "team-rules.toml").unwrap();
    rules.enable("team.js-dist").unwrap();
    let run = run_checked(&fx, &Pipeline::new(fx.root()).rules(rules));
    assert_eq!(run.rel_paths(), ["committed/dist", "ignored/dist"]);

    let committed = run.at("committed/dist");
    assert_eq!(
        committed.caller.git,
        scull_safety::GitTracking::Tracked { files: 2 }
    );
    let c = &committed.candidate;
    assert_eq!(c.tier, Tier::S3);
    assert_eq!(c.action, Action::None);
    assert!(
        reasons(c).contains(&ReasonCode::GitTracked),
        "{:?}",
        c.reasons
    );
    assert!(
        !run.plan().iter().any(|p| p.id == c.id),
        "a git-tracked item is never planned"
    );

    let ignored = run.at("ignored/dist");
    assert_eq!(ignored.caller.git, scull_safety::GitTracking::NotTracked);
    assert_eq!(
        ignored.candidate.tier,
        Tier::S1,
        "{:?}",
        ignored.candidate.reasons
    );
    assert!(run.plan().iter().any(|p| p.id == ignored.candidate.id));
}

/// FX-CLS-04: a shared Cargo target directory outside two projects → one candidate,
/// attributed to both, bytes counted once.
#[test]
fn fx_cls_04_shared_target_dir_is_one_candidate_counted_once() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("a", "a")
        .unwrap()
        .cargo_project("b", "b")
        .unwrap()
        .file("build-cache/CACHEDIR.TAG", CACHEDIR_TAG)
        .unwrap()
        .file(
            "build-cache/.rustc_info.json",
            r#"{"rustc_fingerprint":0,"outputs":{}}"#,
        )
        .unwrap()
        .file_of_size("build-cache/debug/a", 8192)
        .unwrap()
        .file_of_size("build-cache/debug/b", 8192)
        .unwrap()
        // a: relative target-dir; b: absolute (TOML literal string: no escaping on Windows).
        .file(
            "a/.cargo/config.toml",
            "[build]\ntarget-dir = '../build-cache'\n",
        )
        .unwrap();
    let abs = fx.root().join("build-cache");
    fx.file(
        "b/.cargo/config.toml",
        format!("[build]\ntarget-dir = '{}'\n", abs.display()),
    )
    .unwrap()
    .age("build-cache", days_ago(3))
    .unwrap()
    .checkpoint()
    .unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    assert_eq!(run.rel_paths(), ["build-cache"], "one candidate");
    let item = run.at("build-cache");
    let projects: Vec<&str> = item
        .artifact
        .projects
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(projects, ["a", "b"], "attributed to both projects");
    // Two projects must rebuild: costly, so S1 even though idle with lockfiles.
    assert_eq!(
        item.candidate.tier,
        Tier::S1,
        "{:?}",
        item.candidate.reasons
    );

    // Bytes counted once: the plan's total equals the directory, measured independently.
    let planned: u64 = run.plan().iter().map(|c| c.sizes.logical).sum();
    let independent = logical_bytes(&abs);
    assert!(independent >= 2 * 8192);
    // The scanner's logical size also counts directory nodes; it must be the directory's
    // content plus those nodes, never twice the content.
    assert!(planned >= independent, "{planned} < {independent}");
    assert!(planned < 2 * independent, "counted twice: {planned}");
    let summary = item.summary.as_ref().unwrap();
    assert_eq!(item.candidate.sizes.logical, summary.totals.logical);
}

/// FX-CLS-05: `node_modules` with `package-lock.json` / `pnpm-lock.yaml` → S0; without any
/// lockfile → S2.
#[test]
fn fx_cls_05_node_modules_with_lockfile_s0_without_lockfile_s2() {
    let mut fx = Fixture::new().unwrap();
    for (dir, lock) in [
        ("npm", NodeLock::Npm),
        ("pnpm", NodeLock::Pnpm),
        ("none", NodeLock::None),
    ] {
        fx.npm_project(dir, dir, lock)
            .unwrap()
            .node_modules(dir, &["left-pad", "chalk"])
            .unwrap()
            .age(format!("{dir}/node_modules"), days_ago(3))
            .unwrap();
    }
    fx.checkpoint().unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    assert_eq!(
        run.rel_paths(),
        ["none/node_modules", "npm/node_modules", "pnpm/node_modules"]
    );
    for with_lock in ["npm/node_modules", "pnpm/node_modules"] {
        let c = &run.at(with_lock).candidate;
        assert_eq!(c.tier, Tier::S0, "{with_lock}: {:?}", c.reasons);
        assert!(
            c.evidence
                .iter()
                .any(|e| matches!(e, Evidence::Lockfile { .. }))
        );
    }
    let c = &run.at("none/node_modules").candidate;
    assert_eq!(c.tier, Tier::S2, "{:?}", c.reasons);
    assert!(reasons(c).contains(&ReasonCode::NoLockfile));
    assert!(matches!(c.action, Action::Quarantine | Action::Trash));
}

/// FX-CLS-06: `node_modules` containing `npm link` / workspace symlinks to sibling source
/// folders → removing `node_modules` removes the links only; linked sources intact.
///
/// The removal half uses `std::fs::remove_dir_all` (which never follows links) as a stand-in
/// until the Executor exists (#67/#68); the registry tracks that under #83.
#[test]
fn fx_cls_06_linked_workspace_sources_survive_node_modules_removal() {
    let mut fx = Fixture::new().unwrap();
    fx.file(
        "web/package.json",
        "{\"name\":\"web\",\"private\":true,\"workspaces\":[\"packages/*\"]}\n",
    )
    .unwrap()
    .file("web/package-lock.json", "{}")
    .unwrap()
    .npm_project("web/packages/ui", "@acme/ui", NodeLock::None)
    .unwrap()
    .file("web/packages/ui/src/index.js", "export const ui = 1;\n")
    .unwrap()
    .npm_project("linked-lib", "linked-lib", NodeLock::None)
    .unwrap()
    .file("linked-lib/src/index.js", "module.exports = 1;\n")
    .unwrap()
    .node_modules("web", &["left-pad"])
    .unwrap();
    let ws = fx
        .symlink("web/node_modules/@acme/ui", "../../packages/ui")
        .unwrap();
    let linked = fx
        .symlink("web/node_modules/linked-lib", "../../linked-lib")
        .unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    let escape = fx.symlink("web/node_modules/escape", &canary).unwrap();
    fx.age("web/node_modules", days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();

    let pipeline = Pipeline::new(fx.root());
    let scan = pipeline.run();
    let revalidated = pipeline.run();
    check_all(&fx, &scan, &revalidated, &[]);
    assert_eq!(scan.rel_paths(), ["web/node_modules"]);
    let item = scan.at("web/node_modules");
    // The detection walk stopped at the candidate and never went through a link.
    let nm = item.artifact.path.clone();
    assert!(!scan.visited.iter().any(|v| v.starts_with(&nm) && *v != nm));
    // The scanner counted the links as links (never followed): the linked sources' and the
    // canary's files are not part of the candidate's size.
    let made = [&ws, &linked, &escape]
        .iter()
        .filter(|l| l.is_created())
        .count() as u64;
    assert_eq!(item.summary.as_ref().unwrap().totals.links, made);
    if made == 3 {
        assert_eq!(item.candidate.flags.symlink, Some(true));
    }
    let c = item.candidate.clone();
    assert_eq!(c.tier, Tier::S0, "{:?}", c.reasons);
    assert_eq!(scan.plan().len(), 1);

    // Execute the plan (stand-in): only the links go; the targets stay.
    fs::remove_dir_all(&nm).unwrap();
    assert!(fx.root().join("web/packages/ui/src/index.js").is_file());
    assert!(fx.root().join("linked-lib/src/index.js").is_file());
    let after = Pipeline::new(fx.root()).run();
    assert!(after.items.is_empty());
    check_all(&fx, &scan, &revalidated, &["web/node_modules"]);
}

/// FX-CLS-08: pnpm monorepo with 30 packages, each with `node_modules` → every candidate found
/// once; totals match an independent measurement.
#[test]
fn fx_cls_08_pnpm_monorepo_30_packages_found_once_totals_match() {
    let mut fx = Fixture::new().unwrap();
    let names: Vec<String> = (1..=30).map(|i| format!("p{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    fx.pnpm_workspace("mono", "mono", &refs)
        .unwrap()
        .file_of_size(
            "mono/node_modules/.pnpm/left-pad@1.3.0/node_modules/left-pad/index.js",
            700,
        )
        .unwrap()
        .file("mono/node_modules/.modules.yaml", "layoutVersion: 5\n")
        .unwrap();
    for (i, n) in names.iter().enumerate() {
        let pkg = format!("mono/packages/{n}");
        fx.node_modules(&pkg, &[&format!("dep-{n}")])
            .unwrap()
            .file_of_size(
                format!("{pkg}/node_modules/dep-{n}/data.bin"),
                1000 + i as u64,
            )
            .unwrap();
    }
    fx.age("mono", days_ago(3)).unwrap().checkpoint().unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    let mut expected: Vec<String> = names
        .iter()
        .map(|n| format!("mono/packages/{n}/node_modules"))
        .collect();
    expected.push("mono/node_modules".into());
    expected.sort();
    assert_eq!(run.rel_paths(), expected, "every candidate found once");
    // The store's nested node_modules belongs to the root candidate.
    assert!(
        !run.rel_paths().iter().any(|p| p.contains(".pnpm")),
        "{:?}",
        run.rel_paths()
    );
    for item in &run.items {
        assert_eq!(
            item.candidate.tier,
            Tier::S0,
            "{}",
            item.artifact.path.display()
        );
    }

    // Totals: the sum of the candidates' file bytes (scanner) equals an independent walk; no
    // byte is attributed to two candidates.
    let scanned: u64 = run
        .items
        .iter()
        .map(|i| {
            let s = i.summary.as_ref().unwrap();
            assert!(s.complete);
            s.totals.files
        })
        .sum();
    let walked_files: u64 = run
        .items
        .iter()
        .map(|i| count_files(&i.artifact.path))
        .sum();
    assert_eq!(scanned, walked_files);
    let independent: u64 = run
        .items
        .iter()
        .map(|i| logical_bytes(&i.artifact.path))
        .sum();
    let expected_bytes: u64 = 700
        + "layoutVersion: 5\n".len() as u64
        + (0..30u64).map(|i| 1000 + i).sum::<u64>()
        + names
            .iter()
            .map(|n| {
                let p = format!("dep-{n}");
                (format!("{{\"name\":\"{p}\",\"version\":\"1.0.0\",\"main\":\"index.js\"}}\n")
                    .len()
                    + format!("module.exports = '{p}';\n").len()) as u64
            })
            .sum::<u64>();
    assert_eq!(independent, expected_bytes);
}

fn count_files(path: &Path) -> u64 {
    let md = fs::symlink_metadata(path).unwrap();
    if md.is_dir() {
        fs::read_dir(path)
            .unwrap()
            .map(|e| count_files(&e.unwrap().path()))
            .sum()
    } else {
        u64::from(md.is_file())
    }
}

/// FX-CLS-09: `target/` with a file modified 1 hour ago → bumped one tier (S0 → S1), reason
/// "recent activity".
#[test]
fn fx_cls_09_target_touched_an_hour_ago_is_bumped_to_s1() {
    let mut fx = Fixture::new().unwrap();
    for p in ["idle", "busy"] {
        fx.cargo_project(p, p)
            .unwrap()
            .cargo_target(p, p, 4096)
            .unwrap()
            .age(format!("{p}/target"), days_ago(3))
            .unwrap();
    }
    fx.set_mtime("busy/target/debug/busy", ago(Duration::from_secs(3600)))
        .unwrap()
        .checkpoint()
        .unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    assert_eq!(run.rel_paths(), ["busy/target", "idle/target"]);
    assert_eq!(run.at("idle/target").candidate.tier, Tier::S0);
    let busy = run.at("busy/target");
    assert_eq!(busy.caller.recent_activity, scull_safety::Tri::Yes);
    assert_eq!(
        busy.candidate.tier,
        Tier::S1,
        "{:?}",
        busy.candidate.reasons
    );
    assert!(reasons(&busy.candidate).contains(&ReasonCode::RecentActivity));
    assert_ne!(busy.candidate.action, Action::Delete);
}

/// FX-CLS-11: a project containing `.scull-protect` → all candidates inside are S3 (and a
/// marker deeper down protects only its own subtree).
#[test]
fn fx_cls_11_scull_protect_makes_every_candidate_inside_s3() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("precious", "precious")
        .unwrap()
        .cargo_target("precious", "precious", 1024)
        .unwrap()
        .npm_project("precious/web", "web", NodeLock::Npm)
        .unwrap()
        .node_modules("precious/web", &["left-pad"])
        .unwrap()
        .file("precious/.scull-protect", "")
        .unwrap()
        // A monorepo where only one app is protected.
        .npm_project("mono/apps/kept", "kept", NodeLock::Npm)
        .unwrap()
        .node_modules("mono/apps/kept", &["chalk"])
        .unwrap()
        .file("mono/apps/kept/.scull-protect", "")
        .unwrap()
        .npm_project("mono/apps/free", "free", NodeLock::Npm)
        .unwrap()
        .node_modules("mono/apps/free", &["chalk"])
        .unwrap();
    for d in [
        "precious/target",
        "precious/web/node_modules",
        "mono/apps/kept/node_modules",
        "mono/apps/free/node_modules",
    ] {
        fx.age(d, days_ago(3)).unwrap();
    }
    fx.checkpoint().unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    assert_eq!(
        run.rel_paths(),
        [
            "mono/apps/free/node_modules",
            "mono/apps/kept/node_modules",
            "precious/target",
            "precious/web/node_modules",
        ]
    );
    for p in [
        "precious/target",
        "precious/web/node_modules",
        "mono/apps/kept/node_modules",
    ] {
        let c = &run.at(p).candidate;
        assert_eq!(c.tier, Tier::S3, "{p}");
        assert!(reasons(c).contains(&ReasonCode::ProtectMarker), "{p}");
        assert!(!run.plan().iter().any(|x| x.id == c.id), "{p} planned");
    }
    let free = &run.at("mono/apps/free/node_modules").candidate;
    assert_eq!(free.tier, Tier::S0, "{:?}", free.reasons);
    assert_eq!(run.plan().len(), 1);
}

/// FX-CLS-12: the scan root is the home folder itself → scanning it needs confirmation
/// (CFG-04); home itself, its OS folders, `~/.ssh` and `~/.gnupg` never become candidates,
/// even when something artifact-shaped sits inside them (decoys below are blocked by the
/// built-in protected roots, P1-SAFE-03); a project inside home still does. The home is a
/// stand-in: the fixture root, handed to the protected-roots set as "home".
#[test]
fn fx_cls_12_home_scan_needs_confirmation_and_secrets_are_never_candidates() {
    let mut fx = Fixture::new().unwrap();
    fx.file(".ssh/id_ed25519", "-----BEGIN OPENSSH PRIVATE KEY-----\n")
        .unwrap()
        .file(".ssh/known_hosts", "example.com ssh-ed25519 AAAA\n")
        .unwrap()
        // Decoy: an npm project inside ~/.ssh, complete with lockfile and node_modules.
        .npm_project(".ssh", "ssh-tools", NodeLock::Npm)
        .unwrap()
        .node_modules(".ssh", &["left-pad"])
        .unwrap()
        .file(".gnupg/pubring.kbx", "kbx")
        .unwrap()
        // Decoy: a Cargo project with a target/ inside ~/.gnupg.
        .cargo_project(".gnupg/tool", "tool")
        .unwrap()
        .cargo_target(".gnupg/tool", "tool", 1024)
        .unwrap()
        // Decoy: a Cargo project inside ~/Library (an OS folder on macOS).
        .cargo_project("Library/Application Support/proj", "proj")
        .unwrap()
        .cargo_target("Library/Application Support/proj", "proj", 1024)
        .unwrap()
        .file("Documents/thesis.docx", "thesis")
        .unwrap()
        .cargo_project("Projects/app", "app")
        .unwrap()
        .cargo_target("Projects/app", "app", 2048)
        .unwrap();
    for d in [
        ".ssh/node_modules",
        ".gnupg/tool/target",
        "Library/Application Support/proj/target",
        "Projects/app/target",
    ] {
        fx.age(d, days_ago(3)).unwrap();
    }
    fx.checkpoint().unwrap();
    let home = fx.root().to_path_buf();

    // CFG-04: the home folder as a scan root needs confirmation; a folder inside it doesn't.
    let concerns = validate_roots(std::slice::from_ref(&home), &home, &[]).unwrap();
    assert_eq!(concerns.len(), 1);
    assert_eq!(concerns[0].concern, RootConcern::HomeRoot);
    assert!(
        validate_roots(&[home.join("Projects")], &home, &[])
            .unwrap()
            .is_empty()
    );

    // Confirmed: scan home.
    let run = run_checked(&fx, &Pipeline::new(&home).home(&home));
    assert_eq!(
        run.rel_paths(),
        [
            ".gnupg/tool/target",
            ".ssh/node_modules",
            "Library/Application Support/proj/target",
            "Projects/app/target",
        ]
    );
    let library_is_os_folder = cfg!(target_os = "macos");
    for item in &run.items {
        let c = &item.candidate;
        let p = rel(&home, &item.artifact.path);
        let blocked = p.starts_with(".ssh/")
            || p.starts_with(".gnupg/")
            || (library_is_os_folder && p.starts_with("Library/"));
        if blocked {
            assert_eq!(c.tier, Tier::S3, "{p}: {:?}", c.reasons);
            assert!(item.caller.in_protected_root, "{p}");
            assert!(reasons(c).contains(&ReasonCode::ProtectedRoot), "{p}");
            assert!(!run.plan().iter().any(|x| x.id == c.id), "{p} planned");
        } else {
            assert_eq!(c.tier, Tier::S0, "{p}: {:?}", c.reasons);
        }
    }
    let planned: Vec<String> = run
        .plan()
        .iter()
        .map(|c| rel(&home, &c.path.to_path_buf()))
        .collect();
    assert!(planned.contains(&"Projects/app/target".to_owned()));
    assert!(
        planned
            .iter()
            .all(|p| !p.starts_with(".ssh") && !p.starts_with(".gnupg"))
    );
    assert!(
        !planned.iter().any(|p| p.is_empty()),
        "home itself is never planned"
    );
}
