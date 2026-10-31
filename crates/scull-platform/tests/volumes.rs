//! Volumes and mount boundaries on the real OS (P1-PLAT-03; T-12, FX-FS-04, FX-LNX-06).
//!
//! Read-only: nothing here creates anything outside a fresh temp directory, and no mount is
//! created (that needs privileges; the mount fixtures FX-FS-04 / FX-LNX-06 run nightly).
//! Crossing detection is shown with mounts every runner already has (`/` vs `/dev` on Unix,
//! two drives on Windows when present) and with the mount table itself.

use scull_platform::{
    Native, Platform, VolumeKind, free_space, is_mount_boundary, volume_of, volumes,
};
#[cfg(unix)]
use scull_platform::{ScanPolicy, mounts_within};
use std::error::Error;
use std::fs;
#[cfg(unix)]
use std::path::Path;

type TestResult = Result<(), Box<dyn Error>>;

/// The volume a temp file lives on is the temp dir's volume, and its ID is the file's
/// `FileIdentity::volume`.
#[test]
fn volume_of_temp_matches_file_identity() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let file = tmp.path().join("probe.txt");
    fs::write(&file, b"x")?;

    let dir_vol = volume_of(tmp.path())?;
    let file_vol = volume_of(&file)?;
    eprintln!("temp volume: {dir_vol:?}");
    assert_eq!(dir_vol.mount_point, file_vol.mount_point);
    assert_eq!(dir_vol.volume_id, file_vol.volume_id);

    let identity = Native.metadata(&file)?.identity.ok_or("no identity")?;
    assert_eq!(file_vol.volume_id, Some(identity.volume));
    assert!(!file_vol.fs_type.is_empty());

    // Same volume: no boundary between the temp dir and its file.
    assert!(!is_mount_boundary(
        &Native.metadata(tmp.path())?,
        &Native.metadata(&file)?
    ));
    Ok(())
}

#[test]
fn free_space_is_positive_and_matches_volume() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let free = free_space(tmp.path())?;
    assert!(free > 0, "free space on the temp volume is {free}");
    let vol = volume_of(tmp.path())?;
    if let Some(total) = vol.total_bytes {
        assert!(free <= total, "free {free} > total {total}");
    }
    Ok(())
}

#[test]
fn missing_path_is_not_found() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let missing = tmp.path().join("missing");
    let err = volume_of(&missing).err().ok_or("volume_of succeeded")?;
    assert!(err.is_not_found(), "{err}");
    assert!(free_space(&missing).is_err());
    Ok(())
}

/// The volume of a symlink is the volume holding the link, not its target's.
#[cfg(unix)]
#[test]
fn volume_of_symlink_is_the_links_volume() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let link = tmp.path().join("to-dev");
    std::os::unix::fs::symlink("/dev", &link)?;
    assert_eq!(
        volume_of(&link)?.mount_point,
        volume_of(tmp.path())?.mount_point
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn root_volume_is_listed() -> TestResult {
    let all = volumes()?;
    assert!(!all.is_empty());
    let root = all
        .iter()
        .rev()
        .find(|v| v.mount_point == Path::new("/"))
        .ok_or("no volume mounted at /")?;
    eprintln!("root volume: {root:?}");
    assert!(root.volume_id.is_some());
    assert_eq!(volume_of(Path::new("/"))?.mount_point, Path::new("/"));
    // Every listed volume has a mount point and a type.
    for v in &all {
        assert!(v.mount_point.is_absolute(), "{v:?}");
        assert!(!v.fs_type.is_empty(), "{v:?}");
    }
    Ok(())
}

/// Crossing detected (T-12): `/dev` is its own filesystem (devtmpfs/tmpfs on Linux, devfs on
/// macOS) inside `/`, so both signals see it: the device number changes, and the mount table
/// lists it within `/`.
#[cfg(unix)]
#[test]
fn crossing_into_dev_is_a_boundary() -> TestResult {
    let root = Native.metadata(Path::new("/"))?;
    let dev = Native.metadata(Path::new("/dev"))?;
    assert!(is_mount_boundary(&root, &dev));

    let all = volumes()?;
    let nested = mounts_within(Path::new("/"), &all);
    assert!(
        nested.iter().any(|v| v.mount_point == Path::new("/dev")),
        "/dev not found among mounts within /"
    );
    let dev_vol = volume_of(Path::new("/dev"))?;
    assert_eq!(dev_vol.mount_point, Path::new("/dev"));
    assert_eq!(dev_vol.kind, VolumeKind::Virtual);
    assert_eq!(dev_vol.kind.scan_policy(), ScanPolicy::OffByDefault);
    assert_eq!(dev_vol.volume_id, dev.identity.map(|i| i.volume));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn linux_proc_is_virtual_and_root_local() -> TestResult {
    let proc_vol = volume_of(Path::new("/proc/self"))?;
    assert_eq!(proc_vol.mount_point, Path::new("/proc"));
    assert_eq!(proc_vol.fs_type, "proc");
    assert_eq!(proc_vol.kind, VolumeKind::Virtual);
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn macos_root_is_apfs_with_clones_and_snapshots() -> TestResult {
    let root = volume_of(Path::new("/"))?;
    eprintln!("root volume: {root:?}");
    assert_eq!(root.fs_type, "apfs");
    assert_eq!(root.kind, VolumeKind::Local);
    assert_eq!(root.supports.clones, Some(true));
    assert_eq!(root.supports.snapshots, Some(true));
    // The sealed system volume is mounted read-only; the user's data is elsewhere.
    assert!(root.read_only);
    let home = std::env::temp_dir();
    let data = volume_of(&home)?;
    assert!(!data.read_only, "{data:?}");
    Ok(())
}

#[cfg(windows)]
fn system_drive_root() -> std::path::PathBuf {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| String::from("C:"));
    std::path::PathBuf::from(format!("{drive}\\"))
}

#[cfg(windows)]
#[test]
fn windows_system_drive_is_listed_as_local_ntfs() -> TestResult {
    let root = system_drive_root();
    let all = volumes()?;
    eprintln!("volumes: {all:#?}");
    let sys = all
        .iter()
        .find(|v| {
            v.mount_point
                .to_string_lossy()
                .eq_ignore_ascii_case(&root.to_string_lossy())
        })
        .ok_or("system drive not listed")?;
    assert_eq!(sys.kind, VolumeKind::Local);
    assert!(
        sys.fs_type.eq_ignore_ascii_case("NTFS") || sys.fs_type.eq_ignore_ascii_case("ReFS"),
        "{sys:?}"
    );
    assert!(sys.source.starts_with(r"\\?\Volume{"), "{sys:?}");
    assert!(sys.free_bytes.is_some_and(|f| f > 0));

    let identity = Native.metadata(&root)?.identity.ok_or("no identity")?;
    assert_eq!(sys.volume_id, Some(identity.volume));

    let of = volume_of(&root.join("Windows"))?;
    assert!(
        of.mount_point
            .to_string_lossy()
            .eq_ignore_ascii_case(&root.to_string_lossy()),
        "{of:?}"
    );
    assert_eq!(of.volume_id, sys.volume_id);
    Ok(())
}

/// Crossing detected on Windows (T-12): entries on two different local volumes have different
/// volume IDs. Hosted runners have C: and D:; skipped where only one volume exists.
#[cfg(windows)]
#[test]
fn windows_two_volumes_are_a_boundary() -> TestResult {
    let all = volumes()?;
    let mut local = all
        .iter()
        .filter(|v| v.kind == VolumeKind::Local && v.volume_id.is_some());
    let Some(first) = local.next() else {
        return Err("no local volume".into());
    };
    let Some(second) = local.find(|v| v.volume_id != first.volume_id) else {
        eprintln!("SKIP: only one local volume on this machine");
        return Ok(());
    };
    let a = Native.metadata(&first.mount_point)?;
    let b = Native.metadata(&second.mount_point)?;
    eprintln!(
        "{} vs {}",
        first.mount_point.display(),
        second.mount_point.display()
    );
    assert!(is_mount_boundary(&a, &b));
    assert_eq!(a.identity.map(|i| i.volume), first.volume_id);
    assert_eq!(b.identity.map(|i| i.volume), second.volume_id);
    Ok(())
}
