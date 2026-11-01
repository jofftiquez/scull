//! Emergency headroom (PRO-06) end to end, in temp directories only: create a real reserve,
//! release only the file the manifest describes, refuse anything else, audit the outcome.
//! Reserves here are a few MiB (explicit small limits); nothing outside the temp dirs is
//! created, read or removed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_cleaner::headroom::{
    CreateOptions, HeadroomEvent, Limits, ReleaseOutcome, audit, create_with, release,
};
use scull_core::ErrorKind;
use scull_platform::Platform as _;
use scull_proactive::headroom::{
    HEADROOM_DIR, HeadroomState, MANIFEST_FILE, reserve_relative, status,
};
use scull_storage::{DefaultPermissions, EventKind, Store};
use std::path::Path;

const MIB: u64 = 1024 * 1024;
const LIMITS: Limits = Limits {
    min: MIB,
    max: 64 * MIB,
};

fn opts(size: u64) -> CreateOptions {
    CreateOptions {
        size_bytes: size,
        keep_free_bytes: 0,
        now: 1_800_000_000,
    }
}

fn data_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let d = t.path().join("data");
    std::fs::create_dir(&d).unwrap();
    (t, d)
}

fn reserve(d: &Path) -> std::path::PathBuf {
    d.join(reserve_relative())
}

#[test]
fn create_writes_a_real_owner_only_reserve_and_release_frees_it() {
    let (_t, d) = data_dir();
    assert_eq!(status(&d).state, HeadroomState::None);
    let m = create_with(&d, &opts(4 * MIB), LIMITS).unwrap();
    assert_eq!(m.size_bytes, 4 * MIB);
    assert!(
        m.allocated_bytes >= 4 * MIB * 98 / 100,
        "{}",
        m.allocated_bytes
    );
    assert_eq!(std::fs::metadata(reserve(&d)).unwrap().len(), 4 * MIB);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&reserve(&d)), 0o600);
        assert_eq!(mode(&d.join(HEADROOM_DIR)), 0o700);
        assert_eq!(mode(&d.join(MANIFEST_FILE)), 0o600);
    }
    let s = status(&d);
    assert_eq!(s.state, HeadroomState::Ready);
    assert_eq!(s.manifest.as_ref(), Some(&m));
    assert!(s.releasable_bytes.unwrap() >= 4 * MIB * 98 / 100);

    // One reserve at a time.
    let again = create_with(&d, &opts(4 * MIB), LIMITS).unwrap_err();
    assert_eq!(again.kind, ErrorKind::SafetyBlocked);
    assert!(
        again.message.contains("already exists"),
        "{}",
        again.message
    );

    let r = release(&d).unwrap();
    assert_eq!(r.outcome, ReleaseOutcome::Released);
    assert!(r.freed_bytes >= 4 * MIB * 98 / 100);
    assert_eq!(r.manifest, m);
    assert!(!reserve(&d).exists());
    assert!(!d.join(MANIFEST_FILE).exists());
    assert_eq!(status(&d).state, HeadroomState::None);
    // Nothing left to release.
    assert_eq!(release(&d).unwrap_err().kind, ErrorKind::NotFound);
}

#[test]
fn a_replaced_file_is_never_released() {
    // phase2 run 41321040626: on CI the replacement got the deleted file's inode number back
    // (ext4 reuses them at once), so identity, kind and size all matched and the old check said
    // "ready". The replacement here is made to pass that old check whenever the filesystem
    // allows it (same size, created right after the removal to invite inode reuse); the marker
    // and modification time still tell it apart.
    let (_t, d) = data_dir();
    let m = create_with(&d, &opts(2 * MIB), LIMITS).unwrap();
    std::fs::remove_file(reserve(&d)).unwrap();
    std::fs::write(reserve(&d), vec![7u8; (2 * MIB) as usize]).unwrap();
    let reused = scull_platform::Native::new()
        .metadata(&reserve(&d))
        .unwrap()
        .identity
        == Some(m.identity);
    println!("inode reused by the filesystem: {reused}");
    let st = status(&d);
    assert_eq!(st.state, HeadroomState::Changed, "{st:?}");
    let e = release(&d).unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked);
    assert!(
        e.message.contains("identity")
            || e.message.contains("marker")
            || e.message.contains("modified"),
        "{}",
        e.message
    );
    assert!(reserve(&d).exists(), "the unknown file is left alone");
    assert!(d.join(MANIFEST_FILE).exists());
}

/// The same file (same inode, size and modification time) whose content was overwritten: only
/// the marker tells it apart, and it does.
#[test]
fn a_rewritten_file_with_the_same_identity_is_never_released() {
    use std::io::{Seek, SeekFrom, Write};
    let (_t, d) = data_dir();
    let m = create_with(&d, &opts(2 * MIB), LIMITS).unwrap();
    let mtime = std::fs::metadata(reserve(&d)).unwrap().modified().unwrap();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(reserve(&d))
        .unwrap();
    f.seek(SeekFrom::Start(0)).unwrap();
    f.write_all(&[0u8; 32]).unwrap();
    f.set_modified(mtime).unwrap();
    drop(f);
    let md = scull_platform::Native::new()
        .metadata(&reserve(&d))
        .unwrap();
    assert_eq!(md.identity, Some(m.identity), "same object");
    assert_eq!(md.logical_size, 2 * MIB, "same size");
    assert_eq!(md.mtime_unix, m.mtime_unix, "same modification time");
    let st = status(&d);
    assert_eq!(st.state, HeadroomState::Changed, "{st:?}");
    assert!(st.detail.unwrap().contains("marker differs"));
    let e = release(&d).unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked);
    assert!(e.message.contains("marker differs"), "{}", e.message);
    assert!(reserve(&d).exists());
}

/// The marker is random, recorded and at the start of the file.
#[test]
fn the_marker_is_random_and_recorded() {
    let (_t, a) = data_dir();
    let (_u, b) = data_dir();
    let ma = create_with(&a, &opts(MIB), LIMITS).unwrap();
    let mb = create_with(&b, &opts(MIB), LIMITS).unwrap();
    assert_eq!(ma.marker.len(), 64);
    assert_ne!(ma.marker, mb.marker, "a fresh marker per reserve");
    let head = std::fs::read(reserve(&a)).unwrap();
    assert_eq!(scull_proactive::headroom::hex(&head[..32]), ma.marker);
    assert_eq!(status(&a).state, HeadroomState::Ready);
}

#[test]
fn a_grown_or_shrunk_file_is_never_released() {
    let (_t, d) = data_dir();
    create_with(&d, &opts(2 * MIB), LIMITS).unwrap();
    // `write`, not `append`: a Windows append-only handle may not change the length.
    std::fs::OpenOptions::new()
        .write(true)
        .open(reserve(&d))
        .unwrap()
        .set_len(3 * MIB)
        .unwrap();
    let e = release(&d).unwrap_err();
    assert!(e.message.contains("size changed"), "{}", e.message);
    assert!(reserve(&d).exists());
}

#[cfg(unix)]
#[test]
fn a_link_at_the_reserve_path_is_never_followed() {
    let (t, d) = data_dir();
    create_with(&d, &opts(2 * MIB), LIMITS).unwrap();
    let victim = t.path().join("victim.txt");
    std::fs::write(&victim, b"keep me").unwrap();
    std::fs::remove_file(reserve(&d)).unwrap();
    std::os::unix::fs::symlink(&victim, reserve(&d)).unwrap();
    let e = release(&d).unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked);
    assert!(e.message.contains("symlink"), "{}", e.message);
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
    assert!(
        std::fs::symlink_metadata(reserve(&d))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn a_linked_headroom_folder_is_refused_at_creation() {
    let (t, d) = data_dir();
    let elsewhere = t.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, d.join(HEADROOM_DIR)).unwrap();
    let e = create_with(&d, &opts(2 * MIB), LIMITS).unwrap_err();
    assert_eq!(e.kind, ErrorKind::SafetyBlocked);
    assert!(
        std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
        "nothing written there"
    );
}

#[test]
fn a_missing_file_only_clears_the_record() {
    let (_t, d) = data_dir();
    create_with(&d, &opts(2 * MIB), LIMITS).unwrap();
    std::fs::remove_file(reserve(&d)).unwrap();
    assert_eq!(status(&d).state, HeadroomState::Missing);
    let r = release(&d).unwrap();
    assert_eq!((r.outcome, r.freed_bytes), (ReleaseOutcome::AlreadyGone, 0));
    assert!(!d.join(MANIFEST_FILE).exists());
}

#[test]
fn an_unrecorded_file_is_left_alone() {
    let (_t, d) = data_dir();
    std::fs::create_dir(d.join(HEADROOM_DIR)).unwrap();
    std::fs::write(reserve(&d), b"not scull's").unwrap();
    assert_eq!(status(&d).state, HeadroomState::Unrecorded);
    assert_eq!(release(&d).unwrap_err().kind, ErrorKind::NotFound);
    let e = create_with(&d, &opts(2 * MIB), LIMITS).unwrap_err();
    assert!(e.message.contains("no record"), "{}", e.message);
    assert_eq!(std::fs::read(reserve(&d)).unwrap(), b"not scull's");
}

#[test]
fn creation_refuses_below_the_reserve_and_outside_the_limits() {
    let (_t, d) = data_dir();
    let free = scull_platform::free_space(&d).unwrap();
    let below = create_with(
        &d,
        &CreateOptions {
            keep_free_bytes: free + 1024 * MIB,
            ..opts(2 * MIB)
        },
        LIMITS,
    )
    .unwrap_err();
    assert_eq!(below.kind, ErrorKind::SafetyBlocked);
    assert!(below.message.contains("already below"), "{}", below.message);
    for size in [MIB / 2, 65 * MIB] {
        let e = create_with(&d, &opts(size), LIMITS).unwrap_err();
        assert_eq!(e.kind, ErrorKind::SafetyBlocked, "{size}");
    }
    assert!(!reserve(&d).exists());
    assert!(!d.join(MANIFEST_FILE).exists());
}

#[test]
fn outcomes_are_audited_as_headroom_events() {
    let (t, d) = data_dir();
    let m = create_with(&d, &opts(2 * MIB), LIMITS).unwrap();
    let r = release(&d).unwrap();
    let mut store = Store::open(&t.path().join("scull.db"), &DefaultPermissions).unwrap();
    for (action, bytes) in [("created", m.allocated_bytes), ("released", r.freed_bytes)] {
        audit(
            &mut store,
            &HeadroomEvent {
                action: action.into(),
                path: reserve(&d).display().to_string(),
                manifest: Some(m.clone()),
                bytes,
                reason: None,
            },
            "cli",
            1_800_000_000,
        )
        .unwrap();
    }
    let events = store.events(EventKind::Headroom, 0).unwrap();
    let actions: Vec<&str> = events
        .iter()
        .map(|e| e.payload["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["created", "released"]);
    assert_eq!(events[1].payload["manifest"]["size_bytes"], 2 * MIB);
}
