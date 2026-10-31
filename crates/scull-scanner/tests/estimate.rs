//! Reclaim estimates of real scans (P1-PHY-01, PRD §9) and how their confidence reaches JSON
//! through `Candidate.sizes`. Everything runs inside fresh fixture temp directories.

mod common;

use common::*;
use scull_core::{Action, Candidate, Confidence};
use scull_fixtures::{Fixture, InvariantInputs};
use scull_scanner::{
    ActionReclaim, EstimateContext, EstimateNote, ScanOptions, Snapshots, VolumeFs, assess,
    estimate,
};

fn opts() -> ScanOptions {
    let mut o = ScanOptions::default();
    o.threads = 4;
    o
}

/// A pnpm-style project: `web/node_modules` hard-linked into `store/`, plus one file of its own.
fn pnpm(fx: &mut Fixture) -> TestResult {
    for i in 0..8 {
        let blob = format!("store/v3/files/{i:02}/blob");
        fx.file_of_size(&blob, 64 * 1024)?;
        fx.hardlink(format!("web/node_modules/.pnpm/pkg{i}/index.js"), &blob)?;
    }
    fx.file_of_size("web/node_modules/.modules.yaml", 3000)?;
    Ok(())
}

#[test]
fn pnpm_node_modules_estimate_frees_about_nothing_and_says_why() -> TestResult {
    let mut fx = Fixture::new()?;
    pnpm(&mut fx)?;
    fx.checkpoint()?;
    let root = fx.root().to_path_buf();
    let (out, ev) = run(&root, &opts())?;
    let nm = ev
        .summary_of(&root.join("web/node_modules"))
        .expect("node_modules summary");

    let ctx = EstimateContext::new(VolumeFs::Ext4, Snapshots::Absent);
    let e = assess(nm, &ctx);
    let r = e.sizes.reclaimable;
    assert_eq!(e.sizes.allocated, nm.totals.allocated);
    assert_eq!(e.sizes.logical, nm.totals.logical);
    assert!(r.shared_bytes >= 8 * 64 * 1024, "store blobs are shared");
    assert_eq!(r.bytes + r.shared_bytes, e.sizes.allocated);
    assert!(r.bytes < e.sizes.allocated / 4, "frees ~0: {r:?}");
    if nm.reclaim_confidence == Confidence::High {
        assert_eq!(r.confidence, Confidence::High, "links fully attributed");
    }
    assert!(e.notes.contains(&EstimateNote::SharedWithLinks {
        bytes: r.shared_bytes
    }));

    // The whole root holds every link: nothing is shared any more.
    let all = estimate(out.root.as_ref().expect("root"), &ctx);
    assert_eq!(all.reclaimable.shared_bytes, 0);
    assert_eq!(all.reclaimable.bytes, all.allocated);

    // Same scan on a clone-capable volume with snapshots: same bytes, low confidence.
    let apfs = estimate(
        nm,
        &EstimateContext::new(VolumeFs::Apfs, Snapshots::Possible),
    );
    assert_eq!(apfs.reclaimable.bytes, r.bytes);
    assert_eq!(apfs.reclaimable.confidence, Confidence::Low);

    // Quarantine moves everything and frees nothing until purge.
    let q = ActionReclaim::for_action(&e.sizes, &Action::Quarantine);
    assert_eq!((q.freed_now, q.moved), (0, e.sizes.allocated));
    assert_eq!(q.freed_after_purge, r.bytes);

    fx.check_invariants(&InvariantInputs::new()).assert_ok();
    Ok(())
}

#[test]
fn confidence_is_surfaced_in_candidate_json() -> TestResult {
    let mut fx = Fixture::new()?;
    pnpm(&mut fx)?;
    fx.checkpoint()?;
    let root = fx.root().to_path_buf();
    let (_, ev) = run(&root, &opts())?;
    let nm = ev
        .summary_of(&root.join("web/node_modules"))
        .expect("node_modules summary");
    let sizes = estimate(
        nm,
        &EstimateContext::new(VolumeFs::Btrfs, Snapshots::Present),
    );

    let example = include_str!("../../../schemas/v1/examples/candidate.json");
    let mut candidate: Candidate = serde_json::from_str(example)?;
    candidate.sizes = sizes;
    let json = serde_json::to_value(&candidate)?;
    let reclaimable = &json["sizes"]["reclaimable"];
    assert_eq!(reclaimable["confidence"], "low");
    assert_eq!(reclaimable["bytes"], sizes.reclaimable.bytes);
    assert_eq!(reclaimable["shared_bytes"], sizes.reclaimable.shared_bytes);
    assert_eq!(json["sizes"]["allocated"], sizes.allocated);
    assert_eq!(json["sizes"]["logical"], sizes.logical);
    eprintln!("{}", serde_json::to_string_pretty(&json["sizes"])?);

    // Every confidence level round-trips with its snake_case name.
    for (c, name) in [
        (Confidence::High, "high"),
        (Confidence::Medium, "medium"),
        (Confidence::Low, "low"),
        (Confidence::Unknown, "unknown"),
    ] {
        candidate.sizes.reclaimable.confidence = c;
        let text = serde_json::to_string(&candidate)?;
        assert!(
            text.contains(&format!("\"confidence\":\"{name}\"")),
            "{text}"
        );
        let back: Candidate = serde_json::from_str(&text)?;
        assert_eq!(back, candidate);
    }
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
    Ok(())
}
