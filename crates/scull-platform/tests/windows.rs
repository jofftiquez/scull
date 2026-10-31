//! Windows-only tests for the native [`Platform`] (P1-PLAT-02): file identity, link count,
//! allocated size of compressed and sparse files, junctions and symlinks that are never
//! followed, long paths, reserved names. Everything runs inside a fresh temp directory; the
//! only external tools are `cmd /C mklink`, `fsutil sparse` and `compact`, run on files in
//! that directory.
#![cfg(windows)]

use scull_platform::{EntryKind, ErrorKind, Native, Platform};
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

type TestResult = Result<(), Box<dyn Error>>;

/// `IO_REPARSE_TAG_MOUNT_POINT` (junctions and volume mount points).
const TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// `IO_REPARSE_TAG_SYMLINK`.
const TAG_SYMLINK: u32 = 0xA000_000C;
/// `FILE_ATTRIBUTE_SPARSE_FILE`.
const ATTR_SPARSE: u32 = 0x200;
/// `FILE_ATTRIBUTE_COMPRESSED`.
const ATTR_COMPRESSED: u32 = 0x800;

/// Skips a test whose OS feature is unavailable, except on CI (`CI` is set), where the
/// feature is expected and a silent skip would hide a regression. Set
/// `SCULL_TEST_ALLOW_SKIP=1` to allow the skip on a CI runner that lacks the feature.
fn skip(reason: &str) -> TestResult {
    if std::env::var_os("CI").is_some() && std::env::var_os("SCULL_TEST_ALLOW_SKIP").is_none() {
        return Err(
            format!("{reason} (required on CI; set SCULL_TEST_ALLOW_SKIP=1 to skip)").into(),
        );
    }
    eprintln!("skipping: {reason}");
    Ok(())
}

fn write_file(path: &Path, bytes: &[u8]) -> TestResult {
    let mut f = File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Runs a command quietly; `Ok(false)` if it ran and failed (the caller skips).
fn run(program: &str, args: &[&std::ffi::OsStr]) -> Result<bool, Box<dyn Error>> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        eprintln!(
            "{program} {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out.status.success())
}

fn mklink_junction(link: &Path, target: &Path) -> Result<bool, Box<dyn Error>> {
    run(
        "cmd",
        &[
            "/C".as_ref(),
            "mklink".as_ref(),
            "/J".as_ref(),
            link.as_os_str(),
            target.as_os_str(),
        ],
    )
}

fn listed(dir: &Path, name: &str) -> Result<scull_platform::DirEntry, Box<dyn Error>> {
    for entry in Native.read_dir(dir)? {
        let entry = entry?;
        if entry.file_name == name {
            return Ok(entry);
        }
    }
    Err(format!("{name} not listed in {}", dir.display()).into())
}

#[test]
fn identity_is_stable_and_distinct() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    write_file(&a, b"a")?;
    write_file(&b, b"b")?;

    let id_a = Native.metadata(&a)?.identity.ok_or("identity missing")?;
    assert_eq!(
        Native.metadata(&a)?.identity,
        Some(id_a),
        "identity must be stable"
    );
    assert_ne!(Native.metadata(&b)?.identity, Some(id_a));
    assert_ne!(id_a.file, 0);

    // Other spellings of the same path: forward slashes, `.` and `..` segments.
    let base = tmp.path().to_string_lossy().into_owned();
    for alias in [
        format!("{base}/a"),
        format!("{base}\\.\\a"),
        format!("{base}\\sub\\..\\a"),
    ] {
        assert_eq!(
            Native.metadata(Path::new(&alias))?.identity,
            Some(id_a),
            "{alias}"
        );
    }

    // A directory has an identity too, distinct from its children.
    let dir_id = Native
        .metadata(tmp.path())?
        .identity
        .ok_or("dir identity")?;
    assert_ne!(dir_id, id_a);
    assert_eq!(dir_id.volume, id_a.volume);
    Ok(())
}

/// PHY-03: hard links share one identity and report the link count.
#[test]
fn hardlink_shares_identity_and_counts_links() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let original = tmp.path().join("original");
    let link = tmp.path().join("link");
    write_file(&original, b"shared bytes")?;
    assert_eq!(Native.metadata(&original)?.nlink, Some(1));

    fs::hard_link(&original, &link)?;
    let m1 = Native.metadata(&original)?;
    let m2 = Native.metadata(&link)?;
    assert!(m1.identity.is_some());
    assert_eq!(m1.identity, m2.identity);
    assert_eq!(m1.nlink, Some(2));
    assert_eq!(m2.nlink, Some(2));
    assert_eq!(m1.kind, EntryKind::File);
    assert_eq!(m2.logical_size, 12);
    Ok(())
}

#[test]
fn allocated_size_of_regular_file() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("data.bin");
    // Incompressible-ish and large enough to live outside the MFT record.
    let bytes: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    write_file(&path, &bytes)?;

    let md = Native.metadata(&path)?;
    assert_eq!(md.logical_size, 200_000);
    let allocated = md.allocated_size.ok_or("allocated size missing")?;
    assert!(allocated >= 200_000, "allocated {allocated}");
    assert!(allocated < 200_000 + 1024 * 1024, "allocated {allocated}");
    Ok(())
}

/// PRD §9: a sparse file's allocated size is far below its logical size.
#[test]
fn sparse_file_allocated_size_is_small() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("sparse.bin");
    write_file(&path, b"head")?;
    if !run(
        "fsutil",
        &["sparse".as_ref(), "setflag".as_ref(), path.as_os_str()],
    )? {
        return skip("fsutil sparse setflag unavailable");
    }
    // Extending a sparse file leaves a hole instead of allocating clusters.
    let logical: u64 = 64 * 1024 * 1024;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(logical)?;

    let md = Native.metadata(&path)?;
    eprintln!("sparse: {md:?}");
    assert_eq!(md.logical_size, logical);
    let attrs = md.file_attributes.ok_or("attributes missing")?;
    if attrs & ATTR_SPARSE == 0 {
        return skip("sparse flag not set (filesystem without sparse support?)");
    }
    let allocated = md.allocated_size.ok_or("allocated size missing")?;
    assert!(
        allocated < 1024 * 1024,
        "sparse file allocated {allocated} of {logical}"
    );
    Ok(())
}

/// PRD §9: an NTFS-compressed file's allocated size is below its logical size.
#[test]
fn compressed_file_allocated_size_is_small() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("zeros.bin");
    let logical: u64 = 4 * 1024 * 1024;
    write_file(&path, &vec![0u8; logical as usize])?;
    if !run("compact", &["/C".as_ref(), "/Q".as_ref(), path.as_os_str()])? {
        return skip("compact /C unavailable");
    }

    let md = Native.metadata(&path)?;
    eprintln!("compressed: {md:?}");
    assert_eq!(md.logical_size, logical);
    let attrs = md.file_attributes.ok_or("attributes missing")?;
    if attrs & ATTR_COMPRESSED == 0 {
        return skip("compression not applied (filesystem without compression?)");
    }
    let allocated = md.allocated_size.ok_or("allocated size missing")?;
    assert!(
        allocated < logical / 2,
        "compressed file allocated {allocated} of {logical}"
    );
    Ok(())
}

/// FX-WIN-01 (platform part, T-10): a junction is reported as a junction with its tag, its
/// own identity, and is never listed through.
#[test]
fn fx_win_01_junction_detected_and_not_followed() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let target = tmp.path().join("canary");
    fs::create_dir(&target)?;
    write_file(&target.join("inside"), b"x")?;
    let link = tmp.path().join("junction");
    if !mklink_junction(&link, &target)? {
        return skip("mklink /J failed");
    }

    let md = Native.metadata(&link)?;
    assert_eq!(md.kind, EntryKind::Junction);
    assert!(md.kind.is_link());
    assert_eq!(md.reparse_tag, Some(TAG_MOUNT_POINT));
    // The junction's own identity, not its target's.
    let link_id = md.identity.ok_or("junction identity missing")?;
    assert_ne!(Some(link_id), Native.metadata(&target)?.identity);

    let err = Native
        .read_dir(&link)
        .err()
        .ok_or("junction must not be listed")?;
    assert!(
        matches!(
            err.kind(),
            ErrorKind::NotADirectory {
                found: EntryKind::Junction
            }
        ),
        "{err}"
    );

    let entry = listed(tmp.path(), "junction")?;
    assert_eq!(entry.kind, EntryKind::Junction);
    assert_eq!(entry.reparse_tag, Some(TAG_MOUNT_POINT));
    assert_eq!(entry.path, link);
    let canary = listed(tmp.path(), "canary")?;
    assert_eq!(canary.kind, EntryKind::Dir);
    assert_eq!(canary.reparse_tag, None);

    // A dangling junction is still a junction.
    fs::remove_file(target.join("inside"))?;
    fs::remove_dir(&target)?;
    assert_eq!(Native.metadata(&link)?.kind, EntryKind::Junction);
    Ok(())
}

/// FX-WIN-02 (platform part): file and directory symlinks are reported as symlinks and never
/// followed. Creating symlinks needs a privilege or Developer Mode; skipped without it.
#[test]
fn fx_win_02_symlinks_detected_and_not_followed() -> TestResult {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    let tmp = tempfile::tempdir()?;
    let target_dir = tmp.path().join("target_dir");
    let target_file = tmp.path().join("target_file");
    fs::create_dir(&target_dir)?;
    write_file(&target_dir.join("inside"), b"x")?;
    write_file(&target_file, &[1u8; 3000])?;
    let dir_link = tmp.path().join("dir_link");
    let file_link = tmp.path().join("file_link");
    if let Err(e) = symlink_dir(&target_dir, &dir_link) {
        return skip(&format!("cannot create symlinks here: {e}"));
    }
    symlink_file(&target_file, &file_link)?;

    for link in [&dir_link, &file_link] {
        let md = Native.metadata(link)?;
        assert_eq!(md.kind, EntryKind::Symlink, "{}", link.display());
        assert_eq!(md.reparse_tag, Some(TAG_SYMLINK));
        assert_ne!(
            md.logical_size, 3000,
            "size must be the link's, not the target's"
        );
    }
    let err = Native
        .read_dir(&dir_link)
        .err()
        .ok_or("dir symlink must not be listed")?;
    assert!(matches!(
        err.kind(),
        ErrorKind::NotADirectory {
            found: EntryKind::Symlink
        }
    ));
    assert_eq!(listed(tmp.path(), "dir_link")?.kind, EntryKind::Symlink);
    assert_eq!(listed(tmp.path(), "file_link")?.kind, EntryKind::Symlink);

    // Dangling symlink: still reported, target not needed.
    fs::remove_file(&target_file)?;
    assert_eq!(Native.metadata(&file_link)?.kind, EntryKind::Symlink);
    Ok(())
}

/// FX-WIN-04: paths longer than 260 characters work for metadata and listing.
#[test]
fn fx_win_04_long_paths() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let mut deep: PathBuf = tmp.path().to_path_buf();
    let segment = "a_directory_name_that_is_fairly_long_0123456789";
    while deep.as_os_str().len() < 320 {
        deep.push(segment);
    }
    fs::create_dir_all(&deep)?;
    let file = deep.join("leaf_file_with_a_long_name.txt");
    write_file(&file, b"deep")?;
    assert!(file.as_os_str().len() > 300);

    let md = Native.metadata(&file)?;
    assert_eq!(md.kind, EntryKind::File);
    assert_eq!(md.logical_size, 4);
    assert!(md.identity.is_some());
    assert_eq!(Native.metadata(&deep)?.kind, EntryKind::Dir);

    let entry = listed(&deep, "leaf_file_with_a_long_name.txt")?;
    assert_eq!(entry.kind, EntryKind::File);
    assert_eq!(entry.path, file);
    Ok(())
}

/// FX-WIN-10 (platform part): reserved device names and trailing dots are entries like any
/// other, never the `NUL` device.
#[test]
fn fx_win_10_reserved_names_are_files() -> TestResult {
    let tmp = tempfile::tempdir()?;
    // Create through verbatim paths, which Win32 does not rewrite.
    let mut verbatim = OsString::from(r"\\?\");
    verbatim.push(tmp.path().as_os_str());
    let verbatim = PathBuf::from(verbatim);
    let names = ["NUL", "con.txt", "trailing."];
    for name in names {
        write_file(&verbatim.join(name), b"12345")?;
    }

    for name in names {
        let md = Native.metadata(&tmp.path().join(name))?;
        assert_eq!(md.kind, EntryKind::File, "{name}");
        assert_eq!(md.logical_size, 5, "{name}");
        assert!(md.identity.is_some(), "{name}");
        assert_eq!(listed(tmp.path(), name)?.kind, EntryKind::File, "{name}");
    }

    for name in names {
        fs::remove_file(verbatim.join(name))?;
    }
    Ok(())
}

/// Attribute-only access takes no part in sharing checks: a file held open exclusively by
/// someone else is still inspected, and inspecting it locks nothing.
#[test]
fn exclusively_open_file_is_inspected() -> TestResult {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("locked");
    write_file(&path, b"locked")?;
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(&path)?;

    let md = Native.metadata(&path)?;
    assert_eq!(md.kind, EntryKind::File);
    assert_eq!(md.logical_size, 6);
    assert!(md.identity.is_some());
    drop(held);
    Ok(())
}

#[test]
fn read_dir_lists_with_kinds() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    fs::create_dir(root.join("sub"))?;
    write_file(&root.join("a.txt"), b"hello")?;
    // Enough entries to need more than one enumeration buffer.
    for i in 0..1500 {
        fs::write(
            root.join(format!("file_with_a_moderately_long_name_{i:05}.dat")),
            b"",
        )?;
    }

    let entries = Native.read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    assert_eq!(entries.len(), 1502);
    assert!(
        entries
            .iter()
            .all(|e| e.file_name != "." && e.file_name != "..")
    );
    assert_eq!(listed(root, "sub")?.kind, EntryKind::Dir);
    assert_eq!(listed(root, "a.txt")?.kind, EntryKind::File);
    let mut names: Vec<_> = entries.iter().map(|e| e.file_name.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 1502, "no duplicates");

    // Empty directory.
    let empty = root.join("sub");
    assert_eq!(Native.read_dir(&empty)?.count(), 0);
    Ok(())
}
