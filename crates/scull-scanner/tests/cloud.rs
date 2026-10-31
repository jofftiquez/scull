//! Cloud-sync placeholders are never hydrated (P1-SCAN-05, PLT-04, T-52, PRD §8.3).
//!
//! Listing a placeholder directory (macOS dataless iCloud / File Provider folder, Windows
//! recall-on-open or offline directory) makes the sync provider download it. These tests use
//! the in-memory graph platform, which reports the same metadata bits an OS would
//! (`SF_DATALESS`, `FILE_ATTRIBUTE_RECALL_*` / `OFFLINE`) and records any listing of a
//! placeholder, or any request beneath one, as a violation. macOS gives tests no way to make
//! a dataless directory, so the graph stands in for it; the real Windows `attrib +O` case is
//! in `windows.rs`.

mod common;

use common::graph::{
    Cloud, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_PINNED,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN, Graph, Node, SF_DATALESS,
};
use common::*;
use scull_core::{Confidence, ErrorKind, ScullError};
use scull_platform::EntryKind;
use scull_scanner::{BoundaryReason, ScanError, ScanOptions, ScanStatus};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const HANG: Duration = Duration::from_secs(20);
/// Size the graph reports for every directory node (logical and allocated).
const DIR: u64 = 4096;

fn opts() -> ScanOptions {
    let mut o = ScanOptions::default();
    o.threads = 4;
    o.emit_listings = true;
    o
}

fn scan_graph_with(g: Graph, o: ScanOptions) -> (Arc<Graph>, scull_scanner::ScanOutcome, Events) {
    let g = Arc::new(g);
    let (p, root) = (Arc::clone(&g), g.root().to_path_buf());
    let (out, ev) = within(HANG, move || run_with(p, &root, &o)).expect("scan starts");
    (g, out, ev)
}

fn dataless() -> Cloud {
    Cloud {
        file_attributes: None,
        bsd_flags: Some(SF_DATALESS),
    }
}

fn windows(attributes: u32) -> Cloud {
    Cloud {
        file_attributes: Some(attributes),
        bsd_flags: None,
    }
}

/// A placeholder directory with contents the provider would have to download to list.
fn cloud_dir(g: &mut Graph, parent: usize, name: &str, cloud: Cloud) -> usize {
    let d = g.dir(parent, name);
    g.add(d, "remote-1.bin", Node::File(1 << 20));
    g.add(d, "remote-2.bin", Node::File(1 << 20));
    let sub = g.dir(d, "nested");
    g.add(sub, "deep.bin", Node::File(1 << 20));
    g.set_cloud(d, cloud);
    d
}

/// Invariants of every scan here: nothing listed or reached beneath a placeholder, totals
/// of the root summary match the outcome, nothing reported beneath a boundary.
fn assert_no_hydration(g: &Graph, out: &scull_scanner::ScanOutcome, ev: &Events) {
    assert_eq!(out.status, ScanStatus::Completed);
    assert_eq!(g.violations(), Vec::<String>::new(), "forbidden requests");
    assert!(ev.issues().is_empty(), "{:?}", ev.issues());
    let root = out.root.as_ref().expect("root summary");
    assert_eq!(root.totals, out.totals, "root summary and outcome agree");
    assert!(root.complete, "a placeholder boundary is not an error");
    let placeholders: Vec<PathBuf> = ev
        .boundaries()
        .iter()
        .filter(|b| b.reason == BoundaryReason::CloudPlaceholder)
        .map(|b| b.path.clone())
        .collect();
    for p in reported_paths(ev) {
        assert!(
            !placeholders.iter().any(|ph| p.starts_with(ph) && p != *ph),
            "reported beneath a placeholder: {}",
            p.display()
        );
    }
    for ph in &placeholders {
        assert!(
            ev.summary_of(ph).is_none(),
            "{} was summarized",
            ph.display()
        );
        assert!(!g.reads().contains(ph), "{} was listed", ph.display());
    }
}

fn reads(g: &Graph) -> BTreeSet<PathBuf> {
    g.reads().into_iter().collect()
}

fn paths(root: &Path, rel: &[&str]) -> BTreeSet<PathBuf> {
    rel.iter()
        .map(|r| {
            if r.is_empty() {
                root.to_path_buf()
            } else {
                root.join(r)
            }
        })
        .collect()
}

#[test]
fn macos_dataless_directory_is_never_listed() {
    // root/a/f.txt, root/a/icloud (dataless, holds 3 MiB in the cloud), root/b.
    let mut g = Graph::new();
    let a = g.dir(0, "a");
    g.add(a, "f.txt", Node::File(100));
    cloud_dir(&mut g, a, "icloud", dataless());
    g.dir(0, "b");
    let root = g.root().to_path_buf();
    let (g, out, ev) = scan_graph_with(g, opts());
    assert_no_hydration(&g, &out, &ev);

    assert_eq!(reads(&g), paths(&root, &["", "a", "b"]), "{:?}", g.reads());
    let b = ev.boundaries();
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(b[0].path, root.join("a/icloud"));
    assert_eq!(b[0].reason, BoundaryReason::CloudPlaceholder);
    assert_eq!(b[0].depth, 2);
    assert!(b[0].identity.is_some());

    // Counted from its own metadata only: one directory entry of 4096 bytes, nothing inside.
    let t = out.totals;
    assert_eq!(t.entries, 4, "a, b, f.txt, icloud");
    assert_eq!((t.files, t.dirs), (1, 3));
    assert_eq!(t.logical, 4 * DIR + 100, "root, a, b, icloud + f.txt");
    assert_eq!(t.allocated, 5 * DIR, "f.txt rounds up to one block");
    assert_eq!((t.boundaries, t.cloud_placeholders), (1, 1));

    // The listing marks it, so callers never read it and classify it at least S2.
    let listing = ev
        .listings()
        .into_iter()
        .find(|l| l.path == root.join("a"))
        .expect("listing of a");
    let entry = listing
        .entries
        .iter()
        .find(|e| e.name == "icloud")
        .expect("icloud entry");
    assert!(entry.cloud_placeholder);
    assert_eq!(entry.kind, EntryKind::Dir);
    assert!(
        listing
            .entries
            .iter()
            .filter(|e| e.name != "icloud")
            .all(|e| !e.cloud_placeholder)
    );

    // Its contents are unknown: confidence drops where it is, and only there.
    let sa = ev.summary_of(&root.join("a")).expect("a summary");
    assert_eq!(sa.reclaim_confidence, Confidence::Low);
    assert_eq!((sa.totals.boundaries, sa.totals.cloud_placeholders), (1, 1));
    assert_eq!(sa.totals.logical, 2 * DIR + 100);
    assert_eq!(
        ev.summary_of(&root.join("b"))
            .expect("b")
            .reclaim_confidence,
        Confidence::High
    );
    assert_eq!(out.root.expect("root").reclaim_confidence, Confidence::Low);
}

#[test]
fn windows_recall_and_offline_directories_are_never_listed() {
    let mut g = Graph::new();
    cloud_dir(
        &mut g,
        0,
        "recall-on-open",
        windows(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_RECALL_ON_OPEN),
    );
    cloud_dir(
        &mut g,
        0,
        "recall-on-data",
        windows(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS),
    );
    cloud_dir(
        &mut g,
        0,
        "offline",
        windows(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_OFFLINE),
    );
    // Cloud-managed but fully local ("Always keep on this device"): listed as usual.
    let pinned = g.dir(0, "pinned");
    g.add(pinned, "local.bin", Node::File(10));
    g.set_cloud(
        pinned,
        windows(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_PINNED),
    );
    let root = g.root().to_path_buf();
    let (g, out, ev) = scan_graph_with(g, opts());
    assert_no_hydration(&g, &out, &ev);

    assert_eq!(reads(&g), paths(&root, &["", "pinned"]), "{:?}", g.reads());
    let got: HashSet<(PathBuf, BoundaryReason)> = ev
        .boundaries()
        .iter()
        .map(|b| (b.path.clone(), b.reason))
        .collect();
    let want: HashSet<(PathBuf, BoundaryReason)> = ["recall-on-open", "recall-on-data", "offline"]
        .iter()
        .map(|n| (root.join(n), BoundaryReason::CloudPlaceholder))
        .collect();
    assert_eq!(got, want);
    let t = out.totals;
    assert_eq!((t.files, t.dirs, t.entries), (1, 4, 5));
    assert_eq!(t.logical, 5 * DIR + 10);
    assert_eq!((t.boundaries, t.cloud_placeholders), (3, 3));
    assert_eq!(
        ev.summary_of(&root.join("pinned"))
            .expect("pinned")
            .reclaim_confidence,
        Confidence::High
    );
    assert_eq!(out.root.expect("root").reclaim_confidence, Confidence::Low);
}

#[test]
fn placeholder_files_are_counted_from_metadata_and_never_opened() {
    // Regression: files are only ever lstat'ed, placeholder or not; the platform layer has no
    // way to open them and the scanner lists only real, local directories.
    let mut g = Graph::new();
    let d = g.dir(0, "docs");
    let f1 = g.add(d, "evicted.pdf", Node::File(5000));
    let f2 = g.add(d, "online-only.docx", Node::File(7000));
    g.add(d, "local.txt", Node::File(1));
    g.set_cloud(f1, dataless());
    g.set_cloud(
        f2,
        windows(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE),
    );
    let root = g.root().to_path_buf();
    let (g, out, ev) = scan_graph_with(g, opts());
    assert_no_hydration(&g, &out, &ev);

    assert_eq!(reads(&g), paths(&root, &["", "docs"]));
    assert!(ev.boundaries().is_empty(), "files are not boundaries");
    assert_eq!(out.totals.files, 3);
    assert_eq!(out.totals.cloud_placeholders, 2);
    assert_eq!(out.totals.logical, 2 * DIR + 12_001);
    let listing = ev
        .listings()
        .into_iter()
        .find(|l| l.path == root.join("docs"))
        .expect("listing");
    let flagged: BTreeSet<String> = listing
        .entries
        .iter()
        .filter(|e| e.cloud_placeholder)
        .map(|e| e.name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        flagged,
        ["evicted.pdf", "online-only.docx"].map(String::from).into()
    );
    // A file's size is fully known from its metadata: no confidence penalty.
    assert_eq!(out.root.expect("root").reclaim_confidence, Confidence::High);
}

#[test]
fn placeholder_root_is_refused_not_listed() {
    for cloud in [
        dataless(),
        windows(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_RECALL_ON_OPEN),
    ] {
        let mut g = Graph::new();
        g.add(0, "remote.bin", Node::File(1 << 20));
        g.set_cloud(0, cloud);
        let g = Arc::new(g);
        let (p, root) = (Arc::clone(&g), g.root().to_path_buf());
        let r = within(HANG, move || run_with(p, &root, &opts()));
        let err = match r {
            Err(e) => e,
            Ok(_) => panic!("a placeholder root was scanned"),
        };
        assert!(
            matches!(&err, ScanError::RootCloudPlaceholder { path } if path == g.root()),
            "{err:?}"
        );
        assert!(err.to_string().contains("placeholder"), "{err}");
        assert_eq!(ScullError::from(&err).kind, ErrorKind::SafetyBlocked);
        assert!(g.reads().is_empty(), "listed: {:?}", g.reads());
        assert_eq!(g.violations(), Vec::<String>::new());
    }
}

#[test]
fn placeholder_wins_over_other_boundary_reasons() {
    // At the depth limit both apply; the placeholder reason is the one callers must see.
    let mut g = Graph::new();
    cloud_dir(&mut g, 0, "icloud", dataless());
    g.dir(0, "plain");
    let root = g.root().to_path_buf();
    let mut o = opts();
    o.max_depth = Some(0);
    let (g, out, ev) = scan_graph_with(g, o);
    assert_no_hydration(&g, &out, &ev);
    let got: HashSet<(PathBuf, BoundaryReason)> = ev
        .boundaries()
        .iter()
        .map(|b| (b.path.clone(), b.reason))
        .collect();
    let want: HashSet<(PathBuf, BoundaryReason)> = [
        (root.join("icloud"), BoundaryReason::CloudPlaceholder),
        (root.join("plain"), BoundaryReason::MaxDepth),
    ]
    .into();
    assert_eq!(got, want);
    assert_eq!(reads(&g), paths(&root, &[""]));
}

#[test]
fn placeholders_deep_in_a_tree_never_get_listed() {
    // Placeholders at several depths, nested inside each other and next to local data.
    let mut g = Graph::new();
    let mut parent = 0;
    let mut local_files = 0;
    let mut placeholders = Vec::new();
    for depth in 0..6 {
        let d = g.dir(parent, &format!("d{depth}"));
        g.add(d, "keep.bin", Node::File(100));
        local_files += 1;
        let c = cloud_dir(&mut g, d, "cloud", dataless());
        // A placeholder inside a placeholder: must not be reached either.
        cloud_dir(&mut g, c, "inner", dataless());
        placeholders.push(c);
        parent = d;
    }
    let (g, out, ev) = scan_graph_with(g, opts());
    assert_no_hydration(&g, &out, &ev);
    assert_eq!(g.reads().len(), 7, "root + d0..d5: {:?}", g.reads());
    assert_eq!(out.totals.boundaries, 6);
    assert_eq!(out.totals.cloud_placeholders, 6);
    assert_eq!(out.totals.files, local_files);
    assert_eq!(out.totals.dirs, 12, "d0..d5 and one placeholder in each");
    for s in ev.dir_done() {
        assert_eq!(
            s.reclaim_confidence,
            Confidence::Low,
            "{}",
            s.path.display()
        );
    }
    assert_eq!(placeholders.len(), 6);
}
