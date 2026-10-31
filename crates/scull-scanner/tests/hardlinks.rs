//! Hard-link-aware sizing (P1-SCAN-02, PHY-03): every file counted once; per-subtree split of
//! allocated bytes into unique (freed if the subtree is removed) and shared (links elsewhere,
//! e.g. the pnpm store). Everything runs inside fresh temp directories; the only deletion is
//! inside a fixture's own temp root (FX-PHY-01 measured delta).
#![allow(clippy::unwrap_used, clippy::expect_used)] // tests: panics are failures

mod common;

use common::*;
use scull_core::{Confidence, FileIdentity};
use scull_fixtures::{Fixture, InvariantInputs};
use scull_platform::{EntryKind, Metadata, Native, Platform};
use scull_scanner::{DirSummary, ScanOptions, ScanOutcome, ScanStatus};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Big enough that NTFS stores the data in clusters rather than in the MFT record, so every
/// file has a non-zero allocated size on all CI filesystems.
const SZ: usize = 10_000;

fn opts(threads: usize) -> ScanOptions {
    let mut o = ScanOptions::default();
    o.threads = threads;
    o
}

fn md(p: &Path) -> Metadata {
    Native.metadata(p).unwrap()
}

fn alloc(p: &Path) -> u64 {
    md(p).allocated_size.expect("allocated size")
}

fn link(original: &Path, link: &Path) {
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    fs::hard_link(original, link).unwrap();
}

fn mkfile(p: &Path, len: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    file(p, len);
}

/// Every entry below `dir` (not following links), with its platform metadata.
fn entries(dir: &Path, out: &mut Vec<(PathBuf, Metadata)>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let m = md(&p);
        let is_dir = m.kind == EntryKind::Dir;
        out.push((p.clone(), m));
        if is_dir {
            entries(&p, out);
        }
    }
}

/// Independent computation of a subtree's sizes: a plain recursive walk that counts each
/// identity once and calls it shared if fewer of its links are inside than its link count.
#[derive(Debug, Default, PartialEq, Eq)]
struct RefSizes {
    logical: u64,
    allocated: u64,
    shared: u64,
    multi_linked: u64,
    deduped: u64,
}

fn reference_sizes(dir: &Path) -> RefSizes {
    let mut all = Vec::new();
    entries(dir, &mut all);
    let own = md(dir);
    let mut r = RefSizes {
        logical: own.logical_size,
        allocated: own.allocated_size.unwrap_or(0),
        ..RefSizes::default()
    };
    let mut ids: HashMap<FileIdentity, (u64, u64, u64)> = HashMap::new();
    for (_, m) in &all {
        let multi = m.kind != EntryKind::Dir && m.nlink.unwrap_or(1) > 1;
        if multi {
            r.multi_linked += 1;
            let id = m.identity.expect("identity");
            let e = ids
                .entry(id)
                .or_insert((0, m.nlink.unwrap(), m.allocated_size.unwrap_or(0)));
            e.0 += 1;
            if e.0 > 1 {
                r.deduped += 1;
                continue;
            }
        }
        r.logical += m.logical_size;
        r.allocated += m.allocated_size.unwrap_or(0);
    }
    r.shared = ids
        .values()
        .filter(|(seen, nlink, _)| seen < nlink)
        .map(|(_, _, a)| a)
        .sum();
    r
}

fn assert_matches_link_reference(s: &DirSummary) {
    let r = reference_sizes(&s.path);
    let got = RefSizes {
        logical: s.totals.logical,
        allocated: s.totals.allocated,
        shared: s.shared_allocated,
        multi_linked: s.totals.multi_linked,
        deduped: s.totals.hardlinks_deduped,
    };
    assert_eq!(got, r, "{}", s.path.display());
    assert_eq!(
        s.unique_allocated + s.shared_allocated,
        s.totals.allocated,
        "{}",
        s.path.display()
    );
}

fn summary<'a>(ev: &'a Events, p: &Path) -> &'a DirSummary {
    ev.summary_of(p)
        .unwrap_or_else(|| panic!("no summary for {}", p.display()))
}

fn root_of(out: &ScanOutcome) -> &DirSummary {
    out.root.as_ref().expect("root summary")
}

#[test]
fn two_links_in_the_same_subtree_count_once_and_are_reclaimable() -> TestResult {
    let (_t, root) = temp_root();
    mkfile(&root.join("a/x.bin"), SZ);
    link(&root.join("a/x.bin"), &root.join("a/sub/x-again.bin"));
    mkfile(&root.join("b/y.bin"), SZ);
    let x = alloc(&root.join("a/x.bin"));
    let (out, ev) = run(&root, &opts(4))?;
    assert_eq!(out.status, ScanStatus::Completed);

    let a = summary(&ev, &root.join("a"));
    assert_matches_link_reference(a);
    assert_eq!((a.totals.multi_linked, a.totals.hardlinks_deduped), (2, 1));
    assert_eq!(
        a.shared_allocated, 0,
        "both links inside a/: fully reclaimable"
    );
    assert_eq!(a.unique_allocated, a.totals.allocated);
    assert_eq!(a.reclaim_confidence, Confidence::High);
    let naive = alloc(&root.join("a")) + alloc(&root.join("a/sub")) + 2 * x;
    assert_eq!(a.totals.allocated, naive - x, "counted once");

    // a/sub alone holds one of the two links: its file is pinned by a/x.bin.
    let sub = summary(&ev, &root.join("a/sub"));
    assert_matches_link_reference(sub);
    assert_eq!(sub.shared_allocated, x);

    let r = root_of(&out);
    assert_matches_link_reference(r);
    assert_eq!(out.totals.allocated, r.totals.allocated);
    assert_eq!(out.totals.logical, r.totals.logical);
    assert_eq!(out.totals.hardlinks_deduped, 1);
    let rec = r.reclaimable();
    assert_eq!(
        (rec.bytes, rec.shared_bytes, rec.confidence),
        (r.totals.allocated, 0, Confidence::High)
    );
    Ok(())
}

#[test]
fn link_in_a_sibling_subtree_is_shared() -> TestResult {
    let (_t, root) = temp_root();
    mkfile(&root.join("a/f.bin"), SZ);
    link(&root.join("a/f.bin"), &root.join("b/f.bin"));
    mkfile(&root.join("b/own.bin"), SZ);
    let f = alloc(&root.join("a/f.bin"));
    let (out, ev) = run(&root, &opts(4))?;

    for (d, own) in [("a", 0), ("b", alloc(&root.join("b/own.bin")))] {
        let s = summary(&ev, &root.join(d));
        assert_matches_link_reference(s);
        assert_eq!(s.shared_allocated, f, "{d}: other link is in the sibling");
        assert_eq!(s.unique_allocated, alloc(&root.join(d)) + own, "{d}");
        assert_eq!(s.reclaim_confidence, Confidence::High);
    }
    let r = root_of(&out);
    assert_matches_link_reference(r);
    assert_eq!(r.shared_allocated, 0, "both links meet at the root");
    assert_eq!(out.totals.allocated, r.totals.allocated);
    Ok(())
}

/// A pnpm-style store: `n` packages of two files each, `size` bytes per file.
fn pnpm_store(fx: &mut Fixture, store: &str, n: usize, size: u64) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for i in 0..n {
        for f in ["index.js", "lib.js"] {
            let rel = format!("{store}/files/{i:02}/{f}");
            fx.file_of_size(&rel, size).unwrap();
            files.push(PathBuf::from(rel));
        }
    }
    files
}

/// Link every store file into `project/node_modules/.pnpm/pkgN@1.0.0/node_modules/pkgN/`.
fn pnpm_node_modules(fx: &mut Fixture, project: &str, store_files: &[PathBuf]) {
    for f in store_files {
        let pkg = f.parent().unwrap().file_name().unwrap().to_string_lossy();
        let name = f.file_name().unwrap().to_string_lossy();
        let rel =
            format!("{project}/node_modules/.pnpm/pkg{pkg}@1.0.0/node_modules/pkg{pkg}/{name}");
        fx.hardlink(&rel, f).unwrap();
    }
}

/// Allocated bytes of the directory nodes (not files) at and below `dir`.
fn dir_nodes_alloc(dir: &Path) -> u64 {
    let mut all = Vec::new();
    entries(dir, &mut all);
    alloc(dir)
        + all
            .iter()
            .filter(|(_, m)| m.kind == EntryKind::Dir)
            .map(|(_, m)| m.allocated_size.unwrap_or(0))
            .sum::<u64>()
}

/// FX-PHY-01: a pnpm project whose `node_modules` is hard-linked to a store outside the
/// scanned tree. Predicted reclaim is ~0 (only the directory nodes), the rest is reported as
/// shared, and the measured change after removing `node_modules` agrees.
#[test]
fn fx_phy_01_pnpm_node_modules_linked_to_store_frees_about_nothing() -> TestResult {
    let mut fx = Fixture::new()?;
    let store = pnpm_store(&mut fx, "store", 10, 64 * 1024);
    fx.npm_project("project", "app", scull_fixtures::NodeLock::Pnpm)?;
    pnpm_node_modules(&mut fx, "project", &store);
    fx.checkpoint()?;
    let project = fx.path("project")?;
    let nm = project.join("node_modules");
    let files_alloc: u64 = store.iter().map(|f| alloc(&fx.path(f).unwrap())).sum();

    // Scan the project only: the store is outside the scan root.
    let (out, ev) = run(&project, &opts(4))?;
    let s = summary(&ev, &nm);
    assert_matches_link_reference(s);
    assert_eq!(
        s.shared_allocated, files_alloc,
        "every file is pinned by the store"
    );
    assert_eq!(
        s.unique_allocated,
        dir_nodes_alloc(&nm),
        "only directory nodes are freed"
    );
    assert!(
        s.unique_allocated * 5 < s.totals.allocated,
        "reclaim ≈ 0: {} of {}",
        s.unique_allocated,
        s.totals.allocated
    );
    assert_eq!(s.reclaim_confidence, Confidence::High);
    let rec = s.reclaimable();
    assert_eq!(
        (rec.bytes, rec.shared_bytes),
        (s.unique_allocated, files_alloc)
    );
    assert_eq!(root_of(&out).shared_allocated, files_alloc);
    assert_eq!(
        out.peak_link_identities,
        store.len(),
        "no link completes inside"
    );

    // Scan everything: node_modules is still shared (with the store), the root owns it all.
    let (all, all_ev) = run(fx.root(), &opts(4))?;
    assert_eq!(summary(&all_ev, &nm).shared_allocated, files_alloc);
    let r = root_of(&all);
    assert_matches_link_reference(r);
    assert_eq!(r.shared_allocated, 0);
    assert_eq!(all.totals.allocated, r.totals.allocated);
    assert_eq!(all.totals.hardlinks_deduped, store.len() as u64);

    // Measured delta: remove node_modules (inside the fixture's temp root) and measure the
    // allocated bytes of everything (store included) again.
    let project_node_before = alloc(&project);
    let before = r.totals.allocated;
    fs::remove_dir_all(&nm)?;
    let (after, _) = run(fx.root(), &opts(4))?;
    assert_eq!(
        root_of(&after).totals.allocated,
        reference_sizes(fx.root()).allocated
    );
    // Signed: a directory node may grow or shrink when an entry is removed, depending on the
    // filesystem; that change is measured, not predicted.
    let measured = i128::from(before) - i128::from(root_of(&after).totals.allocated);
    let project_node_change = i128::from(project_node_before) - i128::from(alloc(&project));
    assert_eq!(
        measured,
        i128::from(s.unique_allocated) + project_node_change,
        "predicted reclaim matches the measured change"
    );
    for f in &store {
        assert_eq!(fs::metadata(fx.path(f)?)?.len(), 64 * 1024, "store intact");
    }
    fx.check_invariants(&InvariantInputs::new().allow("project/node_modules"))
        .assert_ok();
    Ok(())
}

/// FX-FS-05: a file hard-linked between a candidate and the canary directory. The canary is
/// left intact and the bytes are attributed once (and as shared from inside the root).
#[test]
fn fx_fs_05_link_to_canary_is_shared_and_counted_once() -> TestResult {
    let mut fx = Fixture::new()?;
    fx.node_modules("project", &["left-pad"])?;
    let canary_file = fx.canary().symlink_target_file.clone();
    fx.hardlink("project/node_modules/left-pad/data.txt", &canary_file)?;
    fx.checkpoint()?;
    let c = alloc(&canary_file);

    let (out, ev) = run(fx.root(), &opts(4))?;
    let nm = summary(&ev, &fx.path("project/node_modules")?);
    assert_matches_link_reference(nm);
    assert_eq!(nm.shared_allocated, c, "pinned by the canary link");
    assert_eq!(root_of(&out).shared_allocated, c);

    // Root and canary together: one file, counted once, owned by the common parent.
    let (both, _) = run(fx.base(), &opts(4))?;
    let b = root_of(&both);
    assert_matches_link_reference(b);
    assert_eq!(b.totals.hardlinks_deduped, 1);
    assert_eq!(b.shared_allocated, 0);
    assert_eq!(both.totals.allocated, b.totals.allocated);

    fx.check_invariants(&InvariantInputs::new()).assert_ok();
    Ok(())
}

/// FX-CLS-08 (sizing part): a pnpm monorepo with 30 packages, each with a `node_modules`
/// linked to one store outside the workspace. Totals match an independent measurement.
#[test]
fn fx_cls_08_pnpm_monorepo_sizes_match_independent_dedup() -> TestResult {
    let mut fx = Fixture::new()?;
    let store = pnpm_store(&mut fx, "store", 6, 12 * 1024);
    let packages: Vec<String> = (0..30).map(|i| format!("p{i:02}")).collect();
    let names: Vec<&str> = packages.iter().map(String::as_str).collect();
    fx.pnpm_workspace("ws", "mono", &names)?;
    for (i, p) in packages.iter().enumerate() {
        // Each package depends on a rotating half of the store.
        let deps: Vec<PathBuf> = store
            .iter()
            .enumerate()
            .filter(|(j, _)| (i + j) % 2 == 0)
            .map(|(_, f)| f.clone())
            .collect();
        pnpm_node_modules(&mut fx, &format!("ws/packages/{p}"), &deps);
    }
    fx.checkpoint()?;
    let ws = fx.path("ws")?;

    let (out, ev) = run(&ws, &opts(8))?;
    assert_eq!(ev.dir_done().len(), reference(&ws).dirs as usize + 1);
    for s in ev.dir_done() {
        assert_matches_link_reference(s);
    }
    for p in &packages {
        let nm = summary(&ev, &ws.join("packages").join(p).join("node_modules"));
        assert_eq!(nm.unique_allocated, dir_nodes_alloc(&nm.path), "{p}");
    }
    let r = root_of(&out);
    let store_alloc: u64 = store.iter().map(|f| alloc(&fx.path(f).unwrap())).sum();
    assert_eq!(
        r.shared_allocated, store_alloc,
        "each store file once, all shared"
    );
    assert_eq!(out.totals.allocated, r.totals.allocated);
    assert_eq!(out.totals.logical, r.totals.logical);

    let (all, _) = run(fx.root(), &opts(8))?;
    assert_matches_link_reference(root_of(&all));
    assert_eq!(root_of(&all).shared_allocated, 0);
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
    Ok(())
}

/// A tree with hard links of 2-5 links each, spread over directories by a fixed
/// pseudo-random sequence; some links live in `outside` (beside the root). Returns the root.
fn tangled_tree(base: &Path) -> PathBuf {
    let root = base.join("root");
    let outside = base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let dirs: Vec<PathBuf> = (0..24)
        .map(|i| root.join(format!("d{}/e{}", i % 6, i / 6)))
        .collect();
    for d in &dirs {
        fs::create_dir_all(d).unwrap();
    }
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n as u64) as usize
    };
    for i in 0..80 {
        let first = dirs[next(dirs.len())].join(format!("f{i}"));
        file(&first, SZ + i * 37);
        for k in 1..(1 + next(5)) {
            let target = if next(6) == 0 {
                outside.join(format!("f{i}-{k}"))
            } else {
                dirs[next(dirs.len())].join(format!("f{i}-{k}"))
            };
            fs::hard_link(&first, target).unwrap();
        }
    }
    for i in 0..40 {
        file(&dirs[next(dirs.len())].join(format!("single{i}")), i * 100);
    }
    root
}

#[test]
fn totals_equal_an_independent_dedup_computation() -> TestResult {
    let base = tempfile::tempdir()?;
    let root = tangled_tree(base.path());
    let (out, ev) = run(&root, &opts(4))?;
    let done = ev.dir_done();
    assert_eq!(done.len(), 31); // root + 6 + 24
    for s in &done {
        assert_matches_link_reference(s);
    }
    let r = root_of(&out);
    assert!(r.totals.hardlinks_deduped > 0 && r.shared_allocated > 0);
    assert_eq!(out.totals.allocated, r.totals.allocated);
    assert_eq!(out.totals.logical, r.totals.logical);
    assert_eq!(out.totals.hardlinks_deduped, r.totals.hardlinks_deduped);
    assert_eq!(out.totals.multi_linked, r.totals.multi_linked);
    Ok(())
}

#[test]
fn numbers_are_identical_for_1_to_16_threads() -> TestResult {
    let base = tempfile::tempdir()?;
    let root = tangled_tree(base.path());
    let mut runs = Vec::new();
    for threads in [1, 2, 3, 4, 8, 16] {
        let (out, ev) = run(&root, &opts(threads))?;
        let mut done: Vec<DirSummary> = ev.dir_done().into_iter().cloned().collect();
        done.sort_by(|a, b| a.path.cmp(&b.path));
        runs.push((out.totals, done));
    }
    for w in runs.windows(2) {
        assert_eq!(w[0], w[1]);
    }
    Ok(())
}

#[test]
fn link_identity_set_holds_only_multi_linked_files() -> TestResult {
    // No hard links at all: nothing is tracked.
    let (_t, plain) = temp_root();
    wide_tree(&plain, 3000, 10);
    let (out, _) = run(&plain, &opts(8))?;
    assert_eq!(out.totals.files, 3000);
    assert_eq!(out.peak_link_identities, 0);

    // Thousands of single files plus four linked files: at most four identities, ever.
    let (_t2, root) = temp_root();
    wide_tree(&root, 3000, 10);
    for i in 0..4 {
        mkfile(&root.join(format!("links/l{i}")), 10);
        link(
            &root.join(format!("links/l{i}")),
            &root.join(format!("d{i}/l{i}")),
        );
    }
    let (out, _) = run(&root, &opts(8))?;
    assert_eq!(out.totals.multi_linked, 8);
    assert!(
        (1..=4).contains(&out.peak_link_identities),
        "{}",
        out.peak_link_identities
    );
    Ok(())
}

/// Hide identities (or link counts) of multi-linked files, as a filesystem without file IDs
/// would: nothing is deduplicated, the bytes count as shared, and confidence is low.
fn hide(what: &'static str) -> Tweaked {
    let mut t = Tweaked::new();
    t.meta = Box::new(move |_, m| {
        if m.kind == EntryKind::File && m.nlink.is_some_and(|n| n > 1) {
            match what {
                "identity" => m.identity = None,
                _ => m.nlink = None,
            }
        }
    });
    t
}

#[test]
fn unknown_identity_is_not_deduplicated_and_lowers_confidence() -> TestResult {
    let (_t, root) = temp_root();
    mkfile(&root.join("a/f.bin"), SZ);
    link(&root.join("a/f.bin"), &root.join("a/g.bin"));
    mkfile(&root.join("b/plain.bin"), SZ);
    let f = alloc(&root.join("a/f.bin"));

    let (out, ev) = run_with(Arc::new(hide("identity")), &root, &opts(4))?;
    let a = summary(&ev, &root.join("a"));
    assert_eq!(
        a.totals.allocated,
        alloc(&root.join("a")) + 2 * f,
        "not deduplicated"
    );
    assert_eq!(a.totals.hardlinks_deduped, 0);
    assert_eq!(a.shared_allocated, 2 * f, "other links can't be located");
    assert_eq!(a.reclaim_confidence, Confidence::Low);
    assert_eq!(root_of(&out).reclaim_confidence, Confidence::Low);
    assert_eq!(
        summary(&ev, &root.join("b")).reclaim_confidence,
        Confidence::High
    );
    assert_eq!(out.peak_link_identities, 0);

    let (_, ev) = run_with(Arc::new(hide("nlink")), &root, &opts(4))?;
    let a = summary(&ev, &root.join("a"));
    assert_eq!(a.reclaim_confidence, Confidence::Low);
    assert_eq!(a.shared_allocated, 0);
    Ok(())
}

#[test]
fn unreadable_part_lowers_confidence() -> TestResult {
    let (_t, root) = temp_root();
    mkfile(&root.join("a/locked/f.bin"), SZ);
    mkfile(&root.join("b/f.bin"), SZ);
    let locked = root.join("a/locked");
    let mut t = Tweaked::new();
    t.list = Box::new(move |p| {
        (p == locked).then(|| {
            scull_platform::PlatformError::from_io(
                scull_platform::Operation::ReadDir,
                p,
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )
        })
    });
    let (out, ev) = run_with(Arc::new(t), &root, &opts(2))?;
    assert_eq!(
        summary(&ev, &root.join("a")).reclaim_confidence,
        Confidence::Low
    );
    assert_eq!(
        summary(&ev, &root.join("b")).reclaim_confidence,
        Confidence::High
    );
    assert_eq!(root_of(&out).reclaim_confidence, Confidence::Low);
    Ok(())
}
