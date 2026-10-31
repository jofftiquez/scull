//! ML / AI cache fixtures (P4-DET-01): FX-CLS-13 (Hugging Face models S1, datasets without a
//! recorded source S2, Ollama models S1), the `datasets` library cache (S2), the PyTorch Hub
//! cache (S1), marker gating, and the per-OS locator (`locate_ml_caches`) with every OS layout
//! checked on every OS through an injected `CacheEnv`. Every test builds its tree with
//! `scull-fixtures`, detects read-only, and checks the universal invariants (I-2 canary, I-3 tree
//! unchanged, I-1 on the would-be plan). Nothing runs `ollama` or reads the real home folder.

#![allow(clippy::unwrap_used)]

mod common;

use common::{age_tree, check, classify_expect, clean_caller, plan_inputs, rel, scan};
use scull_core::{Action, Category, Evidence, ReasonCode, Tier};
use scull_detectors::caches::CacheSource;
use scull_detectors::rules::Platform as Os;
use scull_detectors::{CacheEnv, DetectedArtifact, MlRootKind, MlScan, locate_ml_caches};
use scull_fixtures::{Fixture, days_ago};
use scull_platform::Native;
use scull_safety::Regen;

const ALL_OS: [Os; 3] = [Os::Linux, Os::Macos, Os::Windows];
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn notes(a: &DetectedArtifact) -> Vec<&str> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A hub repository as `huggingface_hub` lays it out: content-addressed blobs, a snapshot per
/// revision (links into the blobs where the platform allows them, as on Linux/macOS), and
/// `refs/main` recording the revision unless `refs` is false.
fn hf_repo(fx: &mut Fixture, dir: &str, refs: bool) {
    fx.file_of_size(format!("{dir}/blobs/a1b2c3d4"), 8192)
        .unwrap()
        .file(format!("{dir}/blobs/e5f6"), "{\"model_type\":\"bert\"}")
        .unwrap()
        .file(format!("{dir}/snapshots/{COMMIT}/config.json"), "{}")
        .unwrap();
    fx.symlink(
        format!("{dir}/snapshots/{COMMIT}/model.safetensors"),
        "../../blobs/a1b2c3d4",
    )
    .unwrap();
    if refs {
        fx.file(format!("{dir}/refs/main"), COMMIT).unwrap();
    }
}

/// An Ollama store with `models` as `(registry, namespace, model, tag)`.
fn ollama_store(fx: &mut Fixture, dir: &str, models: &[(&str, &str, &str, &str)]) {
    fx.file_of_size(format!("{dir}/blobs/sha256-aaaa"), 16384)
        .unwrap()
        .file(format!("{dir}/blobs/sha256-bbbb"), "{}")
        .unwrap();
    fx.dir(format!("{dir}/manifests")).unwrap();
    for (host, ns, model, tag) in models {
        fx.file(
            format!("{dir}/manifests/{host}/{ns}/{model}/{tag}"),
            "{\"schemaVersion\":2,\"layers\":[]}",
        )
        .unwrap();
    }
}

fn datasets_cache(fx: &mut Fixture, dir: &str) {
    fx.file_of_size(format!("{dir}/downloads/9f8e7d.incomplete_ok"), 4096)
        .unwrap()
        .file(format!("{dir}/downloads/9f8e7d.json"), "{\"url\":null}")
        .unwrap()
        .file_of_size(
            format!("{dir}/rajpurkar___squad/plain_text/0.0.0/abc/squad-train.arrow"),
            8192,
        )
        .unwrap();
}

fn torch_hub(fx: &mut Fixture, dir: &str, trusted: bool) {
    fx.file_of_size(format!("{dir}/checkpoints/resnet18-f37072fd.pth"), 8192)
        .unwrap()
        .file(
            format!("{dir}/pytorch_vision_main/hubconf.py"),
            "dependencies = []\n",
        )
        .unwrap();
    if trusted {
        fx.file(format!("{dir}/trusted_list"), "pytorch_vision\n")
            .unwrap();
    }
}

fn locate(env: &CacheEnv) -> MlScan {
    locate_ml_caches(env, &Native::new())
}

fn env(fx: &Fixture, os: Os, vars: &[(&str, String)]) -> CacheEnv {
    CacheEnv::with_vars(
        fx.root().join("home"),
        os,
        vars.iter().map(|(k, v)| (*k, v.clone())),
    )
}

fn abs(fx: &Fixture, rel_path: &str) -> String {
    fx.path(rel_path).unwrap().to_string_lossy().into_owned()
}

fn found_rel(fx: &Fixture, scan: &MlScan) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = scan
        .found
        .iter()
        .map(|a| (a.rule.id.clone(), rel(fx.root(), &a.path)))
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-13: Hugging Face cache and Ollama models

#[test]
fn fx_cls_13_hf_models_s1_datasets_without_recorded_source_s2() {
    let mut fx = Fixture::new().unwrap();
    let hub = "home/.cache/huggingface/hub";
    hf_repo(
        &mut fx,
        &format!("{hub}/models--google-bert--bert-base-uncased"),
        true,
    );
    hf_repo(
        &mut fx,
        &format!("{hub}/models--acme--copied-by-hand"),
        false,
    );
    hf_repo(&mut fx, &format!("{hub}/datasets--rajpurkar--squad"), true);
    hf_repo(
        &mut fx,
        &format!("{hub}/datasets--team--internal-eval"),
        false,
    );
    fx.file(format!("{hub}/version.txt"), "1").unwrap();
    fx.dir(format!(
        "{hub}/.locks/models--google-bert--bert-base-uncased"
    ))
    .unwrap();
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        [
            format!("{hub}/datasets--rajpurkar--squad"),
            format!("{hub}/datasets--team--internal-eval"),
            format!("{hub}/models--acme--copied-by-hand"),
            format!("{hub}/models--google-bert--bert-base-uncased"),
        ]
    );
    let want = [
        (
            "models--google-bert--bert-base-uncased",
            "huggingface.model",
            Tier::S1,
        ),
        (
            "models--acme--copied-by-hand",
            "huggingface.model",
            Tier::S2,
        ),
        (
            "datasets--rajpurkar--squad",
            "huggingface.dataset",
            Tier::S1,
        ),
        (
            "datasets--team--internal-eval",
            "huggingface.dataset",
            Tier::S2,
        ),
    ];
    let mut cands = Vec::new();
    for (name, rule, tier) in want {
        let a = s.at(&root, &format!("{hub}/{name}"));
        assert_eq!(a.rule.id, rule);
        assert_eq!(a.category, Category::MlCache);
        assert!(a.projects.is_empty(), "ML caches belong to no project");
        assert_eq!(a.signals.regen, Regen::NetworkOrLong, "re-download");
        let c = classify_expect(a, &clean_caller(a), tier);
        if tier == Tier::S2 {
            assert!(notes(a).iter().any(|n| n.contains("no `refs/` revision")));
            assert!(c.reasons.iter().any(|r| r.code == ReasonCode::NoLockfile));
        } else {
            assert!(notes(a).iter().any(|n| n.contains("(refs: main)")));
        }
        if c.tier == Tier::S1 {
            assert_eq!(c.action, Action::Quarantine, "never deleted outright");
        }
        cands.push(c);
    }
    assert!(
        notes(s.at(&root, &format!("{hub}/datasets--rajpurkar--squad")))
            .iter()
            .any(|n| n.contains("`rajpurkar/squad`"))
    );
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn fx_cls_13_ollama_models_s1_with_ollama_rm() {
    let mut fx = Fixture::new().unwrap();
    ollama_store(
        &mut fx,
        "home/.ollama/models",
        &[
            ("registry.ollama.ai", "library", "llama3.2", "latest"),
            ("registry.ollama.ai", "library", "qwen2.5-coder", "7b"),
            ("registry.ollama.ai", "acme", "helper", "v1"),
            ("hf.co", "bartowski", "gemma-2-9b-it-GGUF", "Q4_K_M"),
        ],
    );
    fx.file("home/.ollama/id_ed25519.pub", "ssh-ed25519 AAAA")
        .unwrap();
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["home/.ollama/models"]);
    let a = s.at(&root, "home/.ollama/models");
    assert_eq!(a.rule.id, "ollama.models");
    assert_eq!(a.category, Category::MlCache);
    assert!(a.activity_processes.iter().any(|p| p == "ollama"));
    let rm = Action::ToolCommand {
        tool: "ollama".into(),
        args: [
            "rm",
            "acme/helper:v1",
            "hf.co/bartowski/gemma-2-9b-it-GGUF:Q4_K_M",
            "llama3.2:latest",
            "qwen2.5-coder:7b",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect(),
    };
    assert_eq!(a.tool_action.as_ref(), Some(&rm));
    assert!(notes(a).iter().any(|n| n.contains("Not undoable")));
    let c = classify_expect(a, &clean_caller(a), Tier::S1);
    if c.tier == Tier::S1 {
        assert_eq!(
            c.action, rm,
            "tool-native: `ollama rm`, never a filesystem delete"
        );
    }
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn ollama_store_without_models_is_not_a_candidate() {
    let mut fx = Fixture::new().unwrap();
    ollama_store(&mut fx, "home/.ollama/models", &[]);
    // A `models` folder of something else that happens to hold `blobs/` and `manifests/`.
    fx.file("work/models/blobs/x", "x")
        .unwrap()
        .file("work/models/manifests/readme.md", "notes")
        .unwrap()
        .checkpoint()
        .unwrap();
    let root = fx.root().to_path_buf();
    assert!(scan(&root, &[]).found.is_empty());
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// Processed datasets and PyTorch Hub

#[test]
fn hf_datasets_cache_is_s2_and_needs_its_folder() {
    let mut fx = Fixture::new().unwrap();
    datasets_cache(&mut fx, "home/.cache/huggingface/datasets");
    datasets_cache(&mut fx, "research/datasets"); // a user's own `datasets` folder
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["home/.cache/huggingface/datasets"]);
    let a = s.at(&root, "home/.cache/huggingface/datasets");
    assert_eq!(a.rule.id, "huggingface.datasets-cache");
    assert!(notes(a).iter().any(|n| n.contains("can't verify")));
    let c = classify_expect(a, &clean_caller(a), Tier::S2);
    if c.tier == Tier::S2 {
        assert_eq!(c.action, Action::Quarantine, "S2: restorable move only");
    }
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn torch_hub_is_s1_and_needs_its_folder() {
    let mut fx = Fixture::new().unwrap();
    torch_hub(&mut fx, "home/.cache/torch/hub", false);
    torch_hub(&mut fx, "custom-torch/hub", true); // TORCH_HOME elsewhere, trusted_list present
    torch_hub(&mut fx, "app/hub", false); // a generic `hub` folder: not torch's
    age_tree(&mut fx, "home", days_ago(3));
    age_tree(&mut fx, "custom-torch", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        ["custom-torch/hub", "home/.cache/torch/hub"]
    );
    let cands: Vec<_> = s
        .found
        .iter()
        .map(|a| {
            assert_eq!(a.rule.id, "torch.hub");
            classify_expect(a, &clean_caller(a), Tier::S1)
        })
        .collect();
    check(&fx, &plan_inputs(&cands));
}

/// A whole `~/.cache` with every ML cache: one candidate per artifact, nothing inside them,
/// and the hub folder itself is never mistaken for torch's.
#[test]
fn full_ml_cache_scan() {
    let mut fx = Fixture::new().unwrap();
    hf_repo(&mut fx, "home/.cache/huggingface/hub/models--a--b", true);
    datasets_cache(&mut fx, "home/.cache/huggingface/datasets");
    torch_hub(&mut fx, "home/.cache/torch/hub", false);
    ollama_store(
        &mut fx,
        "home/.ollama/models",
        &[("registry.ollama.ai", "library", "phi3", "latest")],
    );
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        [
            "home/.cache/huggingface/datasets",
            "home/.cache/huggingface/hub/models--a--b",
            "home/.cache/torch/hub",
            "home/.ollama/models",
        ]
    );
    let want = |p: &str| {
        if p.ends_with("datasets") {
            Tier::S2
        } else {
            Tier::S1
        }
    };
    let cands: Vec<_> = s
        .found
        .iter()
        .map(|a| classify_expect(a, &clean_caller(a), want(&rel(&root, &a.path))))
        .collect();
    check(&fx, &plan_inputs(&cands));
}

// ---------------------------------------------------------------------------------------------
// Locator: every OS layout, environment overrides, refusals

fn build_defaults(fx: &mut Fixture) {
    hf_repo(fx, "home/.cache/huggingface/hub/models--a--b", true);
    hf_repo(fx, "home/.cache/huggingface/hub/datasets--c--d", false);
    datasets_cache(fx, "home/.cache/huggingface/datasets");
    ollama_store(
        fx,
        "home/.ollama/models",
        &[("registry.ollama.ai", "library", "phi3", "latest")],
    );
    torch_hub(fx, "home/.cache/torch/hub", false);
}

#[test]
fn locator_finds_default_roots_on_every_os_layout() {
    let mut fx = Fixture::new().unwrap();
    build_defaults(&mut fx);
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    for os in ALL_OS {
        let scan = locate(&env(&fx, os, &[]));
        assert!(scan.refused.is_empty(), "{os:?}: {:?}", scan.refused);
        assert_eq!(
            scan.roots
                .iter()
                .map(|r| (r.kind, rel(fx.root(), &r.path), r.source.clone()))
                .collect::<Vec<_>>(),
            [
                (
                    MlRootKind::HuggingFaceHub,
                    "home/.cache/huggingface/hub".to_owned(),
                    CacheSource::Default
                ),
                (
                    MlRootKind::HuggingFaceDatasets,
                    "home/.cache/huggingface/datasets".to_owned(),
                    CacheSource::Default
                ),
                (
                    MlRootKind::Ollama,
                    "home/.ollama/models".to_owned(),
                    CacheSource::Default
                ),
                (
                    MlRootKind::TorchHub,
                    "home/.cache/torch/hub".to_owned(),
                    CacheSource::Default
                ),
            ],
            "{os:?}"
        );
        let mut tiers: Vec<(String, Tier)> = scan
            .found
            .iter()
            .map(|a| {
                assert!(
                    notes(a)
                        .iter()
                        .any(|n| n.contains("location from the tool's default location"))
                );
                let t = scull_safety::classify(&a.signals(&clean_caller(a))).tier();
                (rel(fx.root(), &a.path), t)
            })
            .collect();
        tiers.sort();
        assert_eq!(
            tiers,
            [
                ("home/.cache/huggingface/datasets".to_owned(), Tier::S2),
                (
                    "home/.cache/huggingface/hub/datasets--c--d".to_owned(),
                    Tier::S2
                ),
                (
                    "home/.cache/huggingface/hub/models--a--b".to_owned(),
                    Tier::S1
                ),
                ("home/.cache/torch/hub".to_owned(), Tier::S1),
                ("home/.ollama/models".to_owned(), Tier::S1),
            ],
            "{os:?}"
        );
    }
    let cands: Vec<_> = locate(&env(&fx, Os::Linux, &[]))
        .found
        .iter()
        .map(|a| a.clone().into_candidate(&clean_caller(a)))
        .collect();
    assert!(cands.iter().all(|c| c.is_consistent()));
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn locator_follows_environment_overrides() {
    let mut fx = Fixture::new().unwrap();
    build_defaults(&mut fx); // must be ignored when overridden
    hf_repo(&mut fx, "hfhome/hub/models--x--y", true);
    datasets_cache(&mut fx, "hfhome/datasets");
    hf_repo(&mut fx, "hubcache/models--p--q", true);
    datasets_cache(&mut fx, "dscache"); // any folder name once configured
    ollama_store(
        &mut fx,
        "llm-store",
        &[("registry.ollama.ai", "library", "mistral", "7b")],
    );
    torch_hub(&mut fx, "torchhome/hub", false);
    torch_hub(&mut fx, "xdg/torch/hub", false);
    hf_repo(&mut fx, "xdg/huggingface/hub/models--m--n", true);
    fx.checkpoint().unwrap();
    for os in ALL_OS {
        // HF_HOME, OLLAMA_MODELS (folder not named `models`), TORCH_HOME.
        let scan = locate(&env(
            &fx,
            os,
            &[
                ("HF_HOME", abs(&fx, "hfhome")),
                ("OLLAMA_MODELS", abs(&fx, "llm-store")),
                ("TORCH_HOME", abs(&fx, "torchhome")),
            ],
        ));
        assert_eq!(
            found_rel(&fx, &scan),
            [
                (
                    "huggingface.datasets-cache".to_owned(),
                    "hfhome/datasets".to_owned()
                ),
                (
                    "huggingface.model".to_owned(),
                    "hfhome/hub/models--x--y".to_owned()
                ),
                ("ollama.models".to_owned(), "llm-store".to_owned()),
                ("torch.hub".to_owned(), "torchhome/hub".to_owned()),
            ],
            "{os:?}"
        );
        assert_eq!(
            scan.roots[0].source,
            CacheSource::Env {
                var: "HF_HOME".into()
            }
        );
        // The specific variables beat HF_HOME; XDG_CACHE_HOME applies to the rest.
        let scan = locate(&env(
            &fx,
            os,
            &[
                ("HF_HOME", abs(&fx, "hfhome")),
                ("HF_HUB_CACHE", abs(&fx, "hubcache")),
                ("HF_DATASETS_CACHE", abs(&fx, "dscache")),
                ("XDG_CACHE_HOME", abs(&fx, "xdg")),
            ],
        ));
        assert_eq!(
            found_rel(&fx, &scan),
            [
                (
                    "huggingface.datasets-cache".to_owned(),
                    "dscache".to_owned()
                ),
                (
                    "huggingface.model".to_owned(),
                    "hubcache/models--p--q".to_owned()
                ),
                ("ollama.models".to_owned(), "home/.ollama/models".to_owned()),
                ("torch.hub".to_owned(), "xdg/torch/hub".to_owned()),
            ],
            "{os:?}"
        );
    }
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn locator_refuses_unexpected_locations_and_links() {
    let mut fx = Fixture::new().unwrap();
    build_defaults(&mut fx);
    ollama_store(
        &mut fx,
        "elsewhere/models",
        &[("registry.ollama.ai", "library", "phi3", "latest")],
    );
    let linked = fx
        .symlink("home/linked-torch", "../home/.cache/torch")
        .unwrap()
        .is_created();
    fx.checkpoint().unwrap();
    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[
            ("OLLAMA_MODELS", abs(&fx, "home")),
            ("HF_HUB_CACHE", "relative/hub".into()),
            ("HF_DATASETS_CACHE", abs(&fx, "home/.cache")),
        ],
    ));
    let refused: Vec<(MlRootKind, &str)> = scan
        .refused
        .iter()
        .map(|r| (r.kind, r.reason.as_str()))
        .collect();
    assert_eq!(refused.len(), 3, "{refused:?}");
    assert_eq!(refused[0].0, MlRootKind::HuggingFaceHub);
    assert!(refused[0].1.contains("not an absolute path"));
    assert_eq!(refused[1].0, MlRootKind::HuggingFaceDatasets);
    assert!(refused[1].1.contains("shared folder"));
    assert_eq!(refused[2].0, MlRootKind::Ollama);
    assert!(refused[2].1.contains("home folder"));
    // Refused roots contribute nothing; the torch default is still found.
    assert_eq!(
        found_rel(&fx, &scan),
        [("torch.hub".to_owned(), "home/.cache/torch/hub".to_owned())]
    );
    if linked {
        let scan = locate(&env(
            &fx,
            Os::Linux,
            &[("TORCH_HOME", abs(&fx, "home/linked-torch"))],
        ));
        // `home/linked-torch/hub` is reached through a link: never followed.
        let r = scan
            .refused
            .iter()
            .find(|r| r.kind == MlRootKind::TorchHub)
            .expect("refused");
        assert!(r.reason.contains("is a link"), "{}", r.reason);
        assert!(scan.found.iter().all(|a| a.rule.id != "torch.hub"));
    }
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn missing_ml_caches_are_simply_absent() {
    let mut fx = Fixture::new().unwrap();
    fx.dir("home").unwrap().checkpoint().unwrap();
    for os in ALL_OS {
        let scan = locate(&env(&fx, os, &[]));
        assert!(scan.roots.is_empty() && scan.found.is_empty() && scan.refused.is_empty());
    }
    check(&fx, &plan_inputs(&[]));
}
