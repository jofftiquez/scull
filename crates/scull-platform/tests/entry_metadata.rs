//! `Platform::entry_metadata` (FX-FS-15 speed-up): for every kind of entry a listing returns,
//! it gives exactly what `Platform::metadata` gives for the entry's path. On Unix the entry is
//! stat'ed relative to the listed directory (`fstatat`), never following a link at the final
//! component. Temp dirs only; nothing outside them is read or changed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_platform::{Native, Platform};
use std::fs;
use std::path::Path;

fn listed_equals_lstat(dir: &Path) -> usize {
    let p = Native::new();
    let mut n = 0;
    for entry in p.read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let fast = p.entry_metadata(&entry).unwrap();
        let slow = p.metadata(&entry.path).unwrap();
        assert_eq!(fast, slow, "{}", entry.path.display());
        assert_eq!(fast.kind, entry.kind, "{}", entry.path.display());
        n += 1;
    }
    n
}

#[test]
fn entry_metadata_equals_metadata_for_files_dirs_and_hard_links() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    fs::write(d.join("file.txt"), b"hello").unwrap();
    fs::write(d.join("big.bin"), vec![7u8; 300_000]).unwrap();
    fs::create_dir(d.join("sub")).unwrap();
    fs::write(d.join("sub").join("inner"), b"x").unwrap();
    fs::hard_link(d.join("file.txt"), d.join("hardlink.txt")).unwrap();
    let ro = d.join("readonly.txt");
    fs::write(&ro, b"r").unwrap();
    let mut perms = fs::metadata(&ro).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&ro, perms).unwrap();
    assert_eq!(listed_equals_lstat(d), 5);
    assert_eq!(listed_equals_lstat(&d.join("sub")), 1);
    let link = Native::new()
        .read_dir(d)
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.file_name == "hardlink.txt")
        .unwrap();
    assert_eq!(Native::new().entry_metadata(&link).unwrap().nlink, Some(2));
}

/// Links are never followed: a symlink (to a directory, and a dangling one) reports the link
/// itself, like `lstat`; FIFOs are identified without being opened.
#[cfg(unix)]
#[test]
fn entry_metadata_never_follows_links_or_opens_special_files() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    fs::create_dir(d.join("target")).unwrap();
    std::os::unix::fs::symlink(d.join("target"), d.join("to-dir")).unwrap();
    std::os::unix::fs::symlink(d.join("missing"), d.join("dangling")).unwrap();
    let made_fifo = std::process::Command::new("mkfifo")
        .arg(d.join("fifo"))
        .status()
        .is_ok_and(|s| s.success());
    let n = listed_equals_lstat(d);
    assert_eq!(n, if made_fifo { 4 } else { 3 });
    use scull_platform::EntryKind;
    let p = Native::new();
    for e in p.read_dir(d).unwrap().map(Result::unwrap) {
        let md = p.entry_metadata(&e).unwrap();
        match e.file_name.to_str().unwrap() {
            "to-dir" | "dangling" => assert_eq!(md.kind, EntryKind::Symlink),
            "fifo" => assert_eq!(md.kind, EntryKind::Fifo),
            "target" => assert_eq!(md.kind, EntryKind::Dir),
            other => panic!("unexpected {other}"),
        }
    }
}

/// An entry kept after its listing is dropped still works (it falls back to its path), and an
/// entry removed after it was listed is "not found", as with `metadata`.
#[test]
fn entry_metadata_after_the_listing_or_the_entry_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    fs::write(d.join("a"), b"a").unwrap();
    fs::write(d.join("b"), b"b").unwrap();
    let p = Native::new();
    let entries: Vec<_> = p.read_dir(d).unwrap().map(Result::unwrap).collect();
    for e in &entries {
        assert_eq!(p.entry_metadata(e).unwrap(), p.metadata(&e.path).unwrap());
    }
    let mut it = p.read_dir(d).unwrap();
    let first = it.next().unwrap().unwrap();
    fs::remove_file(&first.path).unwrap();
    assert!(p.entry_metadata(&first).unwrap_err().is_not_found());
    drop(it);
}
