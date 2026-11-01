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

use exec::{Measured, delete_measured, delete_measured_n};
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
/// delta is recorded, and a shortfall beyond the threshold is explained (`clones_possible`).
/// Explanations are for disagreements: when the busy CI volume's free space happens to move by
/// about the predicted amount there is nothing to explain.
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
    let measured = m.delta.measured_total.expect("free space measured");
    let codes = m.delta.explanation_codes();
    let shortfall = i128::from(measured) < i128::from(p.reclaim.freed_now);
    if m.delta.exceeds_threshold && !m.delta.within_resolution && shortfall {
        assert!(codes.contains(&"clones_possible"), "{codes:?}\n{}", m.delta);
    } else {
        println!("FX-PHY-02: measured {measured:+} B agrees with the prediction: {codes:?}");
    }
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

/// The G3 reference corpus under `r/`: five realistic items (see
/// [`fx_phy_06_reference_corpus_median_prediction_error_within_10_percent`]).
fn corpus(fx: &mut Fixture, r: &str) {
    const KIB: u64 = 1024;
    // 1. A Rust target dir with large artifacts (rlibs, a binary).
    fx.cargo_project(format!("{r}/api"), "api").unwrap();
    for i in 0..24 {
        fx.file_of_size(
            format!("{r}/api/target/debug/deps/libdep{i:02}.rlib"),
            768 * KIB,
        )
        .unwrap();
    }
    fx.file_of_size(format!("{r}/api/target/debug/api"), 6 * MIB)
        .unwrap();
    // 2. A Rust target dir with thousands of small incremental files (block rounding).
    fx.cargo_project(format!("{r}/cli"), "cli").unwrap();
    for i in 0..2400 {
        fx.file_of_size(
            format!(
                "{r}/cli/target/debug/incremental/cli-{:02}/s-{i:04}.bin",
                i % 40
            ),
            1 + (i * 37) % (6 * KIB),
        )
        .unwrap();
    }
    // 3. npm node_modules: many packages, many small files.
    fx.npm_project(format!("{r}/web"), "web", scull_fixtures::NodeLock::Npm)
        .unwrap();
    for p in 0..120 {
        for f in 0..14 {
            fx.file_of_size(
                format!("{r}/web/node_modules/pkg{p:03}/lib/f{f:02}.js"),
                512 + ((p * 14 + f) * 97) % (12 * KIB),
            )
            .unwrap();
        }
    }
    // 4. A mixed target: a few big files and a deep tree of medium ones.
    fx.cargo_project(format!("{r}/game"), "game").unwrap();
    for i in 0..6 {
        fx.file_of_size(
            format!("{r}/game/target/release/assets/pack{i}.bin"),
            4 * MIB,
        )
        .unwrap();
    }
    for i in 0..300 {
        fx.file_of_size(
            format!("{r}/game/target/release/build/b{:02}/out/o{i:03}.o", i % 30),
            40 * KIB + (i * 131) % (64 * KIB),
        )
        .unwrap();
    }
    // 5. pnpm node_modules hard-linked into a store outside it (predicted ≈ 0).
    fx.npm_project(format!("{r}/mono"), "mono", scull_fixtures::NodeLock::Pnpm)
        .unwrap();
    for i in 0..16 {
        fx.file_of_size(format!("{r}/store/v10/files/{i:02}/blob"), MIB)
            .unwrap()
            .hardlink(
                format!("{r}/mono/node_modules/.pnpm/p{i}@1.0.0/node_modules/p{i}/index.js"),
                format!("{r}/store/v10/files/{i:02}/blob"),
            )
            .unwrap();
    }
}

/// FX-PHY-06 (nightly, G3): a reference corpus of realistic build outputs and dependency
/// folders, each deleted through the real Executor on this machine's own volume, one item at a
/// time, with the free-space delta measured around each deletion. The corpus is built three
/// times and each item's measurement is the median of its three runs, so one disturbed sample
/// (volume-wide noise: other processes, APFS purgeable space, delayed frees) doesn't decide.
/// The **median prediction error** over the items whose prediction is at least 1 MiB must be
/// at most 10% (G3); the hard-linked pnpm folder, predicted at about zero, must measure about
/// zero (within the volume's resolution and observed noise). Every run keeps predicted and
/// measured bytes in its operation record (I-5) and every invariant runs.
#[test]
#[ignore = "nightly: writes and measures a ~300 MiB corpus on the real volume"]
fn fx_phy_06_reference_corpus_median_prediction_error_within_10_percent() {
    const ROUNDS: [&str; 3] = ["r1", "r2", "r3"];
    const ITEMS: [&str; 5] = [
        "api/target",
        "cli/target",
        "web/node_modules",
        "game/target",
        "mono/node_modules",
    ];
    let mut fx = Fixture::new().unwrap();
    for r in ROUNDS {
        corpus(&mut fx, r);
    }
    fx.checkpoint().unwrap();

    let mut runs = Vec::new();
    // item -> (predicted, measured, noise) per round
    let mut samples: Vec<Vec<(u64, i64, u64)>> = vec![Vec::new(); ITEMS.len()];
    let mut n = 0;
    for r in ROUNDS {
        for (k, item) in ITEMS.iter().enumerate() {
            n += 1;
            let rel = format!("{r}/{item}");
            let m = delete_measured_n(&fx, &rel, n);
            assert!(!fx.root().join(&rel).exists(), "{rel} deleted");
            let predicted = m.prediction.reclaim.freed_now;
            let measured = m.delta.measured_total.expect("free space measured");
            let noise = m
                .delta
                .per_volume
                .iter()
                .map(|v| v.noise)
                .max()
                .unwrap_or(0)
                .max(m.delta.observations.resolution);
            println!(
                "FX-PHY-06 {rel}: predicted {predicted} B, removed {} B, measured {measured:+} B \
                 (noise/resolution {noise} B)",
                m.delta.removed
            );
            samples[k].push((predicted, measured, noise));
            runs.push(m);
        }
    }
    let median = |mut v: Vec<i64>| {
        v.sort_unstable();
        v[v.len() / 2]
    };
    // A volume busy with other writers makes free space meaningless as a measurement. An
    // item's median is trusted when at least two of its three runs agree within 10% of the
    // prediction (one disturbed run, e.g. activity right after the job starts, is outvoted).
    // When two or more items have no such agreement, say so (a SKIPPED line, which the MVP
    // gate counts as a failure) instead of judging G3.
    let disturbed = (0..ITEMS.len())
        .filter(|&k| {
            let predicted = samples[k][0].0;
            let mut m: Vec<i64> = samples[k].iter().map(|s| s.1).collect();
            m.sort_unstable();
            let close = |a: i64, b: i64| (b - a).unsigned_abs() <= predicted / 10;
            predicted >= MIB && !(close(m[0], m[1]) || close(m[1], m[2]))
        })
        .count();
    let mut inputs = scull_fixtures::InvariantInputs::new();
    for m in &runs {
        inputs = inputs
            .plan([(m.item.item_id.clone(), m.item.tier, m.item.action.clone())])
            .allow(&m.rel)
            .audit(&m.store, &m.op)
            .operation_bytes(&m.store, &m.op);
    }
    if disturbed >= 2 {
        println!(
            "SKIPPED fx_phy_06_reference_corpus_median_prediction_error_within_10_percent: the \
             volume was too busy to measure ({disturbed} items have no two runs within 10%); rerun \
             on a quiet volume"
        );
        fx.check_invariants(&inputs).assert_ok();
        return;
    }
    let mut errors = Vec::new();
    for (k, item) in ITEMS.iter().enumerate() {
        let predicted = samples[k][0].0;
        let measured = median(samples[k].iter().map(|s| s.1).collect());
        if predicted >= MIB {
            #[allow(clippy::cast_precision_loss)]
            let err = (measured - i64::try_from(predicted).unwrap()).unsigned_abs() as f64
                / predicted as f64;
            println!(
                "FX-PHY-06 {item}: predicted {predicted} B, median measured {measured:+} B, error {:.2}%",
                err * 100.0
            );
            errors.push(err);
        } else {
            // Hard links into the store: nothing (but directory blocks) is freed. The tolerance
            // is the volume's resolution and observed noise, or the spread of the three runs
            // (other writers on the volume), whichever is larger.
            let lo = samples[k].iter().map(|s| s.1).min().unwrap_or(0);
            let hi = samples[k].iter().map(|s| s.1).max().unwrap_or(0);
            let spread = (hi - lo).unsigned_abs();
            let noise = samples[k]
                .iter()
                .map(|s| s.2)
                .max()
                .unwrap_or(0)
                .max(spread);
            println!("FX-PHY-06 {item}: predicted {predicted} B, median measured {measured:+} B");
            assert!(
                measured.unsigned_abs() <= MIB.max(noise),
                "{item}: predicted {predicted} B, median measured {measured:+} B (noise {noise} B)"
            );
        }
    }
    errors.sort_by(f64::total_cmp);
    let med = errors[errors.len() / 2];
    println!(
        "FX-PHY-06: median prediction error {:.2}% over {} items (G3: <= 10%)",
        med * 100.0,
        errors.len()
    );
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(
        med <= 0.10,
        "median prediction error {med} > 10%: {errors:?}"
    );
    for r in ROUNDS {
        for i in 0..16 {
            let blob = fx.root().join(format!("{r}/store/v10/files/{i:02}/blob"));
            assert_eq!(std::fs::metadata(&blob).unwrap().len(), MIB, "store intact");
        }
    }
    let report = fx.check_invariants(&inputs);
    println!("{report}");
    report.assert_ok();
    assert_eq!(
        report.outcome(InvariantId::I5),
        &Outcome::Passed,
        "{report}"
    );
}
