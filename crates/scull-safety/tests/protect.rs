//! Identity-based protected roots, markers and exclusions (P1-SAFE-03; SAFE-03, SAFE-13,
//! CFG-03, CFG-04): FX-CLS-11, FX-CLS-12, FX-FS-11, FX-WIN-05, FX-FS-09 (normalization
//! aliases), plus links, `..` segments, carve-outs and unresolvable paths.
//!
//! Every check is read-only. Real OS folders (`/usr/bin`, `C:\Windows\System32`, ...) are
//! only looked up, never touched.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: panics are failures

use scull_core::config::{RepoPolicy, RootConcern, ScanConfig, validate_roots};
use scull_core::{Evidence, ReasonCode, Tier};
use scull_fixtures::{Fixture, InvariantInputs, LinkOutcome};
use scull_platform::Native;
use scull_safety::{
    DefaultProtection, GitTracking, ProtectedPath, ProtectedSet, Protection, Regen, Scope, Signals,
    Source, TargetOs, Tri, classify, default_protected,
};
use std::path::{Path, PathBuf};

fn signals(p: &Protection) -> Signals {
    Signals {
        git: GitTracking::NotTracked,
        in_protected_root: p.in_protected_root,
        user_excluded: p.user_excluded,
        protect_marker: p.protect_marker,
        in_use: Tri::No,
        crosses_mount: false,
        link_escape: false,
        cloud_sync: false,
        marker_present: true,
        lockfile_present: true,
        artifact_known: true,
        regen: Regen::Local,
        recent_activity: Tri::No,
    }
}

fn check(set: &ProtectedSet, p: &Path) -> Protection {
    set.check(p, &Native)
}

/// Whether the filesystem holding `dir` resolves `alias` (a sibling spelling of `name`) to the
/// same entry.
fn resolves_alias(dir: &Path, alias: &str) -> bool {
    std::fs::symlink_metadata(dir.join(alias)).is_ok()
}

fn env(k: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(k)
}

/// FX-CLS-12: with the home folder itself as the scan root, home, OS folders, `~/.ssh` and
/// `~/.gnupg` never become candidates, and scanning home needs confirmation (CFG-04). Uses a
/// stand-in home inside the fixture; OS folders are the real ones (looked up only).
#[test]
fn fx_cls_12_home_os_folders_and_secrets_are_protected() {
    let mut fx = Fixture::new().unwrap();
    fx.file("home/.ssh/id_ed25519", "not a key")
        .unwrap()
        .file("home/.gnupg/pubring.kbx", "x")
        .unwrap()
        .cargo_project("home/projects/app", "app")
        .unwrap()
        .cargo_target("home/projects/app", "app", 16)
        .unwrap();
    fx.checkpoint().unwrap();
    let home = fx.path("home").unwrap();

    let mut set = ProtectedSet::new();
    set.add_defaults(
        &Native,
        &default_protected(TargetOs::current(), Some(&home), &env),
    );

    for p in [
        home.clone(),
        home.join(".ssh"),
        home.join(".ssh").join("id_ed25519"),
        home.join(".gnupg"),
        home.join(".gnupg").join("pubring.kbx"),
    ] {
        let prot = check(&set, &p);
        assert!(prot.in_protected_root, "{} must be protected", p.display());
        let c = classify(&signals(&prot));
        assert_eq!(c.tier(), Tier::S3);
        assert!(
            c.reasons()
                .iter()
                .any(|r| r.code == ReasonCode::ProtectedRoot)
        );
    }
    let target = home.join("projects").join("app").join("target");
    let prot = check(&set, &target);
    assert!(
        !prot.is_protected(),
        "projects inside home stay scannable: {prot:?}"
    );

    // Real OS folders and the filesystem root.
    let os_dirs: Vec<PathBuf> = if cfg!(windows) {
        let windir = env("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        vec![windir.join("System32"), windir, PathBuf::from(r"C:\")]
    } else if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/System/Library"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/etc"),
            PathBuf::from("/"),
        ]
    } else {
        vec![
            PathBuf::from("/usr/bin"),
            PathBuf::from("/etc"),
            PathBuf::from("/"),
        ]
    };
    for d in os_dirs {
        assert!(
            check(&set, &d).in_protected_root,
            "{} must be protected",
            d.display()
        );
    }

    // CFG-04: scanning home itself needs confirmation.
    let confirm = validate_roots(std::slice::from_ref(&home), &home, &[]).unwrap();
    assert!(confirm.iter().any(|c| c.concern == RootConcern::HomeRoot));
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// FX-CLS-11: a project containing `.scull-protect`: every candidate inside is S3.
#[test]
fn fx_cls_11_protect_marker_covers_everything_inside() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("kept", "kept")
        .unwrap()
        .cargo_target("kept", "kept", 16)
        .unwrap()
        .file("kept/.scull-protect", "")
        .unwrap()
        .cargo_project("other", "other")
        .unwrap()
        .cargo_target("other", "other", 16)
        .unwrap();
    fx.checkpoint().unwrap();
    let set = ProtectedSet::new();

    for rel in ["kept", "kept/target", "kept/target/debug"] {
        let prot = check(&set, &fx.path(rel).unwrap());
        assert!(prot.protect_marker, "{rel}: {prot:?}");
        assert!(prot.evidence.iter().any(|e| matches!(
            e,
            Evidence::ProtectMarker { path } if path.as_str().ends_with(".scull-protect")
        )));
        let c = classify(&signals(&prot));
        assert_eq!(c.tier(), Tier::S3);
        assert!(
            c.reasons()
                .iter()
                .any(|r| r.code == ReasonCode::ProtectMarker)
        );
    }
    let other = check(&set, &fx.path("other/target").unwrap());
    assert!(!other.is_protected(), "a sibling project is unaffected");
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// FX-FS-11: a protected folder referenced through a case-different alias is still protected
/// (on case-insensitive filesystems: default macOS APFS, NTFS). Exclusions behave the same.
#[test]
fn fx_fs_11_case_alias_cannot_bypass_protection() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("Protected/sub").unwrap().dir("Vendor/lib").unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    if !resolves_alias(&root, "PROTECTED") {
        eprintln!("case-sensitive filesystem: no case alias exists here; skipping");
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    }
    let mut set = ProtectedSet::new();
    set.protect(
        &Native,
        &root.join("Protected"),
        Scope::Subtree,
        Source::User,
    );
    let mut scan = ScanConfig::default();
    scan.exclude.insert(root.join("vendor")); // configured in lower case
    set.add_config(&Native, &scan);

    for alias in ["PROTECTED", "protected", "pRoTeCtEd"] {
        for p in [
            root.join(alias),
            root.join(alias).join("sub"),
            root.join(alias).join("SUB"),
        ] {
            assert!(
                check(&set, &p).in_protected_root,
                "{} bypassed protection",
                p.display()
            );
        }
    }
    let ex = check(&set, &root.join("VENDOR").join("Lib"));
    assert!(ex.user_excluded, "{ex:?}");
    assert_eq!(classify(&signals(&ex)).tier(), Tier::S3);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Short (8.3) name of an existing path, via `cmd`'s `%~s` expansion.
#[cfg(windows)]
fn short_name(p: &Path) -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("cmd")
        .raw_arg(format!(
            "/d /c for %I in (\"{}\") do @echo %~sI",
            p.display()
        ))
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// FX-WIN-05: a protected folder referenced by its 8.3 short name is still protected.
#[cfg(windows)]
#[test]
fn fx_win_05_short_name_alias_cannot_bypass_protection() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("Protected Long Folder Name/inner").unwrap();
    fx.checkpoint().unwrap();
    let long = fx.path("Protected Long Folder Name").unwrap();
    let short = short_name(&long).expect("short name");
    if short.file_name() == long.file_name() {
        eprintln!("8.3 names are disabled on this volume; skipping");
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    }
    eprintln!("short alias: {}", short.display());
    let mut set = ProtectedSet::new();
    set.protect(&Native, &long, Scope::Subtree, Source::User);
    assert!(check(&set, &short).in_protected_root, "{}", short.display());
    assert!(check(&set, &short.join("inner")).in_protected_root);

    // And the other way round: protected by its short name, reached by its long name.
    let mut set = ProtectedSet::new();
    set.protect(&Native, &short, Scope::Subtree, Source::User);
    assert!(check(&set, &long.join("inner")).in_protected_root);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// FX-FS-09 (identity part): a protected folder named in NFC and reached through its NFD
/// spelling is still protected, where the filesystem treats them as the same name (APFS).
#[test]
fn fx_fs_09_unicode_normalization_alias_cannot_bypass_protection() {
    let nfc = "caf\u{e9}";
    let nfd = "cafe\u{301}";
    let mut fx = Fixture::new().unwrap();
    fx.dir(format!("{nfc}/sub")).unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    if !resolves_alias(&root, nfd) {
        eprintln!("normalization-sensitive filesystem: NFC and NFD are distinct names; skipping");
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    }
    let mut set = ProtectedSet::new();
    set.protect(&Native, &root.join(nfc), Scope::Subtree, Source::User);
    assert!(check(&set, &root.join(nfd)).in_protected_root);
    assert!(check(&set, &root.join(nfd).join("sub")).in_protected_root);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// A symlink to a protected directory: the link itself is not the protected directory (removing
/// a link leaves its target alone), but anything reached *through* it resolves to the target's
/// identity and is protected.
#[test]
fn symlink_to_protected_dir_resolves_to_the_target() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("secret/data").unwrap();
    let outcome = fx.symlink("link", "secret").unwrap();
    fx.checkpoint().unwrap();
    if let LinkOutcome::Skipped(why) = outcome {
        eprintln!("symlinks unavailable: {why}");
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return;
    }
    let mut set = ProtectedSet::new();
    set.protect(
        &Native,
        &fx.path("secret").unwrap(),
        Scope::Subtree,
        Source::User,
    );
    let link = fx.path("link").unwrap();
    assert!(!check(&set, &link).is_protected(), "the link itself");
    assert!(
        check(&set, &link.join("data")).in_protected_root,
        "through the link"
    );

    // Protecting through the link protects the target too.
    let mut set = ProtectedSet::new();
    set.protect(&Native, &link, Scope::Subtree, Source::User);
    assert!(check(&set, &fx.path("secret/data").unwrap()).in_protected_root);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// `..` segments cannot step around a protected root (T-13).
#[test]
fn dot_dot_segments_cannot_bypass_protection() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("secret/data").unwrap().dir("other").unwrap();
    fx.checkpoint().unwrap();
    let mut set = ProtectedSet::new();
    set.protect(
        &Native,
        &fx.path("secret").unwrap(),
        Scope::Subtree,
        Source::User,
    );
    let sneaky = fx
        .root()
        .join("other")
        .join("..")
        .join("secret")
        .join("data");
    assert!(check(&set, &sneaky).in_protected_root);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// A candidate that *contains* a protected path or an exclusion is protected too: deleting it
/// would delete them.
#[test]
fn candidate_containing_protected_path_is_protected() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("proj/node_modules/keep")
        .unwrap()
        .dir("proj/node_modules/excluded")
        .unwrap()
        .dir("proj/dist")
        .unwrap();
    fx.checkpoint().unwrap();
    let mut set = ProtectedSet::new();
    set.protect(
        &Native,
        &fx.path("proj/node_modules/keep").unwrap(),
        Scope::Subtree,
        Source::User,
    );
    let nm = fx.path("proj/node_modules").unwrap();
    let prot = check(&set, &nm);
    assert!(prot.in_protected_root, "{prot:?}");

    let mut set = ProtectedSet::new();
    set.exclude(
        &Native,
        &fx.path("proj/node_modules/excluded").unwrap(),
        Source::User,
    );
    assert!(check(&set, &nm).user_excluded);
    assert!(!check(&set, &fx.path("proj/dist").unwrap()).is_protected());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Carve-outs cancel built-in subtree roots strictly inside them, never user protection and
/// never the carve-out directory itself.
#[test]
fn carve_outs_only_relax_built_in_roots() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("sys/cache/pkg").unwrap().dir("sys/prefs").unwrap();
    fx.checkpoint().unwrap();
    let sys = fx.path("sys").unwrap();
    let defaults = DefaultProtection {
        protected: vec![ProtectedPath {
            path: sys.clone(),
            scope: Scope::Subtree,
        }],
        allowed: vec![sys.join("cache")],
    };
    let mut set = ProtectedSet::new();
    set.add_defaults(&Native, &defaults);
    assert!(!check(&set, &sys.join("cache").join("pkg")).is_protected());
    assert!(
        check(&set, &sys.join("cache")).in_protected_root,
        "carve-out dir itself"
    );
    assert!(check(&set, &sys.join("prefs")).in_protected_root);

    set.protect(&Native, &sys, Scope::Subtree, Source::User);
    assert!(
        check(&set, &sys.join("cache").join("pkg")).in_protected_root,
        "user protection is never carved out"
    );
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// CFG-03: a repository's `.scull.toml` adds protection (protect, exclude) by identity.
#[test]
fn repo_policy_adds_protection() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("repo/data/raw")
        .unwrap()
        .dir("repo/big/blob")
        .unwrap()
        .dir("repo/build")
        .unwrap();
    fx.checkpoint().unwrap();
    let parsed = RepoPolicy::parse_toml("[scan]\nprotect = [\"data\"]\nexclude = [\"big\"]\n");
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let mut set = ProtectedSet::new();
    set.add_repo_policy(&Native, &fx.path("repo").unwrap(), &parsed.policy);
    let data = check(&set, &fx.path("repo/data/raw").unwrap());
    assert!(data.protect_marker, "{data:?}");
    assert!(check(&set, &fx.path("repo/big/blob").unwrap()).user_excluded);
    assert!(!check(&set, &fx.path("repo/build").unwrap()).is_protected());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// A path whose real location can't be resolved is treated as protected, and says why.
#[test]
fn unresolvable_path_is_protected_and_disclosed() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("x").unwrap();
    fx.checkpoint().unwrap();
    let set = ProtectedSet::new();
    let prot = check(&set, &fx.root().join("missing").join("child"));
    assert!(prot.in_protected_root);
    assert!(
        prot.evidence
            .iter()
            .any(|e| matches!(e, Evidence::SignalUnavailable { .. }))
    );
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}

/// Filesystem roots are protected even with an empty set.
#[test]
fn filesystem_root_is_always_protected() {
    let root = if cfg!(windows) {
        PathBuf::from(r"C:\")
    } else {
        PathBuf::from("/")
    };
    assert!(ProtectedSet::new().check(&root, &Native).in_protected_root);
}

/// The built-in defaults for this OS (real home and environment) resolve, and never make the
/// test's own temp fixtures protected (temp carve-outs).
#[test]
fn defaults_leave_temp_fixtures_scannable() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 16)
        .unwrap();
    fx.checkpoint().unwrap();
    let set = ProtectedSet::with_defaults(&Native);
    let prot = set.check(&fx.path("app/target").unwrap(), &Native);
    assert!(!prot.is_protected(), "{prot:?}");
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
}
