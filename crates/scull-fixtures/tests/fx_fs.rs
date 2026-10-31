//! FX-FS end-to-end fixtures (docs/FIXTURES.md §3.3, P1-FX-04): filesystem edge cases run
//! through the read-only pipelines (`common::Pipeline` for artifacts, `common::worktrees` for
//! Git), each twice (scan, revalidation), checking I-1, I-2, I-3 and I-8.
//!
//! Halves that need the Executor (removing links, read-only or denied entries, very long
//! paths) run against a `std::fs` stand-in where safe and are tracked in the registry under
//! #83 (FX-EXEC fixtures) until the Executor exists (#67, #68).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::worktrees::{GitWorld, WorktreePipeline, run_wt_checked};
use common::{Pipeline, check_all, rel, run_checked};
use scull_core::{Confidence, ReasonCode, Tier};
use scull_fixtures::{Fixture, NodeLock, days_ago};
use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

/// A hang means a loop was followed or a special file was opened.
const HANG: Duration = Duration::from_secs(60);

/// Run `f` on a thread; fail the test if it doesn't finish within [`HANG`].
fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(HANG).expect("pipeline hung")
}

/// An npm project at `rel` with a lockfile and a `node_modules` holding `left-pad`.
fn npm(fx: &mut Fixture, rel: &str) {
    fx.npm_project(rel, "app", NodeLock::Npm)
        .unwrap()
        .node_modules(rel, &["left-pad"])
        .unwrap();
}

// ---------------------------------------------------------------------------------------------

/// FX-FS-01 (scan half; removal tracked under #83): a symlink inside a candidate pointing to the
/// canary directory → counted as a link (the canary's bytes are not the candidate's), the
/// candidate is planned, and removing it (stand-in) removes the link only: canary intact (I-2).
#[test]
fn fx_fs_01_symlink_to_canary_inside_candidate_link_removed_canary_intact() {
    let mut fx = Fixture::new().unwrap();
    npm(&mut fx, "web");
    let canary = fx.canary().symlink_target_dir.clone();
    let canary_file = fx.canary().symlink_target_file.clone();
    let dir_link = fx.symlink("web/node_modules/escape", &canary).unwrap();
    let file_link = fx
        .symlink("web/node_modules/escape.txt", &canary_file)
        .unwrap();
    let junction = fx.junction("web/node_modules/escape-jn", &canary).unwrap();
    fx.age("web/node_modules", days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();

    let pipeline = Pipeline::new(fx.root());
    let scan = pipeline.run();
    let revalidated = pipeline.run();
    check_all(&fx, &scan, &revalidated, &[]);
    let item = scan.at("web/node_modules");
    assert_eq!(
        item.candidate.tier,
        Tier::S0,
        "{:?}",
        item.candidate.reasons
    );
    let made = [&dir_link, &file_link, &junction]
        .iter()
        .filter(|l| l.is_created())
        .count() as u64;
    let s = item.summary.as_ref().unwrap();
    assert_eq!(
        s.totals.links, made,
        "links counted as links, never followed"
    );
    // The canary's files are not inside the candidate's size.
    let canary_bytes: u64 = fs::read_dir(&canary)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    assert!(canary_bytes > 0);
    let own: u64 = ["package.json", "index.js"]
        .iter()
        .map(|f| {
            fs::metadata(fx.root().join("web/node_modules/left-pad").join(f))
                .unwrap()
                .len()
        })
        .sum();
    assert!(s.totals.files == 2, "{:?}", s.totals);
    assert!(s.totals.logical >= own);

    // Execute (stand-in for the Executor): only the links go.
    fs::remove_dir_all(&item.artifact.path).unwrap();
    check_all(&fx, &scan, &revalidated, &["web/node_modules"]);
}

/// FX-FS-02: symlink loops inside a scan root → the scan terminates; each loop link is
/// recorded once, as a link, and never followed.
#[test]
fn fx_fs_02_symlink_loops_terminate_and_are_recorded_once() {
    let mut fx = Fixture::new().unwrap();
    npm(&mut fx, "web");
    let mut made = 0u64;
    for (link, target) in [
        ("web/node_modules/self", "."),
        ("web/node_modules/up", "../.."),
        ("loops/a", "b"),
        ("loops/b", "a"),
        ("loops/root", ".."),
    ] {
        if link.starts_with("loops/") {
            fx.dir("loops").unwrap();
        }
        made += u64::from(fx.symlink(link, target).unwrap().is_created());
    }
    fx.age("web/node_modules", days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();

    let root = fx.root().to_path_buf();
    let (scan, revalidated) = within(move || {
        let p = Pipeline::new(&root);
        (p.run(), p.run())
    });
    check_all(&fx, &scan, &revalidated, &[]);
    assert_eq!(
        scan.outcome.totals.links, made,
        "each loop link recorded once"
    );
    assert!(scan.issues.is_empty(), "{:?}", scan.issues);
    assert_eq!(scan.rel_paths(), ["web/node_modules"]);
    assert_eq!(scan.at("web/node_modules").candidate.tier, Tier::S0);
    // Nothing beneath a link was visited.
    for v in &scan.visited {
        let r = rel(fx.root(), v);
        assert!(!r.contains("self/") && !r.contains("up/") && !r.starts_with("loops/a"));
    }
}

/// FX-FS-03 (partial; the engine's link-escape signal is tracked under #56): a candidate
/// reached only through a symlink that points outside the scan root is never reached: the
/// scanner doesn't follow the link, and scanning through the link itself is refused.
#[test]
fn fx_fs_03_candidate_behind_an_escaping_link_is_never_reached() {
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("outside/app", "app")
        .unwrap()
        .cargo_target("outside/app", "app", 1024)
        .unwrap()
        .age("outside/app/target", days_ago(3))
        .unwrap()
        .dir("scan")
        .unwrap();
    let outside = fx.root().join("outside");
    let link = fx.symlink("scan/projects", &outside).unwrap();
    fx.checkpoint().unwrap();

    let scan_root = fx.root().join("scan");
    let run = run_checked(&fx, &Pipeline::new(&scan_root));
    assert!(run.items.is_empty(), "{:?}", run.rel_paths());
    if link.is_created() {
        // The link itself as a scan root: refused, never followed.
        let err = scull_scanner::scan(
            std::sync::Arc::new(scull_platform::Native::new()),
            &scan_root.join("projects"),
            &scull_scanner::ScanOptions::default(),
            &scull_scanner::CancelToken::new(),
            scull_scanner::NullSink,
        );
        assert!(matches!(
            err,
            Err(scull_scanner::ScanError::RootNotADirectory { .. })
        ));
    }
}

/// FX-FS-05 (scan half; removal tracked under #83): a file hard-linked between a candidate and
/// the canary → the canary file is intact and its bytes are attributed once: the candidate's
/// reclaimable estimate counts them as shared, not freed.
#[test]
fn fx_fs_05_hardlink_to_canary_bytes_attributed_once() {
    let mut fx = Fixture::new().unwrap();
    npm(&mut fx, "web");
    let canary_file = fx.canary().symlink_target_file.clone();
    // Age first: the link shares the canary file's mtime, which must not change (I-2).
    fx.age("web/node_modules", days_ago(3)).unwrap();
    fx.hardlink("web/node_modules/shared.txt", &canary_file)
        .unwrap()
        .age_one("web/node_modules", days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    let item = run.at("web/node_modules");
    let s = item.summary.as_ref().unwrap();
    {
        let len = fs::metadata(&canary_file).unwrap().len();
        assert!(len > 0);
        assert!(s.shared_allocated > 0 || s.reclaim_confidence != Confidence::High);
        let r = &item.candidate.sizes.reclaimable;
        assert_eq!(r.bytes + r.shared_bytes, item.candidate.sizes.allocated);
        assert!(
            r.shared_bytes > 0 || r.confidence != Confidence::High,
            "{r:?}"
        );
    }
}

/// FX-FS-06: a 10 GB sparse file with 1 MB allocated → the allocated size is used for
/// estimates. (Where the filesystem doesn't make it sparse, allocated equals logical and the
/// estimate still uses allocated.)
#[test]
fn fx_fs_06_sparse_file_estimate_uses_allocated_size() {
    // NTFS only makes a file sparse with FSCTL_SET_SPARSE (not available from std), so a
    // 10 GB length would reserve 10 GB on the runner: use 256 MB there.
    const LOGICAL: u64 = if cfg!(windows) {
        256 * 1024 * 1024
    } else {
        10 * 1024 * 1024 * 1024
    };
    let mut fx = Fixture::new().unwrap();
    fx.cargo_project("app", "app")
        .unwrap()
        .cargo_target("app", "app", 1024)
        .unwrap()
        .age("app/target", days_ago(3))
        .unwrap()
        // Baseline before the sparse file exists: the harness's snapshots hash every file, and
        // hashing 10 GB (in a debug build) would take minutes. The file is created after the
        // checkpoint, removed before the invariants are checked, and allowed in I-3.
        .checkpoint()
        .unwrap();
    let sparse = fx.path("app/target/debug/huge.bin").unwrap();
    {
        use std::io::Write;
        let mut f = fs::File::create(&sparse).unwrap();
        f.write_all(&vec![7u8; 1024 * 1024]).unwrap();
        // Extending without writing leaves a hole on filesystems that support sparse files
        // (ext4, APFS).
        f.set_len(LOGICAL).expect("extend the file");
    }
    let md = scull_platform::Platform::metadata(&scull_platform::Native::new(), &sparse).unwrap();
    let allocated = md.allocated_size.expect("allocated size known");

    let pipeline = Pipeline::new(fx.root());
    let scan = pipeline.run();
    let revalidated = pipeline.run();
    fs::remove_file(&sparse).unwrap();
    check_all(&fx, &scan, &revalidated, &["app/target/debug"]);
    let c = &scan.at("app/target").candidate;
    assert!(c.sizes.logical >= LOGICAL);
    assert!(c.sizes.allocated >= allocated);
    assert!(c.sizes.reclaimable.bytes <= c.sizes.allocated);
    if allocated < LOGICAL / 2 {
        // Actually sparse: the estimate is about the 1 MB written, not the 10 GB length.
        assert!(
            c.sizes.allocated < 64 * 1024 * 1024,
            "allocated {} for a sparse file",
            c.sizes.allocated
        );
        assert!(c.sizes.reclaimable.bytes < 64 * 1024 * 1024);
    } else {
        eprintln!("note: this filesystem did not make the file sparse ({allocated} allocated)");
    }
}

/// FX-FS-07 (scan half; deletion tracked under #83): read-only files and folders inside a
/// candidate → classified normally; nothing (permissions included, checked by I-3) changes,
/// inside or outside the candidate.
#[test]
fn fx_fs_07_read_only_entries_inside_candidate_permissions_untouched() {
    let mut fx = Fixture::new().unwrap();
    npm(&mut fx, "web");
    fx.file("web/node_modules/left-pad/locked.js", "x").unwrap();
    fx.file("web/README.md", "keep").unwrap();
    fx.age("web/node_modules", days_ago(3)).unwrap();
    let ro = |p: &Path| {
        let mut perm = fs::metadata(p).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(p, perm).unwrap();
    };
    let locked = fx.root().join("web/node_modules/left-pad/locked.js");
    let readme = fx.root().join("web/README.md");
    ro(&locked);
    ro(&readme);
    if cfg!(unix) {
        ro(&fx.root().join("web/node_modules/left-pad"));
    }
    fx.checkpoint().unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    let item = run.at("web/node_modules");
    assert_eq!(
        item.candidate.tier,
        Tier::S0,
        "{:?}",
        item.candidate.reasons
    );
    assert!(item.summary.as_ref().unwrap().complete);
    assert!(fs::metadata(&readme).unwrap().permissions().readonly());
    // Let the temp dir clean up.
    for p in [fx.root().join("web/node_modules/left-pad"), locked, readme] {
        let mut perm = fs::metadata(&p).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        fs::set_permissions(&p, perm).unwrap();
    }
}

/// FX-FS-08 (scan half; the executor's partial/failed outcome is tracked under #83): a
/// permission-denied subfolder inside a candidate → the scan reports it with its reason and
/// goes on; the candidate's size is marked incomplete (low confidence); nothing escalates.
#[cfg(unix)]
#[test]
fn fx_fs_08_permission_denied_subfolder_is_reported_not_escalated() {
    use std::os::unix::fs::PermissionsExt;
    let mut fx = Fixture::new().unwrap();
    npm(&mut fx, "web");
    fx.file("web/node_modules/secret/inner.js", "x").unwrap();
    fx.age("web/node_modules", days_ago(3)).unwrap();
    let denied = fx.root().join("web/node_modules/secret");
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&denied).is_ok() {
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipping: running with privileges that bypass permissions");
        fx.check_invariants(&scull_fixtures::InvariantInputs::new());
        return;
    }
    fx.checkpoint().unwrap();

    let pipeline = Pipeline::new(fx.root());
    let scan = pipeline.run();
    let revalidated = pipeline.run();
    let restore = || fs::set_permissions(&denied, fs::Permissions::from_mode(0o755)).unwrap();
    let issue = scan.issues.iter().find(|i| i.path == denied).cloned();
    let item = scan.at("web/node_modules").clone();
    check_all(&fx, &scan, &revalidated, &[]);
    restore();
    let issue = issue.expect("permission-denied issue reported");
    assert_eq!(issue.kind, scull_scanner::IssueKind::PermissionDenied);
    let s = item.summary.as_ref().unwrap();
    assert!(!s.complete, "the candidate is marked incomplete");
    assert_ne!(
        item.candidate.sizes.reclaimable.confidence,
        Confidence::High
    );
    // Incomplete knowledge never lowers protection: idle activity can't be proven → not S0.
    assert!(
        item.candidate.tier >= Tier::S1,
        "{:?}",
        item.candidate.reasons
    );
}

/// FX-FS-09: Unicode names (NFC vs NFD duplicates, emoji, right-to-left override) → correct
/// identity handling (two distinct directories are two candidates with distinct IDs, an alias
/// is one), and the right-to-left override is escaped and flagged for display.
#[test]
fn fx_fs_09_unicode_names_identity_and_rtl_override_flagged() {
    use scull_cli::output::escape::{escape_for_terminal, is_suspicious_name};
    let nfc = "caf\u{e9}";
    let nfd = "cafe\u{301}";
    let rtl = "invoice\u{202e}fdp.exe";
    let emoji = "box-\u{1f4e6}";
    let mut fx = Fixture::new().unwrap();
    npm(&mut fx, nfc);
    // On a normalization-insensitive filesystem (APFS) the NFD name is the same directory.
    let nfd_is_alias = fs::symlink_metadata(fx.root().join(nfd)).is_ok();
    if !nfd_is_alias {
        npm(&mut fx, nfd);
    }
    npm(&mut fx, rtl);
    npm(&mut fx, emoji);
    for d in [nfc, nfd, rtl, emoji] {
        if fs::symlink_metadata(fx.root().join(d).join("node_modules")).is_ok() {
            fx.age(format!("{d}/node_modules"), days_ago(3)).unwrap();
        }
    }
    fx.checkpoint().unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    let expected = if nfd_is_alias { 3 } else { 4 };
    assert_eq!(run.items.len(), expected, "{:?}", run.rel_paths());
    let mut ids: Vec<_> = run.items.iter().map(|i| i.candidate.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), expected, "distinct directories get distinct IDs");
    for item in &run.items {
        assert_eq!(
            item.candidate.tier,
            Tier::S0,
            "{:?}",
            item.candidate.reasons
        );
        let path = item.candidate.path.to_string();
        let shown = escape_for_terminal(&path);
        let raw = rel(fx.root(), &item.artifact.path);
        if raw.contains('\u{202e}') {
            assert!(shown.contains("\\u{202e}"), "{shown}");
            assert!(!shown.contains('\u{202e}'));
            assert!(is_suspicious_name(&raw));
        } else {
            assert!(!is_suspicious_name(&raw), "{raw}");
            // Emoji and accented letters pass through unchanged.
            assert!(!shown.contains("\\u{"), "{shown}");
        }
    }
}

/// FX-FS-10: names containing newlines, tabs and ANSI escape codes → escaped in table output,
/// valid JSON, terminal state unchanged. Windows forbids control characters in names, so there
/// the fixture checks that such names can't be created at all.
#[test]
fn fx_fs_10_control_characters_in_names_are_escaped_and_json_stays_valid() {
    use scull_cli::output::escape::{escape_for_terminal, needs_escape};
    let names = ["new\nline", "tab\there", "ansi\u{1b}[2J\u{1b}[31mred"];
    let mut fx = Fixture::new().unwrap();
    let mut created = Vec::new();
    for n in names {
        match fx.npm_project(n, "app", NodeLock::Npm) {
            Ok(_) => {
                fx.node_modules(n, &["left-pad"]).unwrap();
                fx.age(format!("{n}/node_modules"), days_ago(3)).unwrap();
                created.push(n);
            }
            // Only Windows refuses control characters in names.
            Err(e) if cfg!(windows) => eprintln!("{n:?} can't exist on Windows: {e}"),
            Err(e) => panic!("{n:?} could not be created: {e}"),
        }
    }
    fx.checkpoint().unwrap();

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    assert_eq!(run.items.len(), created.len());
    for item in &run.items {
        let path = item.candidate.path.to_string();
        // Table output: every control character is escaped; nothing can move the cursor,
        // clear the screen or fake a row.
        let shown = escape_for_terminal(&path);
        assert!(!shown.chars().any(needs_escape), "{shown:?}");
        assert!(!shown.contains('\n') && !shown.contains('\u{1b}'));
        // JSON: valid, round-trips the exact name, and carries no raw control character.
        let json = serde_json::to_string(&item.candidate).unwrap();
        assert!(!json.chars().any(|c| c.is_control()), "{json}");
        let back: scull_core::Candidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.path.to_string(), path);
    }
    if cfg!(unix) {
        assert_eq!(created.len(), names.len());
    }
}

/// FX-FS-11: a protected folder referenced through a case-different alias → still protected
/// (identity-based comparison). Runs where the filesystem is case-insensitive (macOS, Windows
/// per the OS column); on a case-sensitive filesystem there is no alias to test.
#[test]
fn fx_fs_11_protected_folder_by_case_alias_is_still_protected() {
    let mut fx = Fixture::new().unwrap();
    npm(&mut fx, "Precious");
    npm(&mut fx, "Other");
    fx.age("Precious/node_modules", days_ago(3))
        .unwrap()
        .age("Other/node_modules", days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();
    let alias = fx.root().join("PRECIOUS");
    if fs::symlink_metadata(&alias).is_err() {
        eprintln!("skipping: case-sensitive filesystem, no alias exists (OS column: MW)");
        fx.check_invariants(&scull_fixtures::InvariantInputs::new());
        return;
    }

    let run = run_checked(&fx, &Pipeline::new(fx.root()).protect(&alias));
    let c = &run.at("Precious/node_modules").candidate;
    assert_eq!(c.tier, Tier::S3, "{:?}", c.reasons);
    assert!(
        c.reasons
            .iter()
            .any(|r| r.code == ReasonCode::ProtectedRoot)
    );
    assert!(!run.plan().iter().any(|p| p.id == c.id));
    assert_eq!(run.at("Other/node_modules").candidate.tier, Tier::S0);
}

/// FX-FS-12 (partial; very long paths beyond the OS path limit and their deletion are tracked
/// under #83): a candidate whose path is longer than 260 characters (the classic Windows
/// limit) and has a long name chain is scanned, sized and classified correctly.
#[test]
fn fx_fs_12_long_path_candidate_is_scanned_and_classified() {
    let mut fx = Fixture::new().unwrap();
    let segment = "d".repeat(60);
    let deep = (0..6)
        .map(|_| segment.as_str())
        .collect::<Vec<_>>()
        .join("/");
    let project = format!("{deep}/proj");
    fx.cargo_project(&project, "proj")
        .unwrap()
        .cargo_target(&project, "proj", 2048)
        .unwrap()
        .age(format!("{project}/target"), days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();
    let target = fx.root().join(&project).join("target");
    assert!(target.to_string_lossy().len() > 260);

    let run = run_checked(&fx, &Pipeline::new(fx.root()));
    let item = run.at(&format!("{project}/target"));
    assert_eq!(
        item.candidate.tier,
        Tier::S0,
        "{:?}",
        item.candidate.reasons
    );
    assert!(item.candidate.sizes.logical >= 2048);
    assert!(item.summary.as_ref().unwrap().complete);
    assert!(run.issues.is_empty(), "{:?}", run.issues);
}

/// FX-FS-13: a folder named `--force` or `-rf` passed as a root or target → treated as a path,
/// never as an option: the CLI parses it as a path after `--` (and rejects it as an unknown
/// flag otherwise, rather than guessing), and Git is driven over a repository at `--force/`
/// with a worktree at `-rf/` without the names ever being read as options.
#[test]
fn fx_fs_13_option_like_folder_names_are_paths() {
    use scull_cli::cli::{Cli, Command};
    let cli = Cli::try_parse_checked(["scull", "scan", "--", "--force", "-rf"]).unwrap();
    match cli.command {
        Command::Scan(args) => assert_eq!(
            args.paths,
            [std::path::PathBuf::from("--force"), "-rf".into()]
        ),
        other => panic!("parsed as {other:?}"),
    }
    assert!(Cli::try_parse_checked(["scull", "scan", "--force"]).is_err());
    let cli = Cli::try_parse_checked(["scull", "large-files", "--", "-rf"]).unwrap();
    assert!(matches!(cli.command, Command::LargeFiles { path, .. } if path == Path::new("-rf")));

    // Git over option-like folder names, end to end.
    let mut w = GitWorld::local();
    w.fx.dir("--force").unwrap();
    w.git(&[
        "worktree", "add", "--quiet", "-b", "agent", "../-rf", "main",
    ]);
    w.fx.file("-rf/notes.txt", "mine\n").unwrap();
    npm(&mut w.fx, "--force/web");
    w.fx.age("--force/web/node_modules", days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();
    let run = run_wt_checked(&w.fx, &WorktreePipeline::new(w.fx.root()));
    let wt = run.at("-rf");
    assert_eq!(wt.candidate.tier, Tier::S3, "{:#?}", wt.decision);
    assert!(wt.has_reason(ReasonCode::WorktreeDirty, Tier::S3));
    assert!(
        wt.decision
            .reasons
            .iter()
            .any(|r| r.message.contains("notes.txt"))
    );
    let artifacts = run_checked(&w.fx, &Pipeline::new(&w.fx.root().join("--force")));
    assert_eq!(artifacts.rel_paths(), ["web/node_modules"]);
    assert_eq!(artifacts.items[0].candidate.tier, Tier::S0);
}

/// FX-FS-14: FIFO, socket and device node inside a candidate → never opened; the scan doesn't
/// hang; the item is flagged (special entries counted in its summary). Device nodes need root
/// to create; the scanner's own tests cover them from `/dev` and a simulated tree.
#[cfg(unix)]
#[test]
fn fx_fs_14_fifo_and_socket_inside_candidate_never_opened() {
    use std::os::unix::net::UnixListener;
    let mut fx = Fixture::new().unwrap();
    npm(&mut fx, "web");
    let nm = fx.root().join("web/node_modules");
    let ok = std::process::Command::new("mkfifo")
        .arg(nm.join("left-pad/pipe"))
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "mkfifo");
    // Socket paths are limited to ~104 bytes: bind via a short relative name? Not portable;
    // bind at the absolute path when it fits.
    let sock_path = nm.join("s.sock");
    let _sock =
        (sock_path.as_os_str().len() < 100).then(|| UnixListener::bind(&sock_path).unwrap());
    fx.age("web/node_modules", days_ago(3))
        .unwrap()
        .checkpoint()
        .unwrap();

    let root = fx.root().to_path_buf();
    let (scan, revalidated) = within(move || {
        let p = Pipeline::new(&root);
        (p.run(), p.run())
    });
    check_all(&fx, &scan, &revalidated, &[]);
    let item = scan.at("web/node_modules");
    let specials = item.summary.as_ref().unwrap().totals.specials;
    assert_eq!(
        specials,
        1 + u64::from(_sock.is_some()),
        "flagged via its summary"
    );
    assert_eq!(
        scull_platform::Platform::metadata(
            &scull_platform::Native::new(),
            &nm.join("left-pad/pipe")
        )
        .unwrap()
        .kind,
        scull_platform::EntryKind::Fifo
    );
}
