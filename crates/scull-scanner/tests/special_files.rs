//! Special files and link loops on every OS (P1-SCAN-03, SAFE-04, SAFE-18, T-14, T-53).
//!
//! * FX-FS-02: symlink loops, chains and dangling links in a real temp tree are recorded as
//!   links and never followed; directory hard-link / bind-mount loops (the same directory
//!   reachable below itself, simulated with an in-memory graph because no OS lets a test make
//!   one) are reported once each as a loop boundary and not descended.
//! * FX-FS-14: FIFOs, sockets (including Windows AF_UNIX reparse points) and device nodes
//!   inside a candidate are classified from metadata and never listed or opened.
//! * Property tests: random trees and graphs with random links, aliases and special files
//!   never hang and never report anything beneath a link.
//!
//! OS-specific halves live in `unix.rs` (real FIFOs, sockets, `/dev`) and `windows.rs`
//! (junction loops, AF_UNIX socket files).

mod common;

use common::graph::{Graph, IO_REPARSE_TAG_AF_UNIX, Node};
use common::*;
use proptest::prelude::*;
use scull_fixtures::{Fixture, InvariantInputs, LinkOutcome};
use scull_platform::{EntryKind, Native};
use scull_scanner::{BoundaryReason, ScanError, ScanOptions, ScanStatus};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// A hang means a loop was followed or a special file was opened.
const HANG: Duration = Duration::from_secs(20);

fn opts() -> ScanOptions {
    let mut o = ScanOptions::default();
    o.threads = 4;
    o.emit_listings = true;
    o
}

/// Scan a graph, collecting events; fails the test on a hang.
fn scan_graph(g: Graph) -> (Arc<Graph>, scull_scanner::ScanOutcome, Events) {
    let g = Arc::new(g);
    let (p, root) = (Arc::clone(&g), g.root().to_path_buf());
    let (out, ev) = within(HANG, move || run_with(p, &root, &opts())).expect("scan starts");
    (g, out, ev)
}

/// Assert the invariants every scan of a graph must keep.
fn assert_graph_scan_is_safe(g: &Graph, out: &scull_scanner::ScanOutcome, ev: &Events) {
    assert_eq!(out.status, ScanStatus::Completed);
    assert_eq!(g.violations(), Vec::<String>::new(), "forbidden requests");
    assert!(ev.issues().is_empty(), "{:?}", ev.issues());
    for p in reported_paths(ev) {
        assert_eq!(
            g.beneath_link(&p),
            Some(false),
            "reported a path beneath a link: {}",
            p.display()
        );
    }
    // No directory is descended inside itself.
    for d in ev.dir_done() {
        let chain = g.chain(&d.path).expect("exists");
        let distinct: HashSet<_> = chain.iter().collect();
        assert_eq!(distinct.len(), chain.len(), "{} loops", d.path.display());
    }
    // Every boundary is a loop edge: the directory is one of its own ancestors.
    for b in ev.boundaries() {
        assert_eq!(b.reason, BoundaryReason::Loop);
        let chain = g.chain(&b.path).expect("exists");
        let (me, ancestors) = chain.split_last().expect("non-empty");
        assert!(ancestors.contains(me), "{} is no loop", b.path.display());
    }
}

#[test]
fn fx_fs_02_directory_hardlink_loops_are_reported_once_and_not_descended() {
    // root/a/b/back -> root (a bind mount of an ancestor), root/a/self -> a (of itself), and
    // root/twin -> b (a second name for a sibling: not a loop, descended once more).
    let mut g = Graph::new();
    let a = g.dir(0, "a");
    let b = g.dir(a, "b");
    g.add(b, "data.bin", Node::File(10_000));
    g.alias(b, "back", 0);
    g.alias(a, "self", a);
    g.alias(0, "twin", b);
    let root = g.root().to_path_buf();
    let (g, out, ev) = scan_graph(g);
    assert_graph_scan_is_safe(&g, &out, &ev);

    let got: BTreeSet<PathBuf> = ev.boundaries().iter().map(|b| b.path.clone()).collect();
    let want: BTreeSet<PathBuf> = ["a/b/back", "a/self", "twin/back"]
        .iter()
        .map(|p| root.join(p))
        .collect();
    assert_eq!(got, want, "each loop edge reported exactly once");
    assert_eq!(ev.boundaries().len(), 3);
    assert_eq!(out.totals.boundaries, 3);
    // The twin is a real second path to b: listed, and its file counted again by path.
    let listed: HashSet<PathBuf> = g.reads().into_iter().collect();
    assert!(listed.contains(&root.join("twin")));
    assert_eq!(g.reads().len(), 4, "root, a, a/b, twin: {:?}", g.reads());
    assert_eq!(out.totals.files, 2);
    assert!(out.root.expect("root").complete, "a loop is not an error");
}

#[test]
fn fx_fs_02_links_are_never_followed_even_where_the_os_would() {
    // The graph's `read_dir` follows links like `opendir` would; the scanner must never ask.
    let mut g = Graph::new();
    let a = g.dir(0, "a");
    let b = g.dir(a, "b");
    let f = g.add(b, "f.bin", Node::File(100));
    g.add(b, "up", Node::Link(EntryKind::Symlink, 0));
    g.add(a, "jn", Node::Link(EntryKind::Junction, a));
    g.add(0, "cloud", Node::Link(EntryKind::ReparsePoint, b));
    g.add(0, "to-file", Node::Link(EntryKind::Symlink, f));
    // A chain of links ending at a directory.
    let c3 = g.add(0, "c3", Node::Link(EntryKind::Symlink, a));
    let c2 = g.add(0, "c2", Node::Link(EntryKind::Symlink, c3));
    g.add(0, "c1", Node::Link(EntryKind::Symlink, c2));
    let (g, out, ev) = scan_graph(g);
    assert_graph_scan_is_safe(&g, &out, &ev);
    assert_eq!(out.totals.links, 7);
    assert_eq!(out.totals.dirs, 2);
    assert_eq!(g.reads().len(), 3, "only real directories: {:?}", g.reads());
    assert!(
        ev.boundaries().is_empty(),
        "links are entries, not boundaries"
    );
}

#[test]
fn fx_fs_14_devices_sockets_and_fifos_inside_a_candidate_are_never_opened() {
    // Device nodes can't be created without root, and AF_UNIX socket files need Winsock:
    // simulated here, real ones in unix.rs / windows.rs.
    let mut g = Graph::new();
    let cand = g.dir(0, "node_modules");
    let deep = g.dir(cand, "pkg");
    g.add(cand, "tty", Node::Special(EntryKind::CharDevice, None));
    g.add(cand, "disk", Node::Special(EntryKind::BlockDevice, None));
    g.add(deep, "pipe", Node::Special(EntryKind::Fifo, None));
    g.add(deep, "sock", Node::Special(EntryKind::Socket, None));
    g.add(
        deep,
        "af_unix",
        Node::Special(EntryKind::Socket, Some(IO_REPARSE_TAG_AF_UNIX)),
    );
    g.add(deep, "door", Node::Special(EntryKind::Other, None));
    g.add(deep, "index.js", Node::File(42));
    let root = g.root().to_path_buf();
    let (g, out, ev) = scan_graph(g);
    assert_graph_scan_is_safe(&g, &out, &ev);
    assert_eq!(out.totals.specials, 6);
    // The candidate is flagged through its subtree totals.
    let s = ev
        .summary_of(&root.join("node_modules"))
        .expect("candidate");
    assert_eq!(s.totals.specials, 6);
    assert_eq!(s.totals.files, 1);
    let kinds: BTreeSet<EntryKind> = ev
        .listings()
        .iter()
        .flat_map(|l| l.entries.iter())
        .filter(|e| e.kind.is_special())
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from([
            EntryKind::CharDevice,
            EntryKind::BlockDevice,
            EntryKind::Fifo,
            EntryKind::Socket,
            EntryKind::Other,
        ])
    );
    assert_eq!(g.reads().len(), 3, "root, candidate, pkg: {:?}", g.reads());
}

#[test]
fn special_or_link_roots_are_refused_without_listing() {
    let mut g = Graph::new();
    let a = g.dir(0, "a");
    for (name, kind) in [
        ("fifo", EntryKind::Fifo),
        ("sock", EntryKind::Socket),
        ("chr", EntryKind::CharDevice),
        ("blk", EntryKind::BlockDevice),
    ] {
        g.add(0, name, Node::Special(kind, None));
    }
    g.add(0, "jn", Node::Link(EntryKind::Junction, a));
    let g = Arc::new(g);
    for (name, want) in [
        ("fifo", EntryKind::Fifo),
        ("sock", EntryKind::Socket),
        ("chr", EntryKind::CharDevice),
        ("blk", EntryKind::BlockDevice),
        ("jn", EntryKind::Junction),
    ] {
        let (p, root) = (Arc::clone(&g), g.root().join(name));
        let res = within(HANG, move || run_with(p, &root, &opts()));
        assert!(
            matches!(res, Err(ScanError::RootNotADirectory { found, .. }) if found == want),
            "{name}"
        );
    }
    assert!(g.reads().is_empty(), "nothing listed: {:?}", g.reads());
    assert!(g.violations().is_empty());
}

#[test]
fn fx_fs_02_symlink_loops_chains_and_dangling_links() -> TestResult {
    let mut fx = Fixture::new()?;
    fx.file("a/f.txt", b"hello")?
        .file("a/b/c/deep.bin", vec![b'x'; 3000])?
        .dir("d")?;
    let root = fx.root().to_path_buf();
    let canary = fx.canary().symlink_target_dir.clone();
    let mut links = Vec::new();
    let mut link = |fx: &mut Fixture, at: &str, to: &Path| -> TestResult {
        if fx.symlink(at, to)? == LinkOutcome::Created {
            links.push(root.join(at));
        }
        Ok(())
    };
    link(&mut fx, "self", Path::new("."))?;
    link(&mut fx, "a/b/up", &root)?;
    link(&mut fx, "a/b/c/rel-up", Path::new("../.."))?;
    link(&mut fx, "loop1", Path::new("loop2"))?;
    link(&mut fx, "loop2", Path::new("loop1"))?;
    link(&mut fx, "c3", Path::new("a"))?;
    link(&mut fx, "c2", Path::new("c3"))?;
    link(&mut fx, "c1", Path::new("c2"))?;
    link(&mut fx, "dangling", Path::new("missing/nowhere"))?;
    link(&mut fx, "a/file-link", Path::new("f.txt"))?;
    link(&mut fx, "d/canary", &canary)?;
    fx.checkpoint()?;
    if links.is_empty() {
        // CI runners permit symlinks: a skip there would hide a missing OS case.
        assert!(
            std::env::var_os("CI").is_none(),
            "symlinks must be creatable on CI: {:?}",
            fx.skipped()
        );
        eprintln!("skipped: symlinks not permitted here ({:?})", fx.skipped());
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
        return Ok(());
    }

    let want = reference(&root);
    let rec = Arc::new(Recording::new(Native));
    let (p, r) = (Arc::clone(&rec), root.clone());
    let (out, ev) = within(HANG, move || run_with(p, &r, &opts()))?;
    assert_eq!(out.status, ScanStatus::Completed);
    assert!(ev.issues().is_empty(), "{:?}", ev.issues());
    assert!(ev.boundaries().is_empty());
    assert_eq!(rec.violations(), Vec::<String>::new());
    assert_matches_reference(&out.totals, &want);
    assert_eq!(out.totals.links as usize, links.len());
    // Only the real directories are listed, each once.
    let reads: BTreeSet<PathBuf> = rec.reads().into_iter().collect();
    let dirs: BTreeSet<PathBuf> = ["", "a", "a/b", "a/b/c", "d"]
        .iter()
        .map(|p| {
            if p.is_empty() {
                root.clone()
            } else {
                root.join(p)
            }
        })
        .collect();
    assert_eq!(reads, dirs);
    assert_eq!(rec.reads().len(), dirs.len());
    for p in reported_paths(&ev) {
        assert!(
            !links.iter().any(|l| p.starts_with(l) && p != *l),
            "reported beneath a link: {}",
            p.display()
        );
    }
    // Each link appears once, as a link.
    let listed: Vec<PathBuf> = ev
        .listings()
        .iter()
        .flat_map(|l| {
            l.entries
                .iter()
                .filter(|e| e.kind == EntryKind::Symlink)
                .map(|e| l.path.join(&e.name))
        })
        .collect();
    assert_eq!(
        listed.iter().collect::<BTreeSet<_>>(),
        links.iter().collect::<BTreeSet<_>>()
    );
    assert_eq!(listed.len(), links.len());
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
    Ok(())
}

/// One random edge of a graph: `(op, a, b, k)` with indices taken modulo what exists.
type EdgeSpec = (u8, u16, u16, u8);

const LINK_KINDS: [EntryKind; 3] = [
    EntryKind::Symlink,
    EntryKind::Junction,
    EntryKind::ReparsePoint,
];
const SPECIAL_KINDS: [EntryKind; 5] = [
    EntryKind::Fifo,
    EntryKind::Socket,
    EntryKind::CharDevice,
    EntryKind::BlockDevice,
    EntryKind::Other,
];

fn build_graph(parents: &[u16], edges: &[EdgeSpec]) -> Graph {
    let mut g = Graph::new();
    let mut dirs = vec![0];
    for (i, p) in parents.iter().enumerate() {
        let parent = dirs[*p as usize % dirs.len()];
        dirs.push(g.dir(parent, &format!("d{i}")));
    }
    for (j, &(op, a, b, k)) in edges.iter().enumerate() {
        let parent = dirs[a as usize % dirs.len()];
        let name = format!("x{j}");
        match op % 4 {
            // A directory hard link / bind mount of any directory (may close a cycle).
            0 => g.alias(parent, &name, dirs[b as usize % dirs.len()]),
            // A link to any node, links included (chains and link cycles).
            1 => {
                let target = b as usize % g.len();
                let kind = LINK_KINDS[k as usize % LINK_KINDS.len()];
                g.add(parent, &name, Node::Link(kind, target));
            }
            2 => {
                let kind = SPECIAL_KINDS[k as usize % SPECIAL_KINDS.len()];
                let tag =
                    (kind == EntryKind::Socket && k % 2 == 0).then_some(IO_REPARSE_TAG_AF_UNIX);
                g.add(parent, &name, Node::Special(kind, tag));
            }
            _ => {
                g.add(parent, &name, Node::File(u64::from(b)));
            }
        }
    }
    g
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Random graphs with directory aliases (cycles), links of every kind and special files:
    /// the scan always finishes, never lists a link or special file, never reports a path
    /// beneath a link, and never descends a directory inside itself.
    #[test]
    fn random_graphs_never_hang_or_report_beneath_a_link(
        parents in proptest::collection::vec(any::<u16>(), 0..8),
        edges in proptest::collection::vec(any::<EdgeSpec>(), 0..16),
    ) {
        let g = build_graph(&parents, &edges);
        let (g, out, ev) = scan_graph(g);
        assert_graph_scan_is_safe(&g, &out, &ev);
        // Every real directory node reachable without links was listed at least once.
        let listed: HashSet<usize> = g
            .reads()
            .iter()
            .map(|p| *g.chain(p).expect("exists").last().expect("non-empty"))
            .collect();
        prop_assert!(listed.contains(&0));
        prop_assert_eq!(listed.len(), parents.len() + 1);
    }
}

/// A random real tree: `dirs[i]` picks the parent of directory `i`, `files` and `links` pick
/// where to put files and symlinks and what each link points to.
fn build_real(
    fx: &mut Fixture,
    dirs: &[u8],
    files: &[(u8, u16)],
    links: &[(u8, u8, u8)],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), Box<dyn std::error::Error>> {
    let root = fx.root().to_path_buf();
    let mut rels = vec![PathBuf::new()];
    for (i, p) in dirs.iter().enumerate() {
        let rel = rels[*p as usize % rels.len()].join(format!("d{i}"));
        fx.dir(&rel)?;
        rels.push(rel);
    }
    let mut file_rels = Vec::new();
    for (j, (d, size)) in files.iter().enumerate() {
        let rel = rels[*d as usize % rels.len()].join(format!("f{j}.bin"));
        fx.file_of_size(&rel, u64::from(*size))?;
        file_rels.push(rel);
    }
    let mut created = Vec::new();
    for (j, &(d, kind, t)) in links.iter().enumerate() {
        let dir = &rels[d as usize % rels.len()];
        let rel = dir.join(format!("l{j}"));
        let t = t as usize;
        let target: PathBuf = match kind % 6 {
            0 => root.join(&rels[t % rels.len()]), // any directory (ancestors too)
            1 => PathBuf::from(".."),              // the parent, relative
            2 => PathBuf::from("."),               // itself
            3 => PathBuf::from(format!("missing-{t}")), // dangling
            4 if !file_rels.is_empty() => root.join(&file_rels[t % file_rels.len()]),
            _ => PathBuf::from(format!("l{}", t % links.len())), // another link: chains, cycles
        };
        if fx.symlink(&rel, &target)? == LinkOutcome::Created {
            created.push(root.join(&rel));
        }
    }
    let dir_paths = rels.iter().map(|r| root.join(r)).collect();
    Ok((dir_paths, created))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Random real trees with random symlinks (loops, chains, dangling, to ancestors): the
    /// scan finishes, lists every real directory exactly once, nothing beneath a link, totals
    /// match an independent walk, and the tree is unchanged (I-2, I-3).
    #[test]
    fn random_real_trees_with_links_never_hang(
        dirs in proptest::collection::vec(any::<u8>(), 0..10),
        files in proptest::collection::vec(any::<(u8, u16)>(), 0..12),
        links in proptest::collection::vec(any::<(u8, u8, u8)>(), 1..12),
    ) {
        let mut fx = Fixture::new().expect("fixture");
        let (dir_paths, created) = build_real(&mut fx, &dirs, &files, &links).expect("build");
        fx.checkpoint().expect("checkpoint");
        let root = fx.root().to_path_buf();
        let want = reference(&root);
        let rec = Arc::new(Recording::new(Native));
        let (p, r) = (Arc::clone(&rec), root.clone());
        let (out, ev) = within(HANG, move || run_with(p, &r, &opts())).expect("scan");
        assert_eq!(out.status, ScanStatus::Completed);
        assert!(ev.issues().is_empty(), "{:?}", ev.issues());
        assert_eq!(rec.violations(), Vec::<String>::new());
        assert_matches_reference(&out.totals, &want);
        assert_eq!(out.totals.links as usize, created.len());
        let mut reads = rec.reads();
        reads.sort();
        let mut want_reads = dir_paths;
        want_reads.sort();
        assert_eq!(reads, want_reads, "every real directory listed exactly once");
        for p in reported_paths(&ev) {
            assert!(
                !created.iter().any(|l| p.starts_with(l) && p != *l),
                "reported beneath a link: {}",
                p.display()
            );
        }
        fx.check_invariants(&InvariantInputs::new()).assert_ok();
    }
}
