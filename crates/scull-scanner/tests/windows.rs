//! Windows-only special-file and loop tests (FX-FS-02, FX-FS-14, SAFE-04, SAFE-18): junction
//! loops (created with `cmd /C mklink /J`, no privilege needed) and AF_UNIX socket files
//! (reparse tag `IO_REPARSE_TAG_AF_UNIX`, created with PowerShell 7's .NET sockets where
//! available). Everything runs inside fresh temp directories.
#![cfg(windows)]

mod common;

use common::*;
use scull_fixtures::{Fixture, InvariantInputs, LinkOutcome};
use scull_platform::{EntryKind, Native, Platform};
use scull_scanner::{ScanOptions, ScanStatus};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn opts() -> ScanOptions {
    let mut o = ScanOptions::default();
    o.threads = 4;
    o.emit_listings = true;
    o
}

#[test]
fn fx_fs_02_junction_loops_are_never_descended() -> TestResult {
    let mut fx = Fixture::new()?;
    fx.file("a/f.txt", b"hello")?
        .file("a/b/deep.bin", vec![b'x'; 3000])?
        .dir("d")?;
    let root = fx.root().to_path_buf();
    let a = fx.path("a")?;
    let d = fx.path("d")?;
    let mut junctions = Vec::new();
    for (at, to) in [("a/b/up", &root), ("a/self", &a), ("a/b/to-d", &d)] {
        assert_eq!(fx.junction(at, to)?, LinkOutcome::Created, "mklink /J {at}");
        junctions.push(root.join(at));
    }
    fx.checkpoint()?;

    let rec = Arc::new(Recording::new(Native));
    let (p, r) = (Arc::clone(&rec), root.clone());
    let (out, ev) = within(Duration::from_secs(20), move || run_with(p, &r, &opts()))?;
    assert_eq!(out.status, ScanStatus::Completed);
    assert!(ev.issues().is_empty(), "{:?}", ev.issues());
    assert!(
        ev.boundaries().is_empty(),
        "junctions are links, not boundaries"
    );
    assert_eq!(rec.violations(), Vec::<String>::new());
    assert_eq!(out.totals.links, 3);
    assert_eq!(out.totals.files, 2);
    let reads: BTreeSet<PathBuf> = rec.reads().into_iter().collect();
    let dirs: BTreeSet<PathBuf> = [root.clone(), a.clone(), root.join("a/b"), d.clone()].into();
    assert_eq!(reads, dirs, "only real directories are listed");
    assert_eq!(rec.reads().len(), 4);
    for p in reported_paths(&ev) {
        assert!(
            !junctions.iter().any(|j| p.starts_with(j) && p != *j),
            "reported beneath a junction: {}",
            p.display()
        );
    }
    let listed: BTreeSet<PathBuf> = ev
        .listings()
        .iter()
        .flat_map(|l| {
            l.entries
                .iter()
                .filter(|e| e.kind == EntryKind::Junction)
                .map(|e| l.path.join(&e.name))
        })
        .collect();
    assert_eq!(listed, junctions.iter().cloned().collect());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
    Ok(())
}

/// Create an AF_UNIX socket file at `path` with PowerShell 7 (.NET `UnixDomainSocketEndPoint`).
/// `Err` with the reason if PowerShell 7 is not installed or the OS doesn't support AF_UNIX.
fn make_af_unix_socket(path: &Path) -> Result<(), String> {
    // Single-quoted PowerShell literal: `'` is escaped by doubling it.
    let lit = path.display().to_string().replace('\'', "''");
    // No `Dispose()`: .NET deletes the socket file it bound when the socket is disposed. The
    // process exits instead, which closes the socket and leaves the file behind.
    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         $s = [System.Net.Sockets.Socket]::new([System.Net.Sockets.AddressFamily]::Unix, \
         [System.Net.Sockets.SocketType]::Stream, [System.Net.Sockets.ProtocolType]::Unspecified); \
         $s.Bind([System.Net.Sockets.UnixDomainSocketEndPoint]::new('{lit}')); \
         [Environment]::Exit(0)"
    );
    let out = std::process::Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|e| format!("cannot run pwsh: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pwsh failed ({}): {} {}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    std::fs::symlink_metadata(path)
        .map(drop)
        .map_err(|e| format!("socket file missing after bind: {e}"))
}

#[test]
fn fx_fs_14_af_unix_socket_file_is_a_special_never_opened() -> TestResult {
    let (_t, root) = temp_root();
    mixed_tree(&root);
    let sock = root.join("a").join("b").join("agent.sock");
    if let Err(why) = make_af_unix_socket(&sock) {
        // GitHub's Windows runners ship PowerShell 7: a skip there would hide the OS case.
        assert!(
            std::env::var_os("CI").is_none(),
            "PowerShell 7 must create an AF_UNIX socket on CI: {why}"
        );
        eprintln!("skipped: {why}");
        return Ok(());
    }
    let md = Native.metadata(&sock)?;
    assert_eq!(md.kind, EntryKind::Socket, "{md:?}");
    assert_eq!(md.reparse_tag, Some(common::graph::IO_REPARSE_TAG_AF_UNIX));

    let rec = Arc::new(Recording::new(Native));
    let (p, r) = (Arc::clone(&rec), root.clone());
    let (out, ev) = within(Duration::from_secs(20), move || run_with(p, &r, &opts()))?;
    assert_eq!(out.status, ScanStatus::Completed);
    assert!(ev.issues().is_empty(), "{:?}", ev.issues());
    assert_eq!(rec.violations(), Vec::<String>::new());
    assert_eq!(out.totals.specials, 1);
    assert!(!rec.reads().contains(&sock));
    let entry = ev
        .listings()
        .into_iter()
        .find(|l| l.path == root.join("a/b"))
        .and_then(|l| l.entries.iter().find(|e| e.name == "agent.sock").cloned())
        .expect("socket listed");
    assert_eq!(entry.kind, EntryKind::Socket);
    let s = ev.summary_of(&root.join("a")).expect("a");
    assert_eq!(s.totals.specials, 1, "the containing item is flagged");
    Ok(())
}
