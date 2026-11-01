//! End-to-end FX-PHY fixtures (docs/FIXTURES.md §3.6): predicted reclaim from the real scanner
//! and estimator on the fixture's real volume, deletion through the real Executor, the
//! volume's measured free-space delta, and an audit store beside the root holding predicted and
//! measured bytes; every invariant that has an input runs (I-1…I-6; nothing is spawned by the
//! code under test, so I-7 has no records, and nothing is revalidated, so I-8 is n/a).
//!
//! Free space is volume-wide and noisy (other processes, APFS purgeable space, delayed
//! frees), so these fixtures require the measured delta to be **recorded and explained**,
//! not equal to the prediction. Agreement within tolerance on quiet volumes is
//! `crates/scull-cleaner/tests/measure.rs`, which measures on a private tmpfs where available.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod exec;

use exec::{Measured, delete_measured};
use scull_fixtures::{Fixture, InvariantId, InvariantReport, Outcome};
use scull_scanner::EstimateNote;

const MIB: u64 = 1024 * 1024;

fn check(fx: &Fixture, m: &Measured) -> InvariantReport {
    let report = fx.check_invariants(&m.inputs());
    println!("{report}");
    report.assert_ok();
    for id in [
        InvariantId::I1,
        InvariantId::I2,
        InvariantId::I3,
        InvariantId::I4,
        InvariantId::I5,
    ] {
        assert_eq!(report.outcome(id), &Outcome::Passed, "{id}: {report}");
    }
    assert!(report.outcome(InvariantId::I6).ran(), "{report}");
    report
}

/// FX-PHY-01: a pnpm project whose `node_modules` files are hard links into a store outside
/// it. Predicted reclaim ≈ 0 with the "shared with links" explanation; the deletion removes
/// only links, the store keeps every byte, and the measured delta is recorded beside the
/// prediction.
#[test]
fn fx_phy_01_pnpm_hardlinked_node_modules_predicts_zero_measured_recorded() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("app", "app", scull_fixtures::NodeLock::Pnpm)
        .unwrap();
    for i in 0..8 {
        fx.file_of_size(format!("store/v10/files/{i:02}/blob"), MIB)
            .unwrap()
            .hardlink(
                format!("app/node_modules/.pnpm/pkg{i}@1.0.0/node_modules/pkg{i}/index.js"),
                format!("store/v10/files/{i:02}/blob"),
            )
            .unwrap();
    }
    fx.checkpoint().unwrap();

    let m = delete_measured(&fx, "app/node_modules");
    let p = &m.prediction;
    assert!(
        p.allocated >= 8 * MIB,
        "scanned {} B allocated",
        p.allocated
    );
    // Only the directories' own blocks are unique to the item.
    assert!(
        p.reclaim.freed_now < MIB,
        "predicted {} B for hard links into the store",
        p.reclaim.freed_now
    );
    assert!(
        p.notes
            .iter()
            .any(|n| matches!(n, EstimateNote::SharedWithLinks { bytes } if *bytes >= 8 * MIB)),
        "{:?}",
        p.notes
    );
    assert!(!fx.root().join("app/node_modules").exists());
    for i in 0..8 {
        let blob = fx.root().join(format!("store/v10/files/{i:02}/blob"));
        assert_eq!(
            std::fs::metadata(&blob).unwrap().len(),
            MIB,
            "store keeps its data"
        );
    }
    let measured = m.delta.measured_total.expect("free space measured");
    println!(
        "FX-PHY-01: predicted {} B, removed {} B (allocated, links only), measured {measured:+} B",
        p.reclaim.freed_now, m.delta.removed
    );
    check(&fx, &m);
}

/// FX-PHY-02 (macOS): an APFS clone (`cp -c`) of a large file inside the candidate. The
/// estimate's confidence is "low" with the clones explanation; after deletion the measured
/// delta is recorded and explained (`clones_possible`), whatever it is.
#[test]
#[cfg(target_os = "macos")]
fn fx_phy_02_apfs_clone_low_confidence_measured_and_explained() {
    use scull_core::Confidence;
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("keep/original.bin", 16 * MIB).unwrap();
    fx.dir("cand").unwrap();
    let st = std::process::Command::new("/bin/cp")
        .arg("-c")
        .arg(fx.root().join("keep/original.bin"))
        .arg(fx.root().join("cand/clone.bin"))
        .status()
        .unwrap();
    assert!(st.success(), "cp -c failed: the temp volume is not APFS?");
    fx.checkpoint().unwrap();

    let m = delete_measured(&fx, "cand");
    let p = &m.prediction;
    assert_eq!(p.confidence, Confidence::Low, "{:?}", p.notes);
    assert!(
        p.notes
            .iter()
            .any(|n| matches!(n, EstimateNote::ClonesPossible(_))),
        "{:?}",
        p.notes
    );
    assert!(m.delta.measured_total.is_some(), "{}", m.delta);
    let codes = m.delta.explanation_codes();
    assert!(codes.contains(&"clones_possible"), "{codes:?}");
    assert!(fx.root().join("keep/original.bin").is_file());
    check(&fx, &m);
}

/// Compressible content: compresses well under NTFS and APFS compression.
#[cfg(any(windows, target_os = "macos"))]
fn compressible(fx: &mut Fixture, rel: &str) {
    let line = b"scull FX-PHY-05 compressible line of text, repeated many times over.\n";
    let body: Vec<u8> = line
        .iter()
        .copied()
        .cycle()
        .take(8 * MIB as usize)
        .collect();
    fx.file(rel, body).unwrap();
}

/// The FX-PHY-05 assertions: the scan sees allocated well below logical, and the prediction
/// uses allocated bytes (with the compression note), never logical.
#[cfg(any(windows, target_os = "macos"))]
fn assert_allocated_used(fx: &Fixture, m: &Measured) {
    let p = &m.prediction;
    println!(
        "FX-PHY-05: logical {} B, allocated {} B, predicted {} B",
        p.logical, p.allocated, p.reclaim.freed_now
    );
    assert!(
        p.allocated < p.logical / 2,
        "compression not visible in allocated size: {} of {}",
        p.allocated,
        p.logical
    );
    assert!(p.reclaim.freed_now <= p.allocated, "{:?}", p.reclaim);
    assert!(
        p.notes
            .iter()
            .any(|n| matches!(n, EstimateNote::CompressedOrSparse { .. })),
        "{:?}",
        p.notes
    );
    assert!(m.delta.measured_total.is_some(), "{}", m.delta);
    check(fx, m);
}

/// FX-PHY-05 (Windows): an NTFS-compressed file (`compact /c`) is sized and predicted by its
/// allocated (compressed) size.
#[test]
#[cfg(windows)]
fn fx_phy_05_ntfs_compressed_file_uses_allocated_size() {
    let mut fx = Fixture::new().unwrap();
    compressible(&mut fx, "cand/text.log");
    let st = std::process::Command::new("compact")
        .args(["/c", "/q"])
        .arg(fx.root().join("cand/text.log"))
        .status()
        .unwrap();
    assert!(
        st.success(),
        "compact /c failed: the temp volume does not support compression?"
    );
    fx.checkpoint().unwrap();
    let m = delete_measured(&fx, "cand");
    assert_allocated_used(&fx, &m);
}

/// FX-PHY-05 (macOS): an APFS-compressed file (`ditto --hfsCompression`) is sized and
/// predicted by its allocated (compressed) size.
#[test]
#[cfg(target_os = "macos")]
fn fx_phy_05_apfs_compressed_file_uses_allocated_size() {
    let mut fx = Fixture::new().unwrap();
    compressible(&mut fx, "keep/text.log");
    fx.dir("cand").unwrap();
    let st = std::process::Command::new("/usr/bin/ditto")
        .arg("--hfsCompression")
        .arg(fx.root().join("keep/text.log"))
        .arg(fx.root().join("cand/text.log"))
        .status()
        .unwrap();
    assert!(st.success(), "ditto --hfsCompression failed");
    fx.checkpoint().unwrap();
    let m = delete_measured(&fx, "cand");
    assert_allocated_used(&fx, &m);
}
