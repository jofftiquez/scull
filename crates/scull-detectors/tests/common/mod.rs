//! Test driver for detector fixtures: a minimal no-follow walk that plays the scanner's part,
//! a `std::fs` config reader (tests may use `std::fs`; the crate itself may not), and helpers
//! for caller signals, activity and byte totals.

#![allow(dead_code, clippy::unwrap_used)]

pub mod jvm_caches;

use scull_core::{Candidate, Tier};
use scull_detectors::{
    CallerSignals, ConfigReader, DetectContext, DetectedArtifact, Detectors, SharedTargetDirs,
};
use scull_fixtures::{Fixture, InvariantInputs, PlanEntry};
use scull_platform::{EntryKind, Native, Platform};
use scull_safety::{ActivityWindow, GitTracking, Tri};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Reads small regular files with `std::fs` (test-only reader).
pub struct StdReader;

impl ConfigReader for StdReader {
    fn read_text(&self, path: &Path, max_bytes: u64) -> Option<String> {
        let md = fs::symlink_metadata(path).ok()?;
        if !md.is_file() || md.len() > max_bytes {
            return None;
        }
        let mut s = String::new();
        fs::File::open(path)
            .ok()?
            .take(max_bytes)
            .read_to_string(&mut s)
            .ok()?;
        Some(s)
    }
}

/// What a scan of a fixture produced.
pub struct Scan {
    /// Every artifact, in walk order (shared target directories last).
    pub found: Vec<DetectedArtifact>,
    /// Directories the walk visited (to prove it never went through a link).
    pub visited: Vec<PathBuf>,
}

impl Scan {
    /// Paths of the artifacts, relative to `root`, with `/` separators, sorted.
    pub fn rel_paths(&self, root: &Path) -> Vec<String> {
        let mut v: Vec<String> = self.found.iter().map(|a| rel(root, &a.path)).collect();
        v.sort();
        v
    }

    /// The single artifact at `root/rel`.
    pub fn at(&self, root: &Path, rel_path: &str) -> &DetectedArtifact {
        let hits: Vec<&DetectedArtifact> = self
            .found
            .iter()
            .filter(|a| rel(root, &a.path) == rel_path)
            .collect();
        assert_eq!(hits.len(), 1, "expected exactly one artifact at {rel_path}");
        hits[0]
    }
}

/// `path` relative to `root`, `/`-separated.
pub fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Walk `root` like a scanner would: no-follow listings, links never entered, no descent into
/// a detected artifact; every directory holding a `Cargo.toml` is offered to the shared
/// target-dir collector. `explicit` are caller-supplied target dirs (e.g. `CARGO_TARGET_DIR`).
pub fn scan(root: &Path, explicit: &[(PathBuf, &str)]) -> Scan {
    let platform = Native::new();
    let detectors = Detectors::bundled().expect("bundled detectors");
    let reader = StdReader;
    let ctx = DetectContext::new().with_config_reader(&reader);
    let mut shared = SharedTargetDirs::new();
    for (p, label) in explicit {
        assert!(shared.add_explicit(p, *label));
    }
    let mut found = Vec::new();
    let mut visited = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        visited.push(dir.clone());
        if let Some(a) = detectors.detect(&dir, &platform, &ctx) {
            found.push(a);
            continue;
        }
        let mut has_manifest = false;
        for e in platform.read_dir(&dir).expect("list") {
            let e = e.expect("entry");
            match e.kind {
                EntryKind::Dir => stack.push(e.path),
                EntryKind::File if e.file_name == "Cargo.toml" => has_manifest = true,
                _ => {}
            }
        }
        if has_manifest {
            shared.add_project(&dir, &platform, &ctx);
        }
    }
    shared.finish(&platform, &ctx, &mut found);
    Scan { found, visited }
}

/// Newest modification time of any regular file under `path` (no-follow; links themselves are
/// created by the fixture "now" and are not activity in these tests).
pub fn newest_mtime(path: &Path) -> Option<i64> {
    let mut newest: Option<i64> = None;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let md = fs::symlink_metadata(&p).unwrap();
        if md.is_dir() {
            for e in fs::read_dir(&p).unwrap() {
                stack.push(e.unwrap().path());
            }
        } else if md.is_file()
            && let Ok(t) = md.modified()
        {
            let secs = t
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            newest = Some(newest.map_or(secs, |n| n.max(secs)));
        }
    }
    newest
}

/// Sum of logical sizes of the regular files under `path` (no-follow).
pub fn logical_bytes(path: &Path) -> u64 {
    let md = fs::symlink_metadata(path).unwrap();
    if md.is_dir() {
        fs::read_dir(path)
            .unwrap()
            .map(|e| logical_bytes(&e.unwrap().path()))
            .sum()
    } else if md.is_file() {
        md.len()
    } else {
        0
    }
}

/// Set the mtime of every regular file under `fx.root()/rel` to `t`.
pub fn age_tree(fx: &mut Fixture, rel_path: &str, t: SystemTime) {
    let root = fx.root().to_path_buf();
    let mut stack = vec![fx.path(rel_path).unwrap()];
    let mut files = Vec::new();
    while let Some(p) = stack.pop() {
        let md = fs::symlink_metadata(&p).unwrap();
        if md.is_dir() {
            for e in fs::read_dir(&p).unwrap() {
                stack.push(e.unwrap().path());
            }
        } else if md.is_file() {
            files.push(p.strip_prefix(&root).unwrap().to_path_buf());
        }
    }
    for f in files {
        fx.set_mtime(&f, t).unwrap();
    }
}

/// Caller signals for an artifact on a clean machine: not tracked, not protected, idle unless
/// its files say otherwise (activity from the newest mtime, 24 h window).
pub fn clean_caller(a: &DetectedArtifact) -> CallerSignals {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    CallerSignals {
        git: GitTracking::NotTracked,
        in_protected_root: false,
        user_excluded: false,
        protect_marker: false,
        in_use: Tri::No,
        crosses_mount: false,
        link_escape: false,
        cloud_sync: false,
        recent_activity: ActivityWindow::default().observe(newest_mtime(&a.path), false, now),
    }
}

/// Whether this platform provides file identity (Windows does not until #32).
pub fn identity_available() -> bool {
    Native::new().capabilities().file_identity
}

/// Classify `a` with `caller` and check the tier: the rubric result must be `expected`
/// everywhere; the candidate is `expected` too, or S3 where file identity is unavailable
/// (it can't be revalidated, SAFE-05).
pub fn classify_expect(a: &DetectedArtifact, caller: &CallerSignals, expected: Tier) -> Candidate {
    assert_eq!(
        scull_safety::classify(&a.signals(caller)).tier(),
        expected,
        "rubric tier for {}",
        a.path.display()
    );
    let c = a.clone().into_candidate(caller);
    assert!(c.is_consistent(), "candidate is consistent");
    if identity_available() {
        assert_eq!(c.tier, expected, "candidate tier for {}", a.path.display());
    } else {
        assert_eq!(c.tier, Tier::S3);
    }
    c
}

/// Invariant inputs with the executable part of `candidates` as the plan (I-1).
pub fn plan_inputs(candidates: &[Candidate]) -> InvariantInputs {
    InvariantInputs::new().plan(
        candidates
            .iter()
            .filter(|c| c.action.is_mutating())
            .map(PlanEntry::from),
    )
}

/// Run the universal invariants and require I-2 and I-3 to have run and passed.
pub fn check(fx: &Fixture, inputs: &InvariantInputs) {
    let report = fx.check_invariants(inputs);
    report.assert_ok();
    let ran = report.ran();
    assert!(ran.contains(&scull_fixtures::InvariantId::I2), "I-2 ran");
    assert!(ran.contains(&scull_fixtures::InvariantId::I3), "I-3 ran");
}
