//! Cross-platform tests for the native [`Platform`] (P1-PLAT-01). Everything runs inside a
//! fresh temp directory.

use scull_platform::{EntryKind, ErrorKind, Native, Platform};
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

type TestResult = Result<(), Box<dyn Error>>;

fn write_file(path: &Path, bytes: &[u8]) -> TestResult {
    let mut f = File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

#[test]
fn regular_file_metadata() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("data.bin");
    write_file(&path, &[7u8; 5000])?;

    let md = Native.metadata(&path)?;
    assert_eq!(md.kind, EntryKind::File);
    assert_eq!(md.logical_size, 5000);
    assert!(!md.readonly);
    assert!(md.mtime_unix.is_some());
    Ok(())
}

#[test]
fn directory_metadata_and_listing() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    fs::create_dir(root.join("sub"))?;
    write_file(&root.join("a.txt"), b"hello")?;

    assert_eq!(Native.metadata(root)?.kind, EntryKind::Dir);

    let mut entries = Native
        .read_dir(root)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|e| {
            assert_eq!(e.path, root.join(&e.file_name));
            (e.file_name.to_string_lossy().into_owned(), e.kind)
        })
        .collect::<Vec<_>>();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            ("a.txt".to_string(), EntryKind::File),
            ("sub".to_string(), EntryKind::Dir),
        ]
    );
    Ok(())
}

#[test]
fn read_dir_refuses_a_file() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("f");
    write_file(&path, b"x")?;

    let err = Native
        .read_dir(&path)
        .err()
        .ok_or("read_dir on a file must fail")?;
    assert!(matches!(
        err.kind(),
        ErrorKind::NotADirectory {
            found: EntryKind::File
        }
    ));
    assert_eq!(err.path(), path);
    Ok(())
}

#[test]
fn missing_path_is_not_found() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let missing = tmp.path().join("nope");

    let err = Native.metadata(&missing).err().ok_or("expected error")?;
    assert!(err.is_not_found(), "{err}");
    assert!(err.to_string().contains("nope"), "{err}");

    let err = Native.read_dir(&missing).err().ok_or("expected error")?;
    assert!(err.is_not_found(), "{err}");
    Ok(())
}

#[test]
fn mtime_is_unix_seconds() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("old");
    let f = File::create(&path)?;
    f.set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_000))?;
    drop(f);

    assert_eq!(Native.metadata(&path)?.mtime_unix, Some(1_600_000_000));
    Ok(())
}

#[test]
fn readonly_flag() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("ro");
    write_file(&path, b"x")?;

    let mut perms = fs::metadata(&path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(&path, perms.clone())?;
    let readonly = Native.metadata(&path)?.readonly;

    // Restore so the temp dir can be removed on every OS.
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&path, perms)?;

    assert!(readonly);
    assert!(!Native.metadata(&path)?.readonly);
    Ok(())
}

#[test]
fn capabilities_match_metadata() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("f");
    write_file(&path, b"x")?;

    let caps = Native.capabilities();
    let md = Native.metadata(&path)?;
    assert_eq!(caps.os, std::env::consts::OS);
    assert!(caps.no_follow_metadata);
    assert_eq!(caps.file_identity, md.identity.is_some());
    assert_eq!(caps.allocated_size, md.allocated_size.is_some());
    assert_eq!(caps.link_count, md.nlink.is_some());
    assert_eq!(caps.mtime, md.mtime_unix.is_some());

    let json = serde_json::to_value(caps)?;
    assert_eq!(json["file_identity"], caps.file_identity);
    assert_eq!(json["os"], std::env::consts::OS);
    Ok(())
}

#[cfg(unix)]
#[test]
fn unix_capabilities_are_complete() {
    let caps = Native.capabilities();
    assert!(caps.file_identity && caps.allocated_size && caps.link_count);
    assert!(caps.special_file_kinds);
    assert!(!caps.reparse_points);
}

#[cfg(windows)]
#[test]
fn windows_capabilities_do_not_overclaim() {
    // Identity, allocation and link count land in #32; until then they must read as missing.
    let caps = Native.capabilities();
    assert!(!caps.file_identity && !caps.allocated_size && !caps.link_count);
    assert!(caps.reparse_points);
}

#[test]
fn entry_kind_json_names() -> TestResult {
    for kind in [
        EntryKind::File,
        EntryKind::Dir,
        EntryKind::Symlink,
        EntryKind::ReparsePoint,
        EntryKind::Fifo,
        EntryKind::Socket,
        EntryKind::BlockDevice,
        EntryKind::CharDevice,
        EntryKind::Other,
    ] {
        assert_eq!(serde_json::to_value(kind)?, kind.as_str());
        assert_eq!(kind.to_string(), kind.as_str());
        assert!(!(kind.is_link() && kind.is_special()));
    }
    assert!(EntryKind::Fifo.is_special());
    assert!(EntryKind::ReparsePoint.is_link());
    assert!(!EntryKind::File.is_special() && !EntryKind::Dir.is_link());
    Ok(())
}

#[cfg(unix)]
#[test]
fn native_path_bytes_are_raw_on_unix() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let raw = b"/tmp/\xff\xfe-not-utf8";
    let p = Path::new(OsStr::from_bytes(raw));
    assert_eq!(&*scull_platform::native_path_bytes(p), raw);
}

#[cfg(windows)]
#[test]
fn native_path_bytes_are_utf16le_on_windows() {
    let bytes = scull_platform::native_path_bytes(Path::new("a\u{e9}"));
    assert_eq!(&*bytes, &[0x61, 0x00, 0xe9, 0x00]);
}

/// Junctions need no privilege to create, unlike Windows symlinks. A junction must never be
/// listed through (SAFE-04).
#[cfg(windows)]
#[test]
fn windows_junction_is_not_followed() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let target = tmp.path().join("target");
    fs::create_dir(&target)?;
    write_file(&target.join("inside"), b"x")?;
    let link = tmp.path().join("junction");
    let status = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(&target)
        .stdout(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        eprintln!("skipping: mklink /J failed");
        return Ok(());
    }

    let kind = Native.metadata(&link)?.kind;
    assert!(kind.is_link(), "junction reported as {kind}");
    let err = Native
        .read_dir(&link)
        .err()
        .ok_or("junction must not be listed")?;
    assert!(matches!(err.kind(), ErrorKind::NotADirectory { .. }));

    let listed = Native
        .read_dir(tmp.path())?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find(|e| e.file_name == "junction")
        .ok_or("junction missing from listing")?;
    assert!(listed.kind.is_link(), "listed as {}", listed.kind);
    Ok(())
}
