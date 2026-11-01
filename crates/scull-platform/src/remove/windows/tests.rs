//! Windows walker unit tests that need its private tuning or raw Win32 calls: simulated volume
//! crossings and volume mount points, the FAT listing fallback, a tiny depth limit, pinned
//! directories, 8.3 aliases, alternate-data-stream names, and the raw constants checked
//! against `windows-sys`. All in `scull-fixtures` trees with a canary, invariants checked.
//!
//! These tests run on Windows only (CI); the decision tables they rely on are also unit-tested
//! on every OS in `win_rules`.

use super::{Tuning, remove_tree};
use crate::remove::win_rules::{self, raw};
use crate::remove::{
    Anomaly, RemoveError, RemoveOutcome, RemoveReport, RemoveRequest, SkipReason, WalkEvent,
};
use crate::{Native, Platform};
use scull_core::FileIdentity;
use scull_fixtures::{Fixture, InvariantInputs};
use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

fn id(path: &Path) -> FileIdentity {
    Native.metadata(path).unwrap().identity.unwrap()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn request<'a>(fx: &'a Fixture, rel: &'a str, expected: FileIdentity) -> RemoveRequest<'a> {
    RemoveRequest {
        anchor: fx.root(),
        anchor_identity: Some(id(fx.root())),
        relative: Path::new(rel),
        expected,
        expected_kind: None,
    }
}

fn walk(
    fx: &Fixture,
    rel: &str,
    tuning: &Tuning,
    observer: Option<&mut dyn FnMut(&WalkEvent<'_>)>,
) -> RemoveReport {
    let expected = id(&fx.root().join(rel));
    remove_tree(&request(fx, rel, expected), observer, tuning).unwrap()
}

fn check(fx: &Fixture, allowed: &[&str]) {
    let mut inputs = InvariantInputs::new();
    for a in allowed {
        inputs = inputs.allow(*a);
    }
    let report = fx.check_invariants(&inputs);
    println!("{report}");
    report.assert_ok();
}

fn junction_target_intact(fx: &Fixture) {
    assert_eq!(
        fs::read(fx.canary().junction_target.join("keep.txt")).unwrap(),
        b"junction target content\n"
    );
}

/// The raw values the decision tables use are the SDK's.
#[test]
fn raw_constants_match_windows_sys() {
    use std::mem::offset_of;
    use windows_sys::Win32::Foundation as F;
    use windows_sys::Win32::Storage::FileSystem as FS;
    use windows_sys::Win32::System::SystemServices as SS;

    assert_eq!(raw::FILE_ATTRIBUTE_READONLY, FS::FILE_ATTRIBUTE_READONLY);
    assert_eq!(raw::FILE_ATTRIBUTE_DIRECTORY, FS::FILE_ATTRIBUTE_DIRECTORY);
    assert_eq!(
        raw::FILE_ATTRIBUTE_REPARSE_POINT,
        FS::FILE_ATTRIBUTE_REPARSE_POINT
    );
    assert_eq!(raw::FILE_ATTRIBUTE_OFFLINE, FS::FILE_ATTRIBUTE_OFFLINE);
    assert_eq!(
        raw::FILE_ATTRIBUTE_RECALL_ON_OPEN,
        FS::FILE_ATTRIBUTE_RECALL_ON_OPEN
    );
    assert_eq!(
        raw::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FS::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
    );
    assert_eq!(
        raw::IO_REPARSE_TAG_MOUNT_POINT,
        SS::IO_REPARSE_TAG_MOUNT_POINT
    );
    assert_eq!(raw::IO_REPARSE_TAG_CLOUD, SS::IO_REPARSE_TAG_CLOUD);
    assert_eq!(
        raw::IO_REPARSE_TAG_CLOUD | raw::IO_REPARSE_TAG_CLOUD_MASK,
        SS::IO_REPARSE_TAG_CLOUD_F
    );
    assert_eq!(raw::IO_REPARSE_TAG_ONEDRIVE, SS::IO_REPARSE_TAG_ONEDRIVE);
    assert_eq!(
        raw::MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
        FS::MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize
    );

    assert_eq!(raw::STATUS_INVALID_PARAMETER, F::STATUS_INVALID_PARAMETER);
    assert_eq!(raw::STATUS_ACCESS_DENIED, F::STATUS_ACCESS_DENIED);
    assert_eq!(
        raw::STATUS_OBJECT_NAME_INVALID,
        F::STATUS_OBJECT_NAME_INVALID
    );
    assert_eq!(
        raw::STATUS_OBJECT_NAME_NOT_FOUND,
        F::STATUS_OBJECT_NAME_NOT_FOUND
    );
    assert_eq!(
        raw::STATUS_OBJECT_PATH_NOT_FOUND,
        F::STATUS_OBJECT_PATH_NOT_FOUND
    );
    assert_eq!(raw::STATUS_SHARING_VIOLATION, F::STATUS_SHARING_VIOLATION);
    assert_eq!(raw::STATUS_DELETE_PENDING, F::STATUS_DELETE_PENDING);
    assert_eq!(
        raw::STATUS_MEDIA_WRITE_PROTECTED,
        F::STATUS_MEDIA_WRITE_PROTECTED
    );
    assert_eq!(
        raw::STATUS_FILE_IS_A_DIRECTORY,
        F::STATUS_FILE_IS_A_DIRECTORY
    );
    assert_eq!(raw::STATUS_NOT_A_DIRECTORY, F::STATUS_NOT_A_DIRECTORY);
    assert_eq!(raw::STATUS_CANNOT_DELETE, F::STATUS_CANNOT_DELETE);
    assert_eq!(
        raw::STATUS_REPARSE_POINT_ENCOUNTERED,
        F::STATUS_REPARSE_POINT_ENCOUNTERED
    );
    assert_eq!(raw::STATUS_STOPPED_ON_SYMLINK, F::STATUS_STOPPED_ON_SYMLINK);

    assert_eq!(raw::ERROR_INVALID_FUNCTION, F::ERROR_INVALID_FUNCTION);
    assert_eq!(raw::ERROR_FILE_NOT_FOUND, F::ERROR_FILE_NOT_FOUND);
    assert_eq!(raw::ERROR_PATH_NOT_FOUND, F::ERROR_PATH_NOT_FOUND);
    assert_eq!(raw::ERROR_ACCESS_DENIED, F::ERROR_ACCESS_DENIED);
    assert_eq!(raw::ERROR_NO_MORE_FILES, F::ERROR_NO_MORE_FILES);
    assert_eq!(raw::ERROR_WRITE_PROTECT, F::ERROR_WRITE_PROTECT);
    assert_eq!(raw::ERROR_SHARING_VIOLATION, F::ERROR_SHARING_VIOLATION);
    assert_eq!(raw::ERROR_LOCK_VIOLATION, F::ERROR_LOCK_VIOLATION);
    assert_eq!(raw::ERROR_NOT_SUPPORTED, F::ERROR_NOT_SUPPORTED);
    assert_eq!(raw::ERROR_INVALID_PARAMETER, F::ERROR_INVALID_PARAMETER);
    assert_eq!(raw::ERROR_INVALID_LEVEL, F::ERROR_INVALID_LEVEL);
    assert_eq!(raw::ERROR_DIR_NOT_EMPTY, F::ERROR_DIR_NOT_EMPTY);
    assert_eq!(raw::ERROR_BUSY, F::ERROR_BUSY);
    assert_eq!(raw::ERROR_DELETE_PENDING, F::ERROR_DELETE_PENDING);
    assert_eq!(raw::ERROR_USER_MAPPED_FILE, F::ERROR_USER_MAPPED_FILE);
    assert_eq!(raw::ERROR_PRIVILEGE_NOT_HELD, F::ERROR_PRIVILEGE_NOT_HELD);

    // CTL_CODE(FILE_DEVICE_FILE_SYSTEM = 9, 42, METHOD_BUFFERED = 0, FILE_ANY_ACCESS = 0).
    assert_eq!(raw::FSCTL_GET_REPARSE_POINT, (9 << 16) | (42 << 2));

    use win_rules::extd;
    assert_eq!(
        extd::NEXT_ENTRY_OFFSET,
        offset_of!(FS::FILE_ID_EXTD_DIR_INFO, NextEntryOffset)
    );
    assert_eq!(
        extd::FILE_ATTRIBUTES,
        offset_of!(FS::FILE_ID_EXTD_DIR_INFO, FileAttributes)
    );
    assert_eq!(
        extd::FILE_NAME_LENGTH,
        offset_of!(FS::FILE_ID_EXTD_DIR_INFO, FileNameLength)
    );
    assert_eq!(
        extd::REPARSE_POINT_TAG,
        offset_of!(FS::FILE_ID_EXTD_DIR_INFO, ReparsePointTag)
    );
    assert_eq!(extd::FILE_ID, offset_of!(FS::FILE_ID_EXTD_DIR_INFO, FileId));
    assert_eq!(
        extd::FILE_NAME,
        offset_of!(FS::FILE_ID_EXTD_DIR_INFO, FileName)
    );
}

/// A small tree with a junction to the canary's junction target and a long-named entry.
fn small_tree(fx: &mut Fixture) {
    fx.file("cand/a.txt", b"a")
        .unwrap()
        .file("cand/sub/b.txt", b"b")
        .unwrap()
        .file("cand/sub/inner/c.txt", b"c")
        .unwrap()
        .file("keep/outside.txt", b"never touched")
        .unwrap();
}

/// The listing's file IDs (`FileIdExtdDirectoryInfo`) are the IDs `FILE_ID_INFO` reports on the
/// opened handles: a normal walk completes without a single `IdentityChanged`.
#[test]
fn walk_with_listing_file_ids_completes() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    let target = fx.canary().junction_target.clone();
    assert!(fx.junction("cand/sub/j", &target).unwrap().is_created());
    fx.checkpoint().unwrap();
    let r = walk(&fx, "cand", &Tuning::default(), None);
    assert_eq!(r.outcome, RemoveOutcome::Complete, "{r:#?}");
    assert_eq!(r.removed.files, 3);
    assert_eq!(r.removed.symlinks, 1, "the junction, as a link");
    assert_eq!(r.removed.dirs, 3);
    assert!(!r.mount_ids_checked);
    junction_target_intact(&fx);
    check(&fx, &["cand"]);
}

/// The FAT fallback (`FileFullDirectoryInfo`, no file IDs) walks the same tree to completion.
#[test]
fn full_directory_info_fallback_completes() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    let target = fx.canary().junction_target.clone();
    assert!(fx.junction("cand/j", &target).unwrap().is_created());
    fx.checkpoint().unwrap();
    let tuning = Tuning {
        force_full_listing: true,
        ..Tuning::default()
    };
    let r = walk(&fx, "cand", &tuning, None);
    assert_eq!(r.outcome, RemoveOutcome::Complete, "{r:#?}");
    assert_eq!(r.removed.symlinks, 1);
    junction_target_intact(&fx);
    check(&fx, &["cand"]);
}

/// A simulated foreign volume inside the candidate: the walk aborts at it (`MountCrossing`);
/// what it holds is untouched, nothing after it is touched.
#[test]
fn simulated_foreign_volume_is_never_crossed() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/mnt/on-other-volume.txt", b"other volume")
        .unwrap()
        .checkpoint()
        .unwrap();
    let tuning = Tuning {
        foreign_volume_name: Some(wide("mnt")),
        ..Tuning::default()
    };
    let r = walk(&fx, "cand", &tuning, None);
    assert_eq!(r.outcome, RemoveOutcome::Aborted, "{r:#?}");
    let aborted = r.aborted.as_ref().unwrap();
    assert_eq!(aborted.relative, Path::new("mnt"));
    assert!(
        matches!(aborted.anomaly, Anomaly::MountCrossing { .. }),
        "{aborted:?}"
    );
    assert!(fx.root().join("cand/mnt/on-other-volume.txt").exists());
    check(&fx, &["cand"]);

    // The same entry as the target: refused before anything is removed.
    let e = remove_tree(
        &request(&fx, "cand/mnt", id(&fx.root().join("cand/mnt"))),
        None,
        &tuning,
    )
    .unwrap_err();
    assert!(matches!(e, RemoveError::TargetIsMountPoint { .. }), "{e:?}");
    assert!(fx.root().join("cand/mnt/on-other-volume.txt").exists());
}

/// A junction treated as a volume mount point inside the candidate: the walk aborts
/// (`VolumeMountPoint`) without removing it; as the target it is refused.
#[test]
fn volume_mount_point_is_never_removed_or_crossed() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/a.txt", b"a").unwrap();
    let target = fx.canary().junction_target.clone();
    assert!(fx.junction("cand/vol", &target).unwrap().is_created());
    fx.checkpoint().unwrap();
    let tuning = Tuning {
        volume_mount_name: Some(wide("vol")),
        ..Tuning::default()
    };
    let r = walk(&fx, "cand", &tuning, None);
    assert_eq!(r.outcome, RemoveOutcome::Aborted, "{r:#?}");
    assert_eq!(
        r.aborted.as_ref().unwrap().anomaly,
        Anomaly::VolumeMountPoint
    );
    assert!(fs::symlink_metadata(fx.root().join("cand/vol")).is_ok());
    junction_target_intact(&fx);

    let e = remove_tree(
        &request(&fx, "cand/vol", id(&fx.root().join("cand/vol"))),
        None,
        &tuning,
    )
    .unwrap_err();
    assert!(matches!(e, RemoveError::TargetIsMountPoint { .. }), "{e:?}");
    assert!(fs::symlink_metadata(fx.root().join("cand/vol")).is_ok());
    junction_target_intact(&fx);
    check(&fx, &["cand"]);
}

/// A real directory junction is read with FSCTL_GET_REPARSE_POINT and recognized as a plain
/// directory junction (not a volume mount point): removed as a link.
#[test]
fn real_junction_reparse_data_is_a_directory_junction() {
    let mut fx = Fixture::new().unwrap();
    let target = fx.canary().junction_target.clone();
    assert!(fx.junction("j", &target).unwrap().is_created());
    fx.checkpoint().unwrap();
    let r = walk(&fx, "j", &Tuning::default(), None);
    assert_eq!(r.outcome, RemoveOutcome::Complete, "{r:#?}");
    assert_eq!(r.removed.symlinks, 1);
    assert!(fs::symlink_metadata(fx.root().join("j")).is_err());
    junction_target_intact(&fx);
    check(&fx, &["j"]);
}

#[test]
fn tiny_depth_limit_reports_too_deep_and_keeps_ancestors() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/d1/d2/d3/deep.txt", b"x")
        .unwrap()
        .file("cand/top.txt", b"t")
        .unwrap()
        .checkpoint()
        .unwrap();
    let tuning = Tuning {
        max_depth: 2,
        ..Tuning::default()
    };
    let r = walk(&fx, "cand", &tuning, None);
    assert_eq!(r.outcome, RemoveOutcome::Incomplete, "{r:#?}");
    assert_eq!(r.skipped.len(), 1, "{r:#?}");
    assert_eq!(r.skipped[0].relative, Path::new("d1/d2/d3"));
    assert_eq!(r.skipped[0].reason, SkipReason::TooDeep);
    assert!(fx.root().join("cand/d1/d2/d3/deep.txt").exists());
    assert!(!fx.root().join("cand/top.txt").exists());
    check(&fx, &["cand"]);
}

#[test]
fn verify_every_entry_completes() {
    let mut fx = Fixture::new().unwrap();
    for i in 0..20 {
        fx.file(format!("cand/d{}/f{i}.txt", i % 3), b"x").unwrap();
    }
    fx.checkpoint().unwrap();
    let tuning = Tuning {
        verify_every: 1,
        ..Tuning::default()
    };
    let r = walk(&fx, "cand", &tuning, None);
    assert_eq!(r.outcome, RemoveOutcome::Complete, "{r:#?}");
    assert_eq!(r.removed.files, 20);
    check(&fx, &["cand"]);
}

/// Directories the walker holds are pinned: opened without `FILE_SHARE_DELETE`, so nobody can
/// rename them out of the candidate while the walk is inside (the Unix "held directory moved"
/// window is closed on Windows). The renames fail; the walk completes; nothing outside the
/// candidate changes.
#[test]
fn held_directories_cannot_be_moved_out() {
    let mut fx = Fixture::new().unwrap();
    small_tree(&mut fx);
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let outside = fx.base().join("moved-out");
    let mut attempts = Vec::new();
    let mut obs = |ev: &WalkEvent<'_>| {
        if let WalkEvent::DirListed { relative, .. } = ev
            && *relative == Path::new("sub/inner")
        {
            for held in ["cand/sub/inner", "cand/sub", "cand"] {
                attempts.push((held, fs::rename(root.join(held), &outside).is_ok()));
            }
        }
    };
    let r = walk(&fx, "cand", &Tuning::default(), Some(&mut obs));
    println!("rename attempts while held: {attempts:?}");
    assert_eq!(attempts.len(), 3);
    assert!(
        attempts.iter().all(|(_, moved)| !moved),
        "a held directory was moved: {attempts:?}"
    );
    assert_eq!(r.outcome, RemoveOutcome::Complete, "{r:#?}");
    assert!(!outside.exists());
    check(&fx, &["cand"]);
}

/// A name with `:` would open an alternate data stream of the entry, not the entry: refused.
#[test]
fn alternate_data_stream_names_are_refused() {
    let mut fx = Fixture::new().unwrap();
    fx.file("cand/a.txt", b"a").unwrap().checkpoint().unwrap();
    let expected = id(&fx.root().join("cand"));
    for rel in ["cand:stream", "cand::$DATA", "cand/a.txt:x"] {
        let e = remove_tree(&request(&fx, rel, expected), None, &Tuning::default()).unwrap_err();
        assert!(
            matches!(e, RemoveError::InvalidRequest { .. }),
            "{rel}: {e:?}"
        );
    }
    check(&fx, &[]);
}

/// `GetShortPathNameW`, or `None` when the volume has no 8.3 name for `path`.
fn short_path(path: &Path) -> Option<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut buf = vec![0u16; 4096];
    // SAFETY: `wide` is NUL-terminated; `buf` is writable for `buf.len()` UTF-16 units.
    let n =
        unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    buf.truncate(n);
    Some(PathBuf::from(OsString::from_wide(&buf)))
}

/// FX-WIN-05 (walker part, SAFE-13): an 8.3 short name resolves to an object whose identity is
/// then compared. The short name of the planned target removes exactly it; the short name of
/// another (protected) folder with the target's identity is refused, nothing removed.
#[test]
fn fx_win_05_short_names_resolve_by_identity() {
    let mut fx = Fixture::new().unwrap();
    fx.file("candidate with a long name/a.txt", b"a")
        .unwrap()
        .file("protected folder with a long name/keep.txt", b"keep")
        .unwrap()
        .checkpoint()
        .unwrap();
    let cand = fx.root().join("candidate with a long name");
    let protected = fx.root().join("protected folder with a long name");
    let (Some(cand_short), Some(prot_short)) = (short_path(&cand), short_path(&protected)) else {
        eprintln!("SKIP FX-WIN-05 walker part: GetShortPathNameW failed");
        return;
    };
    let cand_short_name = cand_short
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let prot_short_name = prot_short
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    if cand_short.file_name() == cand.file_name() {
        eprintln!("SKIP FX-WIN-05 walker part: 8.3 short names are disabled on this volume");
        return;
    }
    println!("{cand_short_name} / {prot_short_name}");
    let cand_id = id(&cand);

    // The alias of another object, with the target's identity: refused.
    let e = remove_tree(
        &request(&fx, &prot_short_name, cand_id),
        None,
        &Tuning::default(),
    )
    .unwrap_err();
    assert!(matches!(e, RemoveError::IdentityMismatch { .. }), "{e:?}");
    assert!(protected.join("keep.txt").exists());

    // The target's own alias: resolved to the planned object and removed.
    let r = remove_tree(
        &request(&fx, &cand_short_name, cand_id),
        None,
        &Tuning::default(),
    )
    .unwrap();
    assert_eq!(r.outcome, RemoveOutcome::Complete, "{r:#?}");
    assert!(!cand.exists());
    assert!(protected.join("keep.txt").exists());
    check(&fx, &["candidate with a long name"]);
}

/// Relative opens have no path-length limit: a candidate whose entries lie far beyond
/// `MAX_PATH` (and beyond 32 767 characters) is removed (FX-WIN-04, FX-FS-12).
#[test]
fn entries_beyond_any_path_limit_are_removed() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("cand").unwrap().checkpoint().unwrap();
    // 200 levels of 200-character names: ~40 000 characters, built with verbatim paths.
    let name = "n".repeat(200);
    let mut dir: Vec<u16> = r"\\?\".encode_utf16().collect();
    dir.extend(fx.root().join("cand").as_os_str().encode_wide());
    let mut depth = 0;
    for _ in 0..200 {
        let mut next = dir.clone();
        next.push(u16::from(b'\\'));
        next.extend(name.encode_utf16());
        if next.len() > 32_000 {
            break;
        }
        fs::create_dir(PathBuf::from(OsString::from_wide(&next))).unwrap();
        dir = next;
        depth += 1;
    }
    let mut leaf = dir.clone();
    leaf.extend(r"\leaf.txt".encode_utf16());
    fs::write(PathBuf::from(OsString::from_wide(&leaf)), b"leaf").unwrap();
    println!("depth {depth}, deepest path {} characters", leaf.len());
    assert!(leaf.len() > 30_000);

    let r = walk(&fx, "cand", &Tuning::default(), None);
    assert_eq!(r.outcome, RemoveOutcome::Complete, "{r:#?}");
    assert_eq!(r.removed.dirs as usize, depth + 1);
    assert_eq!(r.removed.files, 1);
    check(&fx, &["cand"]);
}
