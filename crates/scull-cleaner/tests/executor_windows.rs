//! Executor fixtures on Windows (P2-EXEC-02): the handle-based, no-follow removal walker driven
//! through [`Executor::delete`]. Every test builds its tree in a `scull-fixtures` temp
//! directory with a canary beside the root and checks the invariants (I-2 canary untouched,
//! I-3 only the plan item changed). Nothing here touches a path outside its own fixture.
//!
//! Fixture IDs (docs/FIXTURES.md): FX-WIN-01, -02, -03, -04, -05, -10, -11; FX-FS-01, -05, -07,
//! -08, -12; FX-RACE-01, FX-RACE-02.
//!
//! Symlink fixtures need the symlink privilege (admin or Developer Mode); where it is missing
//! the fixture records the skip and the test says why. Junctions need no privilege.
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(windows)]

mod common;

use common::*;
use scull_cleaner::{Anomaly, DeleteTarget, Executor, RemoveOutcome, SkipReason, WalkEvent};
use scull_core::{ErrorKind, PlanItem};
use scull_fixtures::Fixture;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A small candidate: `cand/` with files, a nested directory and a sub-directory `sub/`.
fn small_tree(fx: &mut Fixture) {
    fx.file("cand/a.txt", b"a")
        .unwrap()
        .file_of_size("cand/b.bin", 8192)
        .unwrap()
        .file("cand/sub/c.txt", b"c")
        .unwrap()
        .file("cand/sub/inner/d.txt", b"d")
        .unwrap()
        .file("cand/sub/inner/e.txt", b"e")
        .unwrap()
        .file(
            "keep/outside.txt",
            b"sibling of the candidate, never touched",
        )
        .unwrap();
}

/// The canary's junction target still has its content.
fn assert_junction_target(fx: &Fixture) {
    assert_eq!(
        fs::read(fx.canary().junction_target.join("keep.txt")).unwrap(),
        b"junction target content\n"
    );
}

/// `mklink /J link target` (no privilege needed), for swaps inside observers where the fixture
/// builder (which needs `&mut Fixture`) is not available.
fn mklink_junction(link: &Path, target: &Path) {
    use std::os::windows::process::CommandExt;
    let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    let out = Command::new(shell)
        .raw_arg(format!(
            "/D /C mklink /J \"{}\" \"{}\"",
            link.display(),
            target.display()
        ))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// `\\?\` + `path`, so Win32 does not rewrite reserved names or trailing dots and spaces.
fn verbatim(path: &Path) -> PathBuf {
    let mut w: Vec<u16> = r"\\?\".encode_utf16().collect();
    w.extend(path.as_os_str().encode_wide());
    PathBuf::from(OsString::from_wide(&w))
}

// ------------------------------------------------------------------------------------------
// FX-WIN-01 / -02: junctions and symlinks are removed as links, never followed
// ------------------------------------------------------------------------------------------

/// FX-WIN-01: junctions inside a candidate pointing at the canary's junction target and at the
/// canary itself → the junctions are removed, the targets are intact (I-2).
#[test]
fn fx_win_01_junction_inside_candidate_removed_target_intact() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    let target = fx.canary().junction_target.clone();
    let canary = fx.canary().dir.clone();
    assert!(fx.junction("cand/escape", &target).unwrap().is_created());
    assert!(fx.junction("cand/sub/up", &canary).unwrap().is_created());
    fx.checkpoint().unwrap();

    print_tree(&fx, "FX-WIN-01 before");
    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    println!("report: {:?} {:?}", r.report.outcome, r.report.removed);
    print_tree(&fx, "FX-WIN-01 after");
    assert_complete(&r);
    assert_eq!(r.report.removed.symlinks, 2, "{:#?}", r.report);
    assert_eq!(r.report.removed.files, 5, "{:#?}", r.report);
    assert_eq!(r.report.removed.dirs, 3, "{:#?}", r.report);
    assert!(!fx.root().join("cand").exists());
    assert_junction_target(&fx);
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-WIN-01: the item itself is a junction to the canary → the junction is removed, never
/// its target's contents.
#[test]
fn fx_win_01_junction_item_removes_only_the_junction() {
    let mut fx = Fixture::new().unwrap();
    let target = fx.canary().junction_target.clone();
    assert!(fx.junction("cand", &target).unwrap().is_created());
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.symlinks, 1);
    assert!(fs::symlink_metadata(fx.root().join("cand")).is_err());
    assert_junction_target(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// SAFE-04: a junction between the anchor and the item → refused, never traversed, even though
/// it points inside the root.
#[test]
fn junction_in_the_path_below_the_anchor_is_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("real/cand/x.txt", b"x").unwrap();
    let real = fx.root().join("real");
    assert!(fx.junction("via", &real).unwrap().is_created());
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "via/cand");
    let e = delete(&fx, &item, "via/cand").unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");
    assert!(fx.root().join("real/cand/x.txt").exists());
    check(&fx, &[&item], &[]);
}

/// FX-WIN-02 / FX-FS-01: directory and file symlinks inside a candidate pointing at the canary
/// (absolute, relative, dangling) → the links are removed, the targets are intact.
#[test]
fn fx_win_02_symlinks_inside_candidate_removed_targets_intact() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    let canary_dir = fx.canary().symlink_target_dir.clone();
    let canary_file = fx.canary().symlink_target_file.clone();
    let links = [
        fx.symlink("cand/escape", &canary_dir).unwrap(),
        fx.symlink("cand/escape.txt", &canary_file).unwrap(),
        fx.symlink("cand/sub/up", r"..\..\..\canary").unwrap(),
        fx.symlink("cand/sub/dangling", r"C:\nonexistent\scull")
            .unwrap(),
    ];
    if links.iter().any(|l| !l.is_created()) {
        eprintln!(
            "SKIP FX-WIN-02: symlinks are not permitted here (needs admin or Developer Mode): {:?}",
            fx.skipped()
        );
        fx.checkpoint().unwrap();
        check(&fx, &[], &[]);
        return;
    }
    fx.checkpoint().unwrap();

    print_tree(&fx, "FX-WIN-02 before");
    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    print_tree(&fx, "FX-WIN-02 after");
    assert_complete(&r);
    assert_eq!(r.report.removed.symlinks, 4, "{:#?}", r.report);
    assert_eq!(r.report.removed.files, 5, "{:#?}", r.report);
    assert!(!fx.root().join("cand").exists());
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-WIN-02: the item itself is a directory symlink to the canary → only the link goes.
#[test]
fn fx_win_02_symlink_item_removes_only_the_link() {
    let mut fx = Fixture::new().unwrap();
    let canary_dir = fx.canary().symlink_target_dir.clone();
    if !fx.symlink("cand", &canary_dir).unwrap().is_created() {
        eprintln!("SKIP FX-WIN-02 (item): symlinks are not permitted here");
        return;
    }
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.symlinks, 1);
    assert!(fs::symlink_metadata(fx.root().join("cand")).is_err());
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

// ------------------------------------------------------------------------------------------
// FX-WIN-03: a file held open by another process
// ------------------------------------------------------------------------------------------

const HOLD_ENV: &str = "SCULL_TEST_HOLD_FILE";
const HOLDING: &str = "SCULL-HOLDING";

/// Child mode: open `path` with no sharing at all, say so, and keep it open until the parent
/// closes our stdin.
fn hold_file(path: &OsString) {
    use std::os::windows::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
        .expect("open the file exclusively");
    println!("{HOLDING}");
    std::io::stdout().flush().unwrap();
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    drop(file);
}

/// FX-WIN-03: a file inside the candidate is held open (no sharing) by another process → it is
/// kept and reported `Busy`, its ancestors are kept, the rest of the candidate is removed, and
/// the detail names the holding process from Restart Manager. Nothing is forced.
#[test]
fn fx_win_03_file_held_open_by_another_process_is_skipped_and_named() {
    if let Some(path) = std::env::var_os(HOLD_ENV) {
        hold_file(&path);
        return;
    }
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.file("cand/sub/locked.txt", b"held open elsewhere")
        .unwrap()
        .checkpoint()
        .unwrap();
    let locked = fx.root().join("cand/sub/locked.txt");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fx_win_03_file_held_open_by_another_process_is_skipped_and_named",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLD_ENV, &locked)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let child_pid = child.id();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    loop {
        match lines.next() {
            Some(Ok(line)) if line.trim() == HOLDING => break,
            Some(Ok(_)) => continue,
            other => panic!("the holder exited before opening the file: {other:?}"),
        }
    }
    // Opening it ourselves now fails: it really is locked.
    assert!(fs::OpenOptions::new().read(true).open(&locked).is_err());

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();

    // Release the holder before asserting, so a failure doesn't leave it running.
    drop(child.stdin.take());
    let status = child.wait().unwrap();
    drop(lines);
    println!("report: {:#?}", r.report);
    assert!(status.success(), "holder process failed: {status}");

    assert_eq!(
        r.report.outcome,
        RemoveOutcome::Incomplete,
        "{:#?}",
        r.report
    );
    assert_eq!(r.report.skipped.len(), 1, "{:#?}", r.report);
    let kept = &r.report.skipped[0];
    assert_eq!(kept.relative, Path::new("sub/locked.txt"));
    assert_eq!(kept.reason, SkipReason::Busy);
    assert!(
        kept.detail.contains(&format!("pid {child_pid}")),
        "Restart Manager should name the holder (pid {child_pid}): {}",
        kept.detail
    );
    assert_eq!(fs::read(&locked).unwrap(), b"held open elsewhere");
    // Everything else is gone; the kept file's ancestors remain.
    assert!(!fx.root().join("cand/a.txt").exists());
    assert!(!fx.root().join("cand/sub/inner").exists());
    check(&fx, &[&item], &["cand"]);
}

// ------------------------------------------------------------------------------------------
// FX-WIN-04 / FX-FS-12: long paths and deep trees
// ------------------------------------------------------------------------------------------

/// FX-WIN-04: a candidate whose paths are far longer than 260 characters → removed (the anchor
/// is opened through its `\\?\` form, everything below relative to handles).
#[test]
fn fx_win_04_paths_longer_than_260_characters_are_removed() {
    let mut fx = Fixture::new().unwrap();
    let seg = "a-fairly-long-directory-name-for-fx-win-04-0123456789";
    let rel = format!("cand/{seg}/{seg}/{seg}/{seg}/{seg}/{seg}");
    fx.file(format!("{rel}/deep-file.txt"), b"deep").unwrap();
    fx.checkpoint().unwrap();
    let deepest = fx.root().join(&rel).join("deep-file.txt");
    let len = deepest.as_os_str().len();
    println!("deepest path: {len} characters");
    assert!(len > 300, "{len}");

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.files, 1);
    assert_eq!(r.report.removed.dirs, 7);
    assert!(!fx.root().join("cand").exists());
    check(&fx, &[&item], &["cand"]);
}

/// FX-FS-12 / deep trees: a 1000-level chain (paths far beyond `MAX_PATH`) is removed
/// iteratively, handle-relative (no recursion, no stack overflow).
#[test]
fn fx_fs_12_very_deep_tree_is_removed() {
    const DEPTH: usize = 1000;
    let mut fx = Fixture::new().unwrap();
    fx.dir("cand").unwrap().checkpoint().unwrap();
    // Built after the checkpoint with short paths only: grow the chain at the top by renames.
    let cand = fx.root().join("cand");
    let chain = cand.join("chain");
    let tmp = cand.join("p");
    fs::create_dir(&chain).unwrap();
    fs::write(chain.join("leaf.txt"), b"bottom").unwrap();
    for _ in 0..DEPTH {
        fs::create_dir(&tmp).unwrap();
        fs::rename(&chain, tmp.join("d")).unwrap();
        fs::rename(&tmp, &chain).unwrap();
    }

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_complete(&r);
    assert_eq!(
        r.report.removed.dirs,
        DEPTH as u64 + 2,
        "cand + chain + d x DEPTH"
    );
    assert_eq!(r.report.removed.files, 1);
    check(&fx, &[&item], &["cand"]);
}

// ------------------------------------------------------------------------------------------
// FX-WIN-05: 8.3 short names
// ------------------------------------------------------------------------------------------

/// The 8.3 short form of `path` from `cmd`'s `%~s` expansion, or `None`.
fn short_path(path: &Path) -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
    let out = Command::new(shell)
        .raw_arg(format!(
            "/D /C for %I in (\"{}\") do @echo %~sI",
            path.display()
        ))
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (out.status.success() && !s.is_empty()).then(|| PathBuf::from(s))
}

/// FX-WIN-05: the planned item referenced by its 8.3 short name → refused before the disk is
/// touched (the native path must be the planned item's: `PlanInvalidated`); a protected folder
/// reached through a short-name alias is never removed. (The walker's own resolution of short
/// names by identity is covered in `scull-platform`'s walker tests.)
#[test]
fn fx_win_05_short_name_alias_is_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("candidate with a long name/a.txt", b"a")
        .unwrap()
        .checkpoint()
        .unwrap();
    let cand = fx.root().join("candidate with a long name");
    let item = s0_delete(&fx, "candidate with a long name");
    let Some(short) = short_path(&cand) else {
        eprintln!("SKIP FX-WIN-05: no short path available");
        check(&fx, &[], &[]);
        return;
    };
    if short.file_name() == cand.file_name() {
        eprintln!("SKIP FX-WIN-05: 8.3 short names are disabled on this volume");
        check(&fx, &[], &[]);
        return;
    }
    println!("{} -> {}", cand.display(), short.display());
    let short_rel = short.strip_prefix(fx.root()).map(Path::to_path_buf);
    let short_in_root = match short_rel {
        Ok(rel) => fx.root().join(rel),
        // The root itself has short components: use the short leaf under the long root.
        Err(_) => fx.root().join(short.file_name().unwrap()),
    };
    let e = Executor::new()
        .delete(&item, &target(&fx, &short_in_root))
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    assert!(cand.join("a.txt").exists());
    check(&fx, &[&item], &[]);
}

// ------------------------------------------------------------------------------------------
// FX-WIN-10: reserved names, trailing dots and spaces
// ------------------------------------------------------------------------------------------

/// FX-WIN-10: entries named `NUL`, `CON.txt`, `aux`, `trailing.` and `space ` (created through
/// `\\?\` paths) inside a candidate, and a single-file item named `PRN` → removed by their
/// exact names, never mistaken for devices.
#[test]
fn fx_win_10_reserved_names_and_trailing_dots_are_removed() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/normal.txt", b"n")
        .unwrap()
        .checkpoint()
        .unwrap();
    // Created after the checkpoint (the snapshot reads by Win32 path, which would rewrite them).
    let cand = verbatim(&fx.root().join("cand"));
    for name in ["NUL", "CON.txt", "trailing.", "space "] {
        fs::write(cand.join(name), name.as_bytes()).unwrap();
    }
    fs::create_dir(cand.join("aux")).unwrap();
    fs::write(cand.join("aux").join("COM1"), b"c").unwrap();
    fs::write(
        verbatim(&fx.root().join("PRN")),
        b"single reserved-name item",
    )
    .unwrap();

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.files, 6, "{:#?}", r.report);
    assert_eq!(r.report.removed.dirs, 2, "{:#?}", r.report);
    assert!(fs::symlink_metadata(&cand).is_err());

    let prn = s0_delete(&fx, "PRN");
    let r = delete(&fx, &prn, "PRN").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.files, 1);
    assert!(fs::symlink_metadata(verbatim(&fx.root().join("PRN"))).is_err());
    check(&fx, &[&item, &prn], &["cand"]);
}

// ------------------------------------------------------------------------------------------
// FX-WIN-11 / FX-FS-07 / FX-FS-08: read-only and permission-denied entries
// ------------------------------------------------------------------------------------------

/// FX-WIN-11 / FX-FS-07: a file and a folder with the read-only attribute inside a candidate →
/// kept and reported (`ReadOnlyAttribute`), their attributes unchanged, their ancestors kept;
/// everything else is removed. The attribute is never cleared or ignored.
#[test]
fn fx_win_11_read_only_attribute_entries_are_kept_and_reported() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.file("cand/ro-file.txt", b"read-only file").unwrap();
    fx.file("cand/ro-dir/inside.txt", b"in a read-only dir")
        .unwrap();
    let ro_file = fx.root().join("cand/ro-file.txt");
    let ro_dir = fx.root().join("cand/ro-dir");
    for p in [&ro_file, &ro_dir] {
        set_readonly(p, true);
    }
    fx.checkpoint().unwrap();

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    println!("report: {:#?}", r.report);
    assert_eq!(
        r.report.outcome,
        RemoveOutcome::Incomplete,
        "{:#?}",
        r.report
    );
    let mut kept: Vec<_> = r
        .report
        .skipped
        .iter()
        .map(|s| (s.relative.clone(), s.reason))
        .collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        kept,
        vec![
            (PathBuf::from("ro-dir"), SkipReason::ReadOnlyAttribute),
            (PathBuf::from("ro-file.txt"), SkipReason::ReadOnlyAttribute),
        ],
        "{:#?}",
        r.report
    );
    for p in [&ro_file, &ro_dir] {
        assert!(
            fs::symlink_metadata(p).unwrap().permissions().readonly(),
            "the walker never clears the attribute: {}",
            p.display()
        );
    }
    assert!(
        ro_dir.join("inside.txt").exists(),
        "a read-only folder is kept whole"
    );
    assert!(!fx.root().join("cand/sub").exists());
    assert!(!fx.root().join("cand/a.txt").exists());
    check(&fx, &[&item], &["cand"]);
    // The test (not the walker) clears the attribute so the temp dir can be cleaned up.
    for p in [&ro_file, &ro_dir] {
        set_readonly(p, false);
    }
}

/// A single-file item with the read-only attribute → kept and reported, not refused (the walk
/// ran: `Incomplete`); the attribute is unchanged.
#[test]
fn fx_win_11_read_only_single_file_item_is_kept() {
    let mut fx = Fixture::new().unwrap();
    fx.file("ro.bin", b"read-only").unwrap();
    let p = fx.root().join("ro.bin");
    set_readonly(&p, true);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "ro.bin");
    let r = delete(&fx, &item, "ro.bin").unwrap();
    assert_eq!(
        r.report.outcome,
        RemoveOutcome::Incomplete,
        "{:#?}",
        r.report
    );
    assert_eq!(r.report.skipped[0].reason, SkipReason::ReadOnlyAttribute);
    assert!(fs::metadata(&p).unwrap().permissions().readonly());
    check(&fx, &[&item], &[]);
    set_readonly(&p, false);
}

/// Sets or clears the read-only attribute (test setup and cleanup only; the walker never
/// changes attributes).
#[allow(clippy::permissions_set_readonly_false)]
fn set_readonly(p: &Path, readonly: bool) {
    let mut perms = fs::metadata(p).unwrap().permissions();
    perms.set_readonly(readonly);
    fs::set_permissions(p, perms).unwrap();
}

fn icacls(args: &[&std::ffi::OsStr]) -> bool {
    Command::new("icacls")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// FX-FS-08: a sub-folder whose ACL denies everyone (`icacls /deny *S-1-1-0:F`) inside a
/// candidate → reported `PermissionDenied` with the reason and kept untouched; no escalation
/// (the ACL is never changed by the walker); the rest of the candidate is removed.
#[test]
fn fx_fs_08_permission_denied_subfolder_reported_not_escalated() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.file("cand/denied/secret.txt", b"unreadable").unwrap();
    fx.checkpoint().unwrap();
    let denied = fx.root().join("cand/denied");
    if !icacls(&[
        denied.as_os_str(),
        "/deny".as_ref(),
        "*S-1-1-0:(F)".as_ref(),
    ]) {
        eprintln!("SKIP FX-FS-08: icacls could not set a deny ACE");
        check(&fx, &[], &[]);
        return;
    }

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand");
    // The test (not the walker) restores access first, so the temp dir can be cleaned up.
    assert!(icacls(&[
        denied.as_os_str(),
        "/remove:d".as_ref(),
        "*S-1-1-0".as_ref()
    ]));
    let r = r.unwrap();
    println!("report: {:#?}", r.report);
    assert_eq!(
        r.report.outcome,
        RemoveOutcome::Incomplete,
        "{:#?}",
        r.report
    );
    assert_eq!(r.report.skipped.len(), 1, "{:#?}", r.report);
    let kept = &r.report.skipped[0];
    assert_eq!(kept.relative, Path::new("denied"));
    assert_eq!(kept.reason, SkipReason::PermissionDenied);
    assert!(!kept.detail.is_empty());
    assert!(denied.join("secret.txt").exists());
    assert!(!fx.root().join("cand/sub").exists());
    check(&fx, &[&item], &["cand"]);
}

/// FX-FS-05: a file hardlinked between the candidate and the canary → the candidate's link is
/// removed; the canary file keeps its content and identity.
#[test]
fn fx_fs_05_hardlink_to_canary_link_removed_canary_intact() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    let canary_file = fx.canary().symlink_target_file.clone();
    fx.hardlink("cand/sub/hl.txt", &canary_file).unwrap();
    fx.checkpoint().unwrap();
    let canary_id = identity(&canary_file);

    let item = s0_delete(&fx, "cand");
    let r = delete(&fx, &item, "cand").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.files, 6);
    assert_eq!(identity(&canary_file), canary_id);
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// A single-file item with other hard links is refused: removing one name frees nothing.
#[test]
fn hardlinked_single_file_item_is_refused() {
    let mut fx = Fixture::new().unwrap();
    let canary_file = fx.canary().symlink_target_file.clone();
    fx.hardlink("linked.txt", &canary_file).unwrap();
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "linked.txt");
    let e = delete(&fx, &item, "linked.txt").unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{e:?}");
    assert!(fx.root().join("linked.txt").exists());
    assert_canary_files(&fx);
    check(&fx, &[&item], &[]);
}

/// A single-file item is deleted; its bytes are reported.
#[test]
fn single_file_item_is_deleted() {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("big.bin", 100_000)
        .unwrap()
        .checkpoint()
        .unwrap();
    let item = s0_delete(&fx, "big.bin");
    let r = delete(&fx, &item, "big.bin").unwrap();
    assert_complete(&r);
    assert_eq!(r.report.removed.files, 1);
    assert_eq!(r.report.removed.logical_bytes, 100_000);
    check(&fx, &[&item], &["big.bin"]);
}

// ------------------------------------------------------------------------------------------
// Refusals: nothing removed
// ------------------------------------------------------------------------------------------

/// FX-RACE-01: the candidate is replaced by a junction to the canary between plan and execute
/// → refused (identity mismatch), nothing removed, canary intact.
#[test]
fn fx_race_01_candidate_replaced_by_junction_is_refused() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    fs::rename(fx.root().join("cand"), fx.root().join("cand.real")).unwrap();
    mklink_junction(&fx.root().join("cand"), &fx.canary().junction_target);

    let e = delete(&fx, &item, "cand").unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    assert!(is_link(&fx.root().join("cand")));
    assert!(fx.root().join("cand.real/sub/inner/d.txt").exists());
    assert_junction_target(&fx);
    check(&fx, &[&item], &["cand", "cand.real"]);
}

/// SAFE-05: the candidate directory is replaced by another directory (new file ID) → refused.
#[test]
fn identity_mismatch_at_the_root_is_refused_and_nothing_removed() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    fs::rename(fx.root().join("cand"), fx.root().join("cand.old")).unwrap();
    fs::create_dir(fx.root().join("cand")).unwrap();
    fs::write(fx.root().join("cand/new.txt"), b"not planned").unwrap();

    let e = delete(&fx, &item, "cand").unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    assert!(e.message.contains("different object"), "{e}");
    assert!(fx.root().join("cand/new.txt").exists());
    assert!(fx.root().join("cand.old/a.txt").exists());
    check(&fx, &[&item], &["cand", "cand.old"]);
}

/// A single-file item replaced by another file between its check and its deletion → the
/// deletion handle is not the planned object: refused, the replacement is kept.
#[test]
fn single_file_replaced_before_delete_is_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("big.bin", b"planned")
        .unwrap()
        .file("other.bin", b"not planned")
        .unwrap()
        .checkpoint()
        .unwrap();
    let item = s0_delete(&fx, "big.bin");
    let root = fx.root().to_path_buf();
    let e = delete_observed(&fx, &item, "big.bin", &mut |ev| {
        if let WalkEvent::EntryChecked { relative, .. } = ev
            && relative.as_os_str().is_empty()
        {
            fs::rename(root.join("other.bin"), root.join("big.bin")).unwrap();
        }
    })
    .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PlanInvalidated, "{e:?}");
    assert_eq!(fs::read(root.join("big.bin")).unwrap(), b"not planned");
    check(&fx, &[&item], &["big.bin", "other.bin"]);
}

// ------------------------------------------------------------------------------------------
// FX-RACE-02: swaps during the walk (deterministic hooks + a real race)
// ------------------------------------------------------------------------------------------

/// FX-RACE-02 (a): `cand/sub` is swapped for a junction to the canary after the candidate is
/// opened and before it is listed → the listing sees the junction, which is removed as a link;
/// the moved directory (still inside the candidate) is removed; the canary is intact.
#[test]
fn fx_race_02_subfolder_swapped_for_junction_before_listing_only_link_removed() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let target = fx.canary().junction_target.clone();
    let mut swapped = false;
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirOpened { depth: 0, .. } = ev {
            fs::rename(root.join("cand/sub"), root.join("cand/sub.moved")).unwrap();
            mklink_junction(&root.join("cand/sub"), &target);
            swapped = true;
        }
    })
    .unwrap();
    assert!(swapped);
    assert_complete(&r);
    assert_eq!(r.report.removed.symlinks, 1);
    assert_junction_target(&fx);
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (b): `cand/sub` (listed as a directory) is swapped for a junction to the canary
/// after the listing → the no-follow open returns the junction itself, its kind differs from
/// the listing, the walk aborts (`KindChanged`) and touches nothing more; canary intact.
#[test]
fn fx_race_02_subfolder_swapped_for_junction_after_listing_aborts() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let target = fx.canary().junction_target.clone();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirListed { depth: 0, .. } = ev {
            fs::rename(root.join("cand/sub"), root.join("cand/sub.moved")).unwrap();
            mklink_junction(&root.join("cand/sub"), &target);
        }
    })
    .unwrap();
    println!(
        "report: {:?}, aborted at {:?}",
        r.report.outcome, r.report.aborted
    );
    print_tree(&fx, "FX-RACE-02 (b) after the swap and the aborted walk");
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    let aborted = r.report.aborted.as_ref().unwrap();
    assert_eq!(aborted.relative, Path::new("sub"));
    assert!(
        matches!(aborted.anomaly, Anomaly::KindChanged { .. }),
        "{aborted:?}"
    );
    assert!(is_link(&root.join("cand/sub")));
    assert!(root.join("cand/sub.moved/inner/d.txt").exists());
    assert_junction_target(&fx);
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (c): `cand/sub` is replaced by a *different real directory* after the listing →
/// the opened handle's file ID is not the listed one, the walk aborts (`IdentityChanged`).
#[test]
fn fx_race_02_subfolder_replaced_by_other_directory_aborts_on_identity() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirListed { depth: 0, .. } = ev {
            fs::rename(root.join("cand/sub"), root.join("cand/sub.moved")).unwrap();
            fs::create_dir(root.join("cand/sub")).unwrap();
            fs::write(root.join("cand/sub/planted.txt"), b"not examined").unwrap();
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    let aborted = r.report.aborted.as_ref().unwrap();
    assert!(
        matches!(aborted.anomaly, Anomaly::IdentityChanged { .. }),
        "{aborted:?}"
    );
    assert!(
        root.join("cand/sub/planted.txt").exists(),
        "nothing touched after the anomaly"
    );
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (d): a nested directory (`cand/sub/inner`) is swapped for a junction to the
/// canary after `cand/sub` is listed, deep in the walk → abort; canary intact.
#[test]
fn fx_race_02_nested_folder_swapped_mid_walk_aborts() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let canary = fx.canary().dir.clone();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirListed { relative, .. } = ev
            && *relative == Path::new("sub")
        {
            fs::rename(root.join("cand/sub/inner"), root.join("cand/inner.moved")).unwrap();
            mklink_junction(&root.join("cand/sub/inner"), &canary);
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    assert!(matches!(
        r.report.aborted.as_ref().unwrap().anomaly,
        Anomaly::KindChanged { .. }
    ));
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (e): an entry listed as a file is replaced by a directory before it is opened →
/// the no-follow, non-directory open fails on the directory, the walk aborts (`KindChanged`).
#[test]
fn fx_race_02_entry_kind_changed_since_listing_aborts() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirListed { depth: 0, .. } = ev {
            fs::remove_file(root.join("cand/a.txt")).unwrap();
            fs::create_dir(root.join("cand/a.txt")).unwrap();
            fs::write(root.join("cand/a.txt/planted"), b"p").unwrap();
        }
    })
    .unwrap();
    assert_eq!(r.report.outcome, RemoveOutcome::Aborted, "{:#?}", r.report);
    assert!(matches!(
        r.report.aborted.as_ref().unwrap().anomaly,
        Anomaly::KindChanged { .. }
    ));
    assert!(root.join("cand/a.txt/planted").exists());
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (f): directories the walker is inside cannot be moved out of the candidate (they
/// are held without `FILE_SHARE_DELETE`): the attempted moves fail, the walk completes, and
/// nothing outside the candidate is ever deleted.
#[test]
fn fx_race_02_held_directory_cannot_be_moved_out_during_walk() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let item = s0_delete(&fx, "cand");
    let root = fx.root().to_path_buf();
    let outside = fx.base().join("moved-out");
    let mut moved = Vec::new();
    let r = delete_observed(&fx, &item, "cand", &mut |ev| {
        if let WalkEvent::DirOpened { relative, .. } = ev
            && *relative == Path::new("sub/inner")
        {
            for held in ["cand/sub/inner", "cand/sub"] {
                moved.push(fs::rename(root.join(held), &outside).is_ok());
            }
        }
    })
    .unwrap();
    assert_eq!(moved, vec![false, false], "a held directory was moved");
    assert_complete(&r);
    assert!(!outside.exists());
    assert_canary_files(&fx);
    check(&fx, &[&item], &["cand"]);
}

/// FX-RACE-02 (real race): a thread keeps swapping `cand/a/b` between its real directory and a
/// directory symlink to the canary while the Executor deletes the candidate, over many rounds.
/// Whatever the interleaving (link removed, kind or identity changed, move refused while
/// held), the canary is never touched. Needs the symlink privilege (junctions are created by a
/// child process, too slow to race).
#[test]
fn fx_race_02_concurrent_symlink_swaps_never_reach_the_canary() {
    const ROUNDS: usize = 100;
    let mut fx = Fixture::new().unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let canary = fx.canary().symlink_target_dir.clone();
    let probe = root.join("probe-link");
    if let Err(e) = std::os::windows::fs::symlink_dir(&canary, &probe) {
        eprintln!("SKIP FX-RACE-02 real race: symlinks are not permitted here ({e})");
        check(&fx, &[], &[]);
        return;
    }
    fs::remove_dir(&probe).unwrap();
    let cand = root.join("cand");
    let mut tally = std::collections::BTreeMap::<String, usize>::new();
    let mut executed = Vec::new();

    for round in 0..ROUNDS {
        for d in ["a/b/x", "a/b/y", "a/c"] {
            fs::create_dir_all(cand.join(d)).unwrap();
        }
        for i in 0..8 {
            fs::write(cand.join(format!("a/b/x/f{i}")), b"x").unwrap();
            fs::write(cand.join(format!("a/b/f{i}")), b"b").unwrap();
            fs::write(cand.join(format!("a/c/f{i}")), b"c").unwrap();
        }
        let item = s0_delete(&fx, "cand");

        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let (stop, b, real, canary) = (
                stop.clone(),
                cand.join("a/b"),
                cand.join("a/b.real"),
                canary.clone(),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if fs::rename(&b, &real).is_ok() {
                        if std::os::windows::fs::symlink_dir(&canary, &b).is_ok() {
                            std::thread::yield_now();
                            // Remove our link (never its target) and put the directory back.
                            if is_link(&b) {
                                let _ = fs::remove_dir(&b);
                            }
                        }
                        let _ = fs::rename(&real, &b);
                    }
                    std::thread::yield_now();
                }
            })
        };
        let r = delete(&fx, &item, "cand");
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        let label = match &r {
            Ok(r) => match &r.report.aborted {
                Some(a) => format!("Aborted ({})", a.anomaly),
                None => format!("{:?}", r.report.outcome),
            },
            Err(e) => format!("Refused ({:?})", e.kind),
        };
        *tally.entry(label).or_default() += 1;
        executed.push(item);

        assert_canary_files(&fx);

        // Clean up what the race left (the swapper is stopped): the same walker, no races.
        if fs::symlink_metadata(&cand).is_ok() {
            if fs::symlink_metadata(cand.join("a/b.real")).is_ok() && is_link(&cand.join("a/b")) {
                fs::remove_dir(cand.join("a/b")).unwrap();
            }
            let again = s0_delete(&fx, "cand");
            let r = delete(&fx, &again, "cand").unwrap();
            assert_complete(&r);
        }
        assert!(fs::symlink_metadata(&cand).is_err(), "round {round}");
    }
    println!("FX-RACE-02 real race, {ROUNDS} rounds: {tally:?}");
    let executed: Vec<&PlanItem> = executed.iter().collect();
    check(&fx, &executed, &["cand"]);
}

/// Deleting through the Executor with an explicit `DeleteTarget` whose anchor is given in
/// `\\?\` form works like the plain form (the anchor is opened through its verbatim path).
#[test]
fn verbatim_anchor_path_is_accepted() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/x.txt", b"x").unwrap().checkpoint().unwrap();
    let anchor = verbatim(fx.root());
    let path = anchor.join("cand");
    let item = plan_item(&path, scull_core::Tier::S0, scull_core::Action::Delete);
    let r = Executor::new()
        .delete(
            &item,
            &DeleteTarget {
                anchor: &anchor,
                anchor_identity: Some(identity(fx.root())),
                path: &path,
            },
        )
        .unwrap();
    assert_complete(&r);
    assert!(!fx.root().join("cand").exists());
    check(&fx, &[&item], &["cand"]);
}
