//! Unix-only tests for the native [`Platform`] (P1-PLAT-01): identity, allocated size, link
//! count, symlinks that are never followed, and special files that are never opened. Everything
//! runs inside a fresh temp directory. Also owner-only creation (P1-PLAT-06): modes read back
//! with `lstat`.
#![cfg(unix)]

use scull_platform::{
    EntryKind, ErrorKind, Exposure, Native, Operation, Platform, create_dir_owner_only,
    create_file_owner_only, ensure_owner_only, owner_only,
};
use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::symlink;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

type TestResult = Result<(), Box<dyn Error>>;

fn write_file(path: &Path, bytes: &[u8]) -> TestResult {
    let mut f = File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn listed_kind(dir: &Path, name: &str) -> Result<EntryKind, Box<dyn Error>> {
    for entry in Native.read_dir(dir)? {
        let entry = entry?;
        if entry.file_name == name {
            return Ok(entry.kind);
        }
    }
    Err(format!("{name} not listed").into())
}

/// Runs `f` on another thread and fails if it doesn't finish in time. Opening a FIFO for
/// reading blocks until a writer appears, so a hang here means SAFE-18 was violated.
fn must_not_block<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, Box<dyn Error>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(r) => Ok(r?),
        Err(_) => Err("platform call blocked: special file was opened (SAFE-18)".into()),
    }
}

#[test]
fn identity_is_dev_and_ino() -> TestResult {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::tempdir()?;
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    write_file(&a, b"a")?;
    write_file(&b, b"b")?;

    let std_md = fs::symlink_metadata(&a)?;
    let id_a = Native.metadata(&a)?.identity.ok_or("identity missing")?;
    assert_eq!(id_a.volume, std_md.dev());
    assert_eq!(id_a.file, u128::from(std_md.ino()));

    // Same object through another spelling of the path: same identity.
    let alias = tmp.path().join(".").join("a");
    assert_eq!(Native.metadata(&alias)?.identity, Some(id_a));
    // Different object: different identity.
    assert_ne!(Native.metadata(&b)?.identity, Some(id_a));
    Ok(())
}

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
    assert_eq!(m1.nlink, Some(2));
    assert_eq!(m2.nlink, Some(2));
    assert!(m1.identity.is_some());
    assert_eq!(m1.identity, m2.identity);
    assert_eq!(m2.kind, EntryKind::File);
    Ok(())
}

#[test]
fn allocated_size_of_written_file() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("data");
    // Non-repeating bytes so compressing filesystems can't shrink it to nothing.
    let bytes: Vec<u8> = (0u32..128 * 1024)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    write_file(&path, &bytes)?;

    let md = Native.metadata(&path)?;
    assert_eq!(md.logical_size, 128 * 1024);
    let allocated = md.allocated_size.ok_or("allocated size missing")?;
    assert!(allocated > 0, "a file with data must have blocks");
    assert_eq!(allocated % 512, 0, "st_blocks is in 512-byte units");
    Ok(())
}

#[test]
fn sparse_file_allocated_below_logical() -> TestResult {
    const LOGICAL: u64 = 64 * 1024 * 1024;
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("sparse");
    let f = File::create(&path)?;
    f.set_len(LOGICAL)?;
    f.sync_all()?;
    drop(f);

    let md = Native.metadata(&path)?;
    assert_eq!(md.logical_size, LOGICAL);
    let allocated = md.allocated_size.ok_or("allocated size missing")?;
    if allocated >= LOGICAL {
        // Filesystems without sparse-file support (e.g. FAT, some network mounts).
        eprintln!("skipping assertion: filesystem does not keep files sparse ({allocated} B)");
        return Ok(());
    }
    assert!(
        allocated < LOGICAL / 2,
        "allocated {allocated} for a hole-only file"
    );
    Ok(())
}

#[test]
fn symlink_to_file_is_not_followed() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let target = tmp.path().join("big");
    write_file(&target, &[1u8; 10_000])?;
    let link = tmp.path().join("link");
    symlink(&target, &link)?;

    let md = Native.metadata(&link)?;
    assert_eq!(md.kind, EntryKind::Symlink);
    // st_size of a symlink is the length of the stored target path, not the target's size.
    assert_eq!(md.logical_size, target.as_os_str().len() as u64);
    assert_ne!(md.identity, Native.metadata(&target)?.identity);
    assert_eq!(listed_kind(tmp.path(), "link")?, EntryKind::Symlink);
    Ok(())
}

#[test]
fn symlink_to_dir_is_never_listed_through() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside)?;
    write_file(&outside.join("secret"), b"x")?;
    let root = tmp.path().join("root");
    fs::create_dir(&root)?;
    let link = root.join("escape");
    symlink(&outside, &link)?;

    assert_eq!(Native.metadata(&link)?.kind, EntryKind::Symlink);
    assert_eq!(listed_kind(&root, "escape")?, EntryKind::Symlink);

    let err = Native
        .read_dir(&link)
        .err()
        .ok_or("read_dir must refuse a symlinked directory")?;
    assert!(matches!(
        err.kind(),
        ErrorKind::NotADirectory {
            found: EntryKind::Symlink
        }
    ));
    Ok(())
}

#[test]
fn dangling_and_looping_symlinks() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dangling = tmp.path().join("dangling");
    symlink(tmp.path().join("does-not-exist"), &dangling)?;
    let a = tmp.path().join("loop-a");
    let b = tmp.path().join("loop-b");
    symlink(&b, &a)?;
    symlink(&a, &b)?;

    for p in [&dangling, &a, &b] {
        assert_eq!(
            Native.metadata(p)?.kind,
            EntryKind::Symlink,
            "{}",
            p.display()
        );
    }
    let err = Native.read_dir(&a).err().ok_or("loop must not be listed")?;
    assert!(matches!(err.kind(), ErrorKind::NotADirectory { .. }));
    Ok(())
}

#[test]
fn fifo_is_detected_without_opening() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let fifo = tmp.path().join("pipe");
    let status = std::process::Command::new("mkfifo").arg(&fifo).status()?;
    assert!(status.success(), "mkfifo failed");

    let dir: PathBuf = tmp.path().to_path_buf();
    let (md_kind, listed) = must_not_block(move || {
        let md = Native
            .metadata(&dir.join("pipe"))
            .map_err(|e| e.to_string())?;
        let listed = listed_kind(&dir, "pipe").map_err(|e| e.to_string())?;
        Ok((md.kind, listed))
    })?;
    assert_eq!(md_kind, EntryKind::Fifo);
    assert_eq!(listed, EntryKind::Fifo);
    assert!(md_kind.is_special());

    let err = Native
        .read_dir(&fifo)
        .err()
        .ok_or("fifo is not a directory")?;
    assert!(matches!(
        err.kind(),
        ErrorKind::NotADirectory {
            found: EntryKind::Fifo
        }
    ));
    Ok(())
}

#[test]
fn socket_is_detected_without_opening() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("s");
    let _listener = match std::os::unix::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("skipping: cannot bind unix socket in temp dir: {e}");
            return Ok(());
        }
    };

    let dir = tmp.path().to_path_buf();
    let (md_kind, listed) = must_not_block(move || {
        let md = Native.metadata(&dir.join("s")).map_err(|e| e.to_string())?;
        let listed = listed_kind(&dir, "s").map_err(|e| e.to_string())?;
        Ok((md.kind, listed))
    })?;
    assert_eq!(md_kind, EntryKind::Socket);
    assert_eq!(listed, EntryKind::Socket);
    Ok(())
}

#[test]
fn char_device_is_detected() -> TestResult {
    // lstat only; /dev/null is never opened.
    let md = must_not_block(|| {
        Native
            .metadata(Path::new("/dev/null"))
            .map_err(|e| e.to_string())
    })?;
    assert_eq!(md.kind, EntryKind::CharDevice);
    assert!(md.kind.is_special());
    Ok(())
}

#[test]
fn unreadable_directory_is_permission_denied() -> TestResult {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir()?;
    let locked = tmp.path().join("locked");
    fs::create_dir(&locked)?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;

    let result = Native.read_dir(&locked).map(|it| it.count());
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755))?;

    match result {
        // root (e.g. some CI containers) bypasses permission bits.
        Ok(_) => eprintln!("skipping assertion: running with permission override"),
        Err(e) => assert!(e.is_permission_denied(), "{e}"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Owner-only creation (P1-PLAT-06, DATA-04, SAFE-16, FX-DATA-03)
// ---------------------------------------------------------------------------------------------

fn mode(path: &Path) -> Result<u32, Box<dyn Error>> {
    Ok(fs::symlink_metadata(path)?.mode() & 0o7777)
}

fn exposures(path: &Path) -> Vec<Exposure> {
    match ensure_owner_only(path) {
        Ok(()) => Vec::new(),
        Err(e) => match e.kind() {
            ErrorKind::NotOwnerOnly { exposures } => exposures.clone(),
            _ => panic!("unexpected error: {e}"),
        },
    }
}

#[test]
fn owner_only_dirs_are_0700_and_existing_ancestors_untouched() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let existing = tmp.path().join("existing");
    fs::create_dir(&existing)?;
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o755))?;

    let leaf = existing.join("a").join("b").join("scull");
    create_dir_owner_only(&leaf)?;

    assert_eq!(
        mode(&existing)?,
        0o755,
        "an existing ancestor must never be chmodded"
    );
    for created in [existing.join("a"), existing.join("a/b"), leaf.clone()] {
        assert_eq!(mode(&created)?, 0o700, "{}", created.display());
        ensure_owner_only(&created)?;
    }
    assert_eq!(
        fs::symlink_metadata(&leaf)?.uid(),
        fs::symlink_metadata(tmp.path())?.uid()
    );
    Ok(())
}

#[test]
fn existing_dir_is_left_as_is_and_reported() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let dir = tmp.path().join("shared");
    fs::create_dir(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
    create_dir_owner_only(&dir)?; // exists: not modified
    assert_eq!(mode(&dir)?, 0o755);
    assert_eq!(exposures(&dir), vec![Exposure::Mode { mode: 0o755 }]);
    Ok(())
}

#[test]
fn owner_only_files_are_0600_at_creation() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("scull.db");
    drop(create_file_owner_only(&path)?);
    assert_eq!(mode(&path)?, 0o600);
    ensure_owner_only(&path)?;
    Ok(())
}

#[test]
fn broader_modes_are_reported() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("plain");
    write_file(&path, b"x")?;
    for (m, broad) in [
        (0o644, true),
        (0o640, true),
        (0o604, true),
        (0o600, false),
        (0o400, false),
    ] {
        fs::set_permissions(&path, fs::Permissions::from_mode(m))?;
        let found = exposures(&path);
        if broad {
            assert_eq!(found, vec![Exposure::Mode { mode: m }], "{m:o}");
            let err = ensure_owner_only(&path).expect_err("broad");
            assert_eq!(err.operation(), Operation::CheckPermissions);
            assert!(err.to_string().contains("not owner-only"), "{err}");
        } else {
            assert!(found.is_empty(), "{m:o}: {found:?}");
        }
    }
    Ok(())
}

#[test]
fn symlinks_are_never_followed_by_owner_only_helpers() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let target_dir = tmp.path().join("target-dir");
    fs::create_dir(&target_dir)?;
    let dir_link = tmp.path().join("dir-link");
    symlink(&target_dir, &dir_link)?;

    // A symlink where the directory should be is refused, not followed.
    let err = create_dir_owner_only(&dir_link).expect_err("link");
    assert!(
        matches!(
            err.kind(),
            ErrorKind::NotADirectory {
                found: EntryKind::Symlink
            }
        ),
        "{err}"
    );
    assert_eq!(
        exposures(&dir_link),
        vec![Exposure::Link {
            found: EntryKind::Symlink
        }]
    );

    // A dangling symlink where the file should be: refused, and its target is not created.
    let target_file = tmp.path().join("target-file");
    let file_link = tmp.path().join("file-link");
    symlink(&target_file, &file_link)?;
    let err = create_file_owner_only(&file_link).expect_err("link");
    assert!(
        matches!(err.kind(), ErrorKind::Io(e) if e.kind() == std::io::ErrorKind::AlreadyExists),
        "{err}"
    );
    assert!(
        fs::symlink_metadata(&target_file).is_err(),
        "link target must not be created"
    );
    Ok(())
}

#[test]
fn restrict_clears_group_other_and_special_bits() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let file = tmp.path().join("f");
    write_file(&file, b"x")?;
    for (before, after) in [
        (0o644, 0o600),
        (0o4755, 0o700),
        (0o600, 0o600),
        (0o640, 0o600),
    ] {
        fs::set_permissions(&file, fs::Permissions::from_mode(before))?;
        owner_only::restrict(&file)?;
        assert_eq!(mode(&file)?, after, "{before:o}");
    }
    let dir = tmp.path().join("d");
    fs::create_dir(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o1777))?;
    owner_only::restrict(&dir)?;
    assert_eq!(mode(&dir)?, 0o700);
    Ok(())
}

/// An entry that is already owner-only is left alone without being opened: closing a
/// descriptor would drop this process's POSIX locks on it (SQLite's `-shm` locks, which
/// `scull-storage` restricts while its connection is open). Shown with a write-only file,
/// which can't be opened for reading: restricting it succeeds and changes nothing.
#[test]
fn restrict_does_not_open_an_owner_only_entry() -> TestResult {
    if scull_platform::is_elevated().blocks_destructive() {
        eprintln!("skipped: root can open any file");
        return Ok(());
    }
    let tmp = tempfile::tempdir()?;
    let file = tmp.path().join("f");
    write_file(&file, b"x")?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o200))?;
    assert!(fs::File::open(&file).is_err(), "unreadable");
    owner_only::restrict(&file)?;
    assert_eq!(mode(&file)?, 0o200);
    // Not owner-only: opened (and refused, as before, without read permission).
    fs::set_permissions(&file, fs::Permissions::from_mode(0o220))?;
    assert!(owner_only::restrict(&file).is_err());
    assert_eq!(mode(&file)?, 0o220);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[test]
fn restrict_never_follows_links_or_opens_special_files() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let target = tmp.path().join("target");
    write_file(&target, b"x")?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?;
    let link = tmp.path().join("link");
    symlink(&target, &link)?;
    let err = owner_only::restrict(&link).expect_err("symlink");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    assert_eq!(mode(&target)?, 0o644, "the link target must be untouched");

    let fifo = tmp.path().join("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()?
        .success();
    assert!(made, "mkfifo failed");
    let path = fifo.clone();
    let err = must_not_block(move || {
        owner_only::restrict(&path)
            .map_err(|e| e.to_string())
            .err()
            .ok_or_else(|| "restricted a FIFO".to_owned())
    })?;
    assert!(err.contains("fifo"), "{err}");
    Ok(())
}
