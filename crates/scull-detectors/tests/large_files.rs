//! Large-files inspector fixtures (P1-DET-05): top-N by allocated size with kind hints,
//! FX-FS-06 (sparse file reported by allocated size), links never followed, hard links listed
//! once, and inspect-only candidates that no plan accepts. Universal invariants (I-2 canary,
//! I-3 tree unchanged) are checked after each test.

#![allow(clippy::unwrap_used)]

mod common;

use common::{check, rel};
use scull_core::{
    Action, Category, ErrorKind, Evidence, PlanId, PlanItem, PlanOrigin, PlanParams, ReasonCode,
    ScanId, Tier,
};
use scull_detectors::large_files::{LARGE_FILE_RULE_ID, LargeFileReport};
use scull_detectors::{FileKind, LargeFileOptions, inspect_large_files};
use scull_fixtures::{Fixture, InvariantInputs, LinkOutcome, days_ago};
use scull_platform::{Native, Platform};
use scull_scanner::{CancelToken, ScanOptions};
use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

/// Size unit for the fixtures (kept small: fixture files are generated and hashed).
const UNIT: u64 = 256 * 1024;

fn inspect(dir: &Path, threshold: u64, top: usize) -> LargeFileReport {
    let (report, outcome) = inspect_large_files(
        Arc::new(Native::new()),
        dir,
        LargeFileOptions { threshold, top },
        &ScanOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(!outcome.is_cancelled());
    report
}

fn listed(fx: &Fixture, r: &LargeFileReport) -> Vec<String> {
    r.files.iter().map(|f| rel(fx.root(), &f.path)).collect()
}

fn plan_params() -> PlanParams {
    PlanParams {
        id: PlanId::from_suffix("01J").unwrap(),
        scan_id: ScanId::from_suffix("01J").unwrap(),
        created_unix: 1_800_000_000,
        ttl_secs: None,
        machine_id: "machine-a".into(),
        user: "me".into(),
        origin: PlanOrigin::Cli,
        operations_watermark: None,
    }
}

fn check_clean(fx: &Fixture) {
    check(fx, &InvariantInputs::new());
}

fn skip(fx: &mut Fixture) {
    eprintln!("link not created here; skipped: {:?}", fx.skipped());
    fx.checkpoint().unwrap();
    check_clean(fx);
}

#[test]
fn lists_top_n_by_allocated_size_with_kind_hints() {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("data/vms/ubuntu.vmdk", 3 * UNIT)
        .unwrap()
        .file_of_size("data/isos/ubuntu-24.04.ISO", 5 * UNIT / 2)
        .unwrap()
        .file_of_size("data/models/llama.gguf", 2 * UNIT)
        .unwrap()
        .file_of_size("data/backup.tar.gz", 3 * UNIT / 2)
        .unwrap()
        .file_of_size("data/notes.txt", 10 * 1024)
        .unwrap()
        .set_mtime("data/vms/ubuntu.vmdk", days_ago(10))
        .unwrap()
        .checkpoint()
        .unwrap();
    let r = inspect(&fx.path("data").unwrap(), UNIT, 3);
    assert_eq!(
        listed(&fx, &r),
        [
            "data/vms/ubuntu.vmdk",
            "data/isos/ubuntu-24.04.ISO",
            "data/models/llama.gguf"
        ]
    );
    assert_eq!(r.matched, 4, "the archive matched but is past the top 3");
    assert!(r.truncated());
    let kinds: Vec<FileKind> = r.files.iter().map(|f| f.kind).collect();
    assert_eq!(
        kinds,
        [
            FileKind::VmDisk,
            FileKind::DiskImage,
            FileKind::ModelWeights
        ]
    );
    let ten_days = days_ago(10).duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((r.files[0].mtime.unwrap() - ten_days).abs() <= 2);
    for f in &r.files {
        assert!(f.size() >= UNIT);
        assert_eq!(
            f.allocated.is_some(),
            Native::new().capabilities().allocated_size
        );
    }

    let all = inspect(&fx.path("data").unwrap(), UNIT, 10);
    assert_eq!(all.files.len(), 4);
    assert_eq!(all.files[3].kind, FileKind::Archive);
    assert!(!all.truncated());
    assert_eq!(
        all.matched_bytes,
        all.files.iter().map(|f| f.size()).sum::<u64>()
    );
    check_clean(&fx);
}

#[test]
fn candidates_are_inspect_only_and_never_planned() {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("d/disk.qcow2", 3 * UNIT)
        .unwrap()
        .file_of_size("d/blob", 2 * UNIT)
        .unwrap()
        .checkpoint()
        .unwrap();
    let cands = inspect(&fx.path("d").unwrap(), UNIT, 10).into_candidates();
    assert_eq!(cands.len(), 2);
    for c in &cands {
        assert_eq!(c.category, Category::LargeFile);
        assert_eq!(c.rule.id, LARGE_FILE_RULE_ID);
        assert_eq!(c.tier, Tier::S2);
        assert_eq!(c.action, Action::None, "inspect-only, always");
        assert_eq!(
            c.reasons.iter().map(|r| r.code).collect::<Vec<_>>(),
            [ReasonCode::InspectOnly]
        );
        assert!(c.is_consistent());
        assert!(c.sizes.allocated > 0 && c.sizes.logical >= 2 * UNIT);
        assert!(
            c.evidence
                .iter()
                .any(|e| matches!(e, Evidence::Note { text } if text.contains("never read")))
        );
        assert!(
            c.evidence
                .iter()
                .any(|e| matches!(e, Evidence::LastModified { .. }))
        );
        let err = PlanItem::from_candidate(c).unwrap_err();
        assert_eq!(err.kind, ErrorKind::SafetyBlocked);
        // Even a hand-built plan item for it is refused by `Plan::new`.
        let forced = PlanItem {
            item_id: c.id.clone(),
            identity: c.identity,
            path: c.path.clone(),
            category: c.category,
            rule: c.rule.clone(),
            tier: c.tier,
            action: c.action.clone(),
            reclaimable_bytes: c.sizes.reclaimable.bytes,
        };
        let err = scull_core::Plan::new(plan_params(), vec![forced]).unwrap_err();
        assert_eq!(err.kind, ErrorKind::SafetyBlocked);
    }
    assert_eq!(cands[0].artifact, "VM disk image");
    assert_eq!(cands[1].artifact, "Large file");
    check_clean(&fx);
}

#[test]
fn fx_fs_06_sparse_file_reported_by_allocated_size() {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("d/dense.img", 2 * UNIT).unwrap();
    // A sparse file: 16 MiB long, 16 KiB written (tests may use std::fs inside the fixture).
    let sparse = fx.path("d/sparse.vhd").unwrap();
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&sparse).unwrap();
        f.write_all(&[7u8; 16 * 1024]).unwrap();
        f.set_len(16 * 1024 * 1024).unwrap();
    }
    fx.checkpoint().unwrap();
    let md = Native::new().metadata(&sparse).unwrap();
    assert_eq!(md.logical_size, 16 * 1024 * 1024);
    let allocated = md.allocated_size.unwrap_or(md.logical_size);

    let r = inspect(&fx.path("d").unwrap(), UNIT, 10);
    let sparse_listed = r.files.iter().any(|f| f.path == sparse);
    assert_eq!(
        sparse_listed,
        allocated >= UNIT,
        "listed by allocated size ({allocated} bytes), not its 16 MiB length"
    );
    if allocated < md.logical_size {
        assert!(
            !sparse_listed,
            "a sparse file is ranked by what it occupies"
        );
    }
    assert!(
        r.files
            .iter()
            .any(|f| rel(fx.root(), &f.path) == "d/dense.img")
    );

    // With no threshold it is listed, with its allocated (not logical) size.
    let all = inspect(&fx.path("d").unwrap(), 0, 10);
    let c = all
        .into_candidates()
        .into_iter()
        .find(|c| c.path.to_path_buf() == sparse)
        .unwrap();
    assert_eq!(c.sizes.logical, 16 * 1024 * 1024);
    assert_eq!(c.sizes.allocated, allocated);
    check_clean(&fx);
}

#[test]
fn symlink_to_huge_file_is_not_followed() {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("outside/huge.iso", 3 * UNIT)
        .unwrap()
        .file("scan/small.txt", "x")
        .unwrap();
    let target = fx.path("outside/huge.iso").unwrap();
    if fx.symlink("scan/link.iso", &target).unwrap() != LinkOutcome::Created {
        return skip(&mut fx);
    }
    fx.checkpoint().unwrap();
    let r = inspect(&fx.path("scan").unwrap(), UNIT, 10);
    assert!(r.files.is_empty(), "{:?}", r.files);
    assert_eq!(r.matched, 0);
    // From the common parent, the real file is listed once and the link not at all.
    let r = inspect(fx.root(), UNIT, 10);
    assert_eq!(listed(&fx, &r), ["outside/huge.iso"]);
    check_clean(&fx);
}

#[test]
fn hard_linked_file_is_listed_once() {
    let mut fx = Fixture::new().unwrap();
    fx.file_of_size("d/a.img", 2 * UNIT)
        .unwrap()
        .hardlink("d/b.img", "d/a.img")
        .unwrap()
        .checkpoint()
        .unwrap();
    let r = inspect(&fx.path("d").unwrap(), UNIT, 10);
    let caps = Native::new().capabilities();
    if caps.file_identity && caps.link_count {
        assert_eq!(r.files.len(), 1, "{:?}", r.files);
        assert_eq!(r.matched, 1);
        let c = r.into_candidates().remove(0);
        assert_eq!(c.flags.hardlinked, Some(true));
        assert!(
            c.evidence
                .iter()
                .any(|e| matches!(e, Evidence::Hardlinked { .. }))
        );
    }
    check_clean(&fx);
}

#[test]
fn memory_is_bounded_by_top_n() {
    let mut fx = Fixture::new().unwrap();
    for i in 0..200u64 {
        fx.file_of_size(format!("many/{}/f{i}.bin", i % 10), 1024 + i)
            .unwrap();
    }
    fx.checkpoint().unwrap();
    let r = inspect(&fx.path("many").unwrap(), 0, 5);
    assert_eq!(r.files.len(), 5);
    assert_eq!(r.matched, 200);
    let logical: Vec<u64> = r.files.iter().map(|f| f.logical).collect();
    // The largest by allocated size; logical breaks no ties here, but all five are from the top.
    assert!(logical.iter().all(|l| *l >= 1024), "{logical:?}");
    let sizes: Vec<u64> = r.files.iter().map(|f| f.size()).collect();
    assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "largest first");
    check_clean(&fx);
}
