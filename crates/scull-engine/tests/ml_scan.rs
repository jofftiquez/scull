//! `scull scan` picks up the bundled ML cache rules (P4-DET-01): FX-CLS-13 end to end through the
//! engine. Hugging Face models are S1, a dataset without a recorded source is S2, Ollama models
//! are S1 with the `ollama rm` tool command (never run by a scan), and the PyTorch Hub cache is
//! S1. The scan changes nothing (I-1/I-2/I-3).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scull_core::config::Config;
use scull_core::{Action, Candidate, Category, ReasonCode, Tier};
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, FixedInUse, InUseSource, ScanReport, ScanRequest, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs, PlanEntry};
use scull_platform::Native;
use scull_safety::Tri;
use std::path::Path;
use std::sync::Arc;

const THREE_DAYS: i64 = 3 * 24 * 3600;

fn hf_repo(fx: &mut Fixture, dir: &str, refs: bool) {
    fx.file_of_size(format!("{dir}/blobs/a1b2c3"), 8192)
        .unwrap()
        .file(format!("{dir}/snapshots/0123abcd/config.json"), "{}")
        .unwrap();
    if refs {
        fx.file(format!("{dir}/refs/main"), "0123abcd").unwrap();
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn at<'a>(r: &'a ScanReport, root: &Path, rel_path: &str) -> &'a Candidate {
    let hits: Vec<&Candidate> = r
        .candidates
        .iter()
        .filter(|c| rel(root, &c.path.to_path_buf()) == rel_path)
        .collect();
    assert_eq!(hits.len(), 1, "{rel_path}: {:#?}", r.candidates);
    hits[0]
}

#[test]
fn scan_picks_up_ml_cache_rules() {
    let mut fx = Fixture::new().unwrap();
    hf_repo(
        &mut fx,
        "cache/huggingface/hub/models--google--gemma-2b",
        true,
    );
    hf_repo(&mut fx, "cache/huggingface/hub/datasets--team--eval", false);
    fx.file_of_size("ollama/models/blobs/sha256-aaaa", 16384)
        .unwrap()
        .file(
            "ollama/models/manifests/registry.ollama.ai/library/llama3.2/latest",
            "{}",
        )
        .unwrap()
        .file_of_size("cache/torch/hub/checkpoints/resnet18.pth", 8192)
        .unwrap()
        .checkpoint()
        .unwrap();
    let root = canon(fx.root());

    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(Tri::No)));
    let e = Engine::new(cfg).unwrap();
    let r = e
        .scan(
            std::slice::from_ref(&root),
            &ScanRequest::full()
                .history(false)
                .at(now_unix() + THREE_DAYS),
            &CancelToken::new(),
            |_| {},
        )
        .unwrap();

    let mut paths: Vec<String> = r
        .candidates
        .iter()
        .map(|c| rel(&root, &c.path.to_path_buf()))
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            "cache/huggingface/hub/datasets--team--eval",
            "cache/huggingface/hub/models--google--gemma-2b",
            "cache/torch/hub",
            "ollama/models",
        ]
    );
    let model = at(&r, &root, "cache/huggingface/hub/models--google--gemma-2b");
    assert_eq!(model.category, Category::MlCache);
    assert_eq!(model.tier, Tier::S1, "{model:#?}");
    assert_eq!(model.action, Action::Quarantine);
    assert!(model.project.is_none());
    let dataset = at(&r, &root, "cache/huggingface/hub/datasets--team--eval");
    assert_eq!(dataset.tier, Tier::S2, "{dataset:#?}");
    assert!(
        dataset
            .reasons
            .iter()
            .any(|x| x.code == ReasonCode::NoLockfile)
    );
    let ollama = at(&r, &root, "ollama/models");
    assert_eq!(ollama.tier, Tier::S1, "{ollama:#?}");
    assert_eq!(
        ollama.action,
        Action::ToolCommand {
            tool: "ollama".into(),
            args: vec!["rm".into(), "llama3.2:latest".into()],
        }
    );
    assert_eq!(at(&r, &root, "cache/torch/hub").tier, Tier::S1);

    for c in &r.candidates {
        assert!(c.is_consistent(), "{c:#?}");
    }
    let plan: Vec<PlanEntry> = r
        .candidates
        .iter()
        .filter(|c| c.action != Action::None)
        .map(PlanEntry::from)
        .collect();
    fx.check_invariants(&InvariantInputs::new().plan(plan))
        .assert_ok();
}
