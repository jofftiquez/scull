//! Long-tail project folders (P5-DET-05, PRD §7.1): C/C++ (CMake, Meson, PlatformIO), SwiftPM
//! and CocoaPods, JVM extras (sbt, Bloop, Metals), Haskell (Stack, Cabal), Elixir (Mix), Ruby
//! (`vendor/bundle`), Zig, Terraform, Bazel output bases, Podman storage and the minikube cache.
//!
//! Every folder needs its marker evidence (CLS-03): a name alone, or a build file alone, is
//! never enough (FX-CLS-10). Every test builds its tree with `scull-fixtures`, detects
//! read-only through `scull-platform`, and checks the universal invariants (I-2 canary, I-3
//! tree unchanged, I-1 on the would-be plan).

#![allow(clippy::unwrap_used)]

mod common;

use common::{
    age_tree, check, classify_expect, clean_caller, identity_available, plan_inputs, rel, scan,
};
use scull_core::{Action, Candidate, Evidence, ReasonCode, Tier};
use scull_detectors::DetectedArtifact;
use scull_detectors::rules::Platform as Os;
use scull_fixtures::{Fixture, days_ago};
use std::path::Path;

/// Rule IDs of everything found, with paths, sorted.
fn found(root: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = scan(root, &[])
        .found
        .iter()
        .map(|a| (a.rule.id.clone(), rel(root, &a.path)))
        .collect();
    v.sort();
    v
}

fn notes(a: &DetectedArtifact) -> Vec<&str> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = v
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect();
    v.sort();
    v
}

/// Classify every artifact found at `root` as idle and clean; each must be `tier`.
fn classify_found(root: &Path, want: &[(&str, Tier)]) -> Vec<Candidate> {
    let s = scan(root, &[]);
    want.iter()
        .map(|(p, t)| {
            let a = s.at(root, p);
            classify_expect(a, &clean_caller(a), *t)
        })
        .collect()
}

/// Expected candidate tier: `t`, or S3 where file identity is unavailable (SAFE-05).
fn expected(t: Tier) -> Tier {
    if identity_available() { t } else { Tier::S3 }
}

fn file_uri_free(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

// ---------------------------------------------------------------------------------------------
// C / C++ / embedded

/// A CMake project configured out of source into `build/` (and a CLion `cmake-build-debug/`).
fn cmake_project(fx: &mut Fixture, dir: &str) {
    let src = fx.root().join(dir);
    let cache = format!(
        "# This is the CMakeCache file.\nCMAKE_BUILD_TYPE:STRING=Debug\nCMAKE_HOME_DIRECTORY:INTERNAL={}\n",
        file_uri_free(&src)
    );
    fx.file(format!("{dir}/CMakeLists.txt"), "project(app C)\n")
        .unwrap()
        .file(format!("{dir}/main.c"), "int main(void){return 0;}\n")
        .unwrap()
        .file(format!("{dir}/build/CMakeCache.txt"), &cache)
        .unwrap()
        .file(format!("{dir}/build/CMakeFiles/cmake.check_cache"), "x")
        .unwrap()
        .file_of_size(format!("{dir}/build/CMakeFiles/app.dir/main.c.o"), 2048)
        .unwrap()
        .file_of_size(format!("{dir}/build/app"), 4096)
        .unwrap()
        .file(format!("{dir}/cmake-build-debug/CMakeCache.txt"), &cache)
        .unwrap()
        .file(format!("{dir}/cmake-build-debug/CMakeFiles/x"), "x")
        .unwrap();
}

#[test]
fn cmake_build_folders_are_s1_with_their_source_project() {
    let mut fx = Fixture::new().unwrap();
    cmake_project(&mut fx, "app");
    age_tree(&mut fx, "app", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    assert_eq!(
        found(&root),
        pairs(&[
            ("cmake.build", "app/build"),
            ("cmake.build", "app/cmake-build-debug"),
        ])
    );
    let s = scan(&root, &[]);
    let b = s.at(&root, "app/build");
    assert_eq!(b.projects[0].name, "app");
    assert_eq!(b.projects[0].ecosystem, "cmake");
    assert!(
        notes(b)
            .iter()
            .any(|n| n.starts_with("configured for the sources in"))
    );
    assert!(b.activity_processes.contains(&"cmake".to_owned()));
    let candidates = classify_found(
        &root,
        &[("app/build", Tier::S1), ("app/cmake-build-debug", Tier::S1)],
    );
    for c in &candidates {
        if c.tier == Tier::S1 {
            assert_eq!(
                c.action,
                Action::Quarantine,
                "S1 is quarantined, never deleted"
            );
        }
    }
    check(&fx, &plan_inputs(&candidates));
}

#[test]
fn fx_cls_10_cmake_needs_its_cache_and_never_takes_an_in_source_build() {
    let mut fx = Fixture::new().unwrap();
    // FX-CLS-10: `build/` beside CMakeLists.txt but never configured (no CMakeCache.txt).
    fx.file("a/CMakeLists.txt", "project(a)\n")
        .unwrap()
        .file_of_size("a/build/notes.txt", 1024)
        .unwrap()
        .dir("a/build/CMakeFiles")
        .unwrap();
    // An in-source build named `build`: the project's own sources are in it.
    let insrc = fx.root().join("b/build");
    fx.file("b/build/CMakeLists.txt", "project(b)\n")
        .unwrap()
        .file(
            "b/build/CMakeCache.txt",
            format!("CMAKE_HOME_DIRECTORY:INTERNAL={}\n", file_uri_free(&insrc)),
        )
        .unwrap()
        .dir("b/build/CMakeFiles")
        .unwrap();
    // The cache records the folder itself as the source (CMakeLists.txt elsewhere).
    let self_src = fx.root().join("c/build");
    fx.file(
        "c/build/CMakeCache.txt",
        format!(
            "CMAKE_HOME_DIRECTORY:INTERNAL={}\n",
            file_uri_free(&self_src)
        ),
    )
    .unwrap()
    .dir("c/build/CMakeFiles")
    .unwrap();
    // A folder with an unusual name is left alone even when configured.
    fx.file("d/my-output/CMakeCache.txt", "x\n")
        .unwrap()
        .dir("d/my-output/CMakeFiles")
        .unwrap();
    fx.checkpoint().unwrap();
    assert_eq!(found(fx.root()), Vec::<(String, String)>::new());
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn cmake_build_of_a_deleted_source_tree_is_a_leftover() {
    let mut fx = Fixture::new().unwrap();
    let gone = fx.root().join("old-src");
    fx.file(
        "builds/build-old/CMakeCache.txt",
        format!("CMAKE_HOME_DIRECTORY:INTERNAL={}\n", file_uri_free(&gone)),
    )
    .unwrap()
    .file_of_size("builds/build-old/CMakeFiles/x.o", 512)
    .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let s = scan(&root, &[]);
    let a = s.at(&root, "builds/build-old");
    assert_eq!(a.projects[0].name, "old-src");
    assert!(
        notes(a).iter().any(|n| n.contains("no longer exist")),
        "{:?}",
        notes(a)
    );
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn meson_and_platformio_folders_are_s1() {
    let mut fx = Fixture::new().unwrap();
    fx.file("m/meson.build", "project('m', 'c')\n")
        .unwrap()
        .file("m/builddir/build.ninja", "rule cc\n")
        .unwrap()
        .file("m/builddir/meson-private/cmd_line.txt", "[options]\n")
        .unwrap()
        .file_of_size("m/builddir/m", 2048)
        .unwrap();
    // A Meson build folder holding a meson.build is a source folder: left alone.
    fx.file("n/build/meson.build", "project('n')\n")
        .unwrap()
        .file("n/build/build.ninja", "x")
        .unwrap()
        .dir("n/build/meson-private")
        .unwrap();
    fx.file(
        "fw/platformio.ini",
        "[env:esp32dev]\nplatform = espressif32\n",
    )
    .unwrap()
    .file_of_size("fw/.pio/build/esp32dev/firmware.bin", 4096)
    .unwrap()
    .file("fw/.pio/libdeps/esp32dev/integrity.dat", "x")
    .unwrap();
    // `.pio` without platformio.ini beside it.
    fx.file_of_size("stray/.pio/build/x.bin", 1024).unwrap();
    age_tree(&mut fx, "m", days_ago(3));
    age_tree(&mut fx, "fw", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(
        found(&root),
        pairs(&[("meson.build", "m/builddir"), ("platformio.pio", "fw/.pio")])
    );
    let c = classify_found(&root, &[("m/builddir", Tier::S1), ("fw/.pio", Tier::S1)]);
    check(&fx, &plan_inputs(&c));
}

// ---------------------------------------------------------------------------------------------
// SwiftPM, CocoaPods

#[test]
fn swiftpm_build_and_pods_need_manifest_and_lockfile() {
    let mut fx = Fixture::new().unwrap();
    fx.file("pkg/Package.swift", "// swift-tools-version:5.9\n")
        .unwrap()
        .file("pkg/Package.resolved", "{}")
        .unwrap()
        .file("pkg/.build/workspace-state.json", "{}")
        .unwrap()
        .file_of_size("pkg/.build/checkouts/swift-log/Sources/Logging.swift", 2048)
        .unwrap();
    fx.file("ios/Podfile", "platform :ios, '17.0'\n")
        .unwrap()
        .file("ios/Podfile.lock", "PODS:\n")
        .unwrap()
        .file("ios/Pods/Manifest.lock", "PODS:\n")
        .unwrap()
        .file_of_size("ios/Pods/Alamofire/Source/AF.swift", 2048)
        .unwrap();
    // No Podfile.lock: the pods installed again may differ (S2, FX-CLS-05).
    fx.file("ios2/Podfile", "x")
        .unwrap()
        .file("ios2/Pods/Manifest.lock", "x")
        .unwrap();
    // `.build` without Package.swift, `Pods` without Manifest.lock: not candidates.
    fx.file("other/.build/workspace-state.json", "{}")
        .unwrap()
        .file("other2/Podfile", "x")
        .unwrap()
        .file("other2/Pods/notes.txt", "mine")
        .unwrap();
    for d in ["pkg", "ios", "ios2"] {
        age_tree(&mut fx, d, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(
        found(&root),
        pairs(&[
            ("cocoapods.pods", "ios/Pods"),
            ("cocoapods.pods", "ios2/Pods"),
            ("swiftpm.build", "pkg/.build"),
        ])
    );
    let c = classify_found(
        &root,
        &[
            ("pkg/.build", Tier::S1),
            ("ios/Pods", Tier::S1),
            ("ios2/Pods", Tier::S2),
        ],
    );
    assert!(
        c[2].reasons
            .iter()
            .any(|r| r.code == ReasonCode::NoLockfile)
    );
    check(&fx, &plan_inputs(&c));
}

// ---------------------------------------------------------------------------------------------
// JVM extras

#[test]
fn sbt_bloop_and_metals() {
    let mut fx = Fixture::new().unwrap();
    fx.file("scala/build.sbt", "scalaVersion := \"3.5.0\"\n")
        .unwrap()
        .file("scala/project/build.properties", "sbt.version=1.10.1\n")
        .unwrap()
        .file_of_size("scala/target/scala-3.5.0/classes/Main.class", 2048)
        .unwrap()
        .file("scala/target/streams/compile/x", "x")
        .unwrap()
        .file_of_size("scala/project/target/config-classes/$abc.class", 512)
        .unwrap()
        .file("scala/.bloop/scala.json", "{}")
        .unwrap()
        .file_of_size("scala/.bloop/scala/bloop-bsp-clients-classes/x.class", 512)
        .unwrap()
        .file_of_size("scala/.metals/metals.mv.db", 4096)
        .unwrap()
        .file("scala/.metals/metals.log", "log\n")
        .unwrap();
    // A Maven project's `target/` is Maven's, never sbt's (and vice versa).
    fx.file("mvn/pom.xml", "<project/>")
        .unwrap()
        .file("mvn/build.sbt", "x")
        .unwrap()
        .file("mvn/target/streams/x", "x")
        .unwrap();
    age_tree(&mut fx, "scala", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(
        found(&root),
        pairs(&[
            ("bloop.dir", "scala/.bloop"),
            ("metals.dir", "scala/.metals"),
            ("sbt.project-target", "scala/project/target"),
            ("sbt.target", "scala/target"),
        ])
    );
    let c = classify_found(
        &root,
        &[
            ("scala/target", Tier::S0),
            ("scala/project/target", Tier::S0),
            ("scala/.bloop", Tier::S1),
            ("scala/.metals", Tier::S1),
        ],
    );
    if identity_available() {
        assert_eq!(c[0].action, Action::Delete);
    }
    check(&fx, &plan_inputs(&c));
}

// ---------------------------------------------------------------------------------------------
// Haskell, Elixir

#[test]
fn stack_cabal_and_mix() {
    let mut fx = Fixture::new().unwrap();
    fx.file("hs/stack.yaml", "resolver: lts-22.0\n")
        .unwrap()
        .file_of_size("hs/.stack-work/install/x86_64-linux/bin/app", 4096)
        .unwrap()
        .file("hs/cabal.project", "packages: .\n")
        .unwrap()
        .file("hs/dist-newstyle/cache/plan.json", "{}")
        .unwrap()
        .file_of_size("hs/dist-newstyle/build/x86_64-linux/app.o", 2048)
        .unwrap();
    fx.file("ex/mix.exs", "defmodule App.MixProject do end\n")
        .unwrap()
        .file("ex/mix.lock", "%{}\n")
        .unwrap()
        .file_of_size("ex/_build/dev/lib/app/ebin/app.beam", 2048)
        .unwrap()
        .file("ex/deps/jason/mix.exs", "x")
        .unwrap()
        .file_of_size("ex/deps/jason/lib/jason.ex", 1024)
        .unwrap();
    // Without mix.lock, deps/ can't be reproduced exactly: S2.
    fx.file("ex2/mix.exs", "x")
        .unwrap()
        .file("ex2/deps/plug/mix.exs", "x")
        .unwrap();
    // `dist-newstyle` without a Cabal file beside it; `_build` without `mix.exs`.
    fx.file("x/dist-newstyle/cache/plan.json", "{}")
        .unwrap()
        .file("y/_build/dev/x", "x")
        .unwrap();
    for d in ["hs", "ex", "ex2"] {
        age_tree(&mut fx, d, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(
        found(&root),
        pairs(&[
            ("cabal.dist-newstyle", "hs/dist-newstyle"),
            ("mix.build", "ex/_build"),
            ("mix.deps", "ex/deps"),
            ("mix.deps", "ex2/deps"),
            ("stack.work", "hs/.stack-work"),
        ])
    );
    let c = classify_found(
        &root,
        &[
            ("hs/.stack-work", Tier::S1),
            ("hs/dist-newstyle", Tier::S1),
            ("ex/_build", Tier::S0),
            ("ex/deps", Tier::S1),
            ("ex2/deps", Tier::S2),
        ],
    );
    check(&fx, &plan_inputs(&c));
}

// ---------------------------------------------------------------------------------------------
// Ruby, Zig

#[test]
fn vendor_bundle_must_sit_in_vendor() {
    let mut fx = Fixture::new().unwrap();
    fx.file("rb/Gemfile", "source 'https://rubygems.org'\n")
        .unwrap()
        .file("rb/Gemfile.lock", "GEM\n")
        .unwrap()
        .file_of_size(
            "rb/vendor/bundle/ruby/3.3.0/gems/rack-3.1.7/lib/rack.rb",
            2048,
        )
        .unwrap();
    // Same shape, but not in `vendor/`: a folder of the user's own.
    fx.file("rb2/Gemfile", "x")
        .unwrap()
        .file("rb2/stuff/bundle/ruby/notes.txt", "mine")
        .unwrap();
    age_tree(&mut fx, "rb", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(
        found(&root),
        pairs(&[("bundler.vendor-bundle", "rb/vendor/bundle")])
    );
    let s = scan(&root, &[]);
    assert_eq!(s.at(&root, "rb/vendor/bundle").projects[0].name, "rb");
    let c = classify_found(&root, &[("rb/vendor/bundle", Tier::S1)]);
    check(&fx, &plan_inputs(&c));
}

#[test]
fn zig_cache_and_out_are_s0() {
    let mut fx = Fixture::new().unwrap();
    fx.file("z/build.zig", "pub fn build() void {}\n")
        .unwrap()
        .file_of_size("z/.zig-cache/o/0123/main.o", 2048)
        .unwrap()
        .file_of_size("z/zig-out/bin/z", 4096)
        .unwrap();
    // Legacy `zig-cache/`; and a `zig-out/` without build.zig beside it.
    fx.file("old/build.zig", "x")
        .unwrap()
        .file("old/zig-cache/h/x.txt", "x")
        .unwrap()
        .file("stray/zig-out/bin/x", "x")
        .unwrap();
    age_tree(&mut fx, "z", days_ago(3));
    age_tree(&mut fx, "old", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(
        found(&root),
        pairs(&[
            ("zig.cache", "old/zig-cache"),
            ("zig.cache", "z/.zig-cache"),
            ("zig.out", "z/zig-out"),
        ])
    );
    let c = classify_found(
        &root,
        &[
            ("z/.zig-cache", Tier::S0),
            ("z/zig-out", Tier::S0),
            ("old/zig-cache", Tier::S0),
        ],
    );
    check(&fx, &plan_inputs(&c));
}

// ---------------------------------------------------------------------------------------------
// Terraform, Bazel, Podman, minikube

#[test]
fn terraform_is_s1_and_s2_with_backend_or_workspace() {
    let mut fx = Fixture::new().unwrap();
    for d in ["tf", "tfws", "tfold"] {
        fx.file(format!("{d}/main.tf"), "terraform {}\n")
            .unwrap()
            .file_of_size(
                format!("{d}/.terraform/providers/registry.terraform.io/hashicorp/aws/5.0.0/linux_amd64/terraform-provider-aws"),
                4096,
            )
            .unwrap();
    }
    fx.file("tf/.terraform.lock.hcl", "provider {}\n")
        .unwrap()
        .file("tfws/.terraform.lock.hcl", "provider {}\n")
        .unwrap()
        .file("tfws/.terraform/environment", "staging")
        .unwrap()
        .file("tfws/.terraform/terraform.tfstate", "{\"backend\":{}}")
        .unwrap();
    // `.terraform` beside no configuration.
    fx.file("x/.terraform/providers/p", "x").unwrap();
    for d in ["tf", "tfws", "tfold"] {
        age_tree(&mut fx, d, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert_eq!(
        found(&root),
        pairs(&[
            ("terraform.dir", "tf/.terraform"),
            ("terraform.dir", "tfold/.terraform"),
            ("terraform.dir", "tfws/.terraform"),
        ])
    );
    let s = scan(&root, &[]);
    let plain = classify_expect(
        s.at(&root, "tf/.terraform"),
        &clean_caller(s.at(&root, "tf/.terraform")),
        Tier::S1,
    );
    // No .terraform.lock.hcl: S2 (FX-CLS-05).
    let old = classify_expect(
        s.at(&root, "tfold/.terraform"),
        &clean_caller(s.at(&root, "tfold/.terraform")),
        Tier::S2,
    );
    let ws = s.at(&root, "tfws/.terraform");
    let n = notes(ws);
    assert!(n.iter().any(|t| t.contains("selected workspace")), "{n:?}");
    assert!(
        n.iter().any(|t| t.contains("backend configuration")),
        "{n:?}"
    );
    let wsc = ws.clone().into_candidate(&clean_caller(ws));
    assert_eq!(wsc.tier, expected(Tier::S2));
    assert!(wsc.is_consistent());
    check(&fx, &plan_inputs(&[plain, old, wsc]));
}

#[test]
fn bazel_output_base_is_inspect_only_with_its_workspace() {
    let mut fx = Fixture::new().unwrap();
    let ws = fx.root().join("ws");
    let gone = fx.root().join("deleted-ws");
    fx.file("ws/MODULE.bazel", "module(name = \"ws\")\n")
        .unwrap();
    for (hash, target) in [
        ("0123456789abcdef0123456789abcdef", &ws),
        ("fedcba9876543210fedcba9876543210", &gone),
    ] {
        let base = format!("cache/bazel/_bazel_me/{hash}");
        fx.file(
            format!("{base}/DO_NOT_BUILD_HERE"),
            target.to_string_lossy().as_bytes(),
        )
        .unwrap()
        .file_of_size(
            format!("{base}/execroot/_main/bazel-out/k8-fastbuild/bin/app"),
            4096,
        )
        .unwrap()
        .file(format!("{base}/external/@rules_cc.marker"), "x")
        .unwrap();
    }
    // Look-alikes: not in `_bazel_<user>`, or not a hex name.
    fx.file(
        "cache/other/0123456789abcdef0123456789abcdef/DO_NOT_BUILD_HERE",
        "/x",
    )
    .unwrap()
    .dir("cache/other/0123456789abcdef0123456789abcdef/execroot")
    .unwrap()
    .file("cache/bazel/_bazel_me/install/DO_NOT_BUILD_HERE", "/x")
    .unwrap()
    .dir("cache/bazel/_bazel_me/install/execroot")
    .unwrap();
    age_tree(&mut fx, "cache", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let base = "cache/bazel/_bazel_me/0123456789abcdef0123456789abcdef";
    let left = "cache/bazel/_bazel_me/fedcba9876543210fedcba9876543210";
    assert_eq!(
        found(&root),
        pairs(&[("bazel.output-base", base), ("bazel.output-base", left)])
    );
    let s = scan(&root, &[]);
    let a = s.at(&root, base);
    assert_eq!(a.projects[0].name, "ws");
    assert!(notes(a).iter().any(|n| n.contains("bazel clean --expunge")));
    let l = s.at(&root, left);
    assert!(notes(l).iter().any(|n| n.contains("no longer exists")));
    let mut cands = Vec::new();
    for x in [a, l] {
        let c = x.clone().into_candidate(&clean_caller(x));
        assert_eq!(c.tier, Tier::S3, "inspect-only");
        assert_eq!(c.action, Action::None);
        assert!(c.reasons.iter().any(|r| r.code == ReasonCode::InspectOnly));
        cands.push(c);
    }
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn podman_storage_and_minikube_cache_are_inspect_only() {
    let mut fx = Fixture::new().unwrap();
    fx.file("home/.local/share/containers/storage/storage.lock", "")
        .unwrap()
        .file(
            "home/.local/share/containers/storage/libpod/bolt_state.db",
            "x",
        )
        .unwrap()
        .file_of_size(
            "home/.local/share/containers/storage/overlay/abc/diff/bin/sh",
            4096,
        )
        .unwrap()
        .file_of_size(
            "home/.local/share/containers/storage/volumes/db/_data/pg.dat",
            4096,
        )
        .unwrap()
        .file_of_size(
            "home/.minikube/cache/iso/amd64/minikube-v1.34.0-amd64.iso",
            4096,
        )
        .unwrap()
        .file_of_size(
            "home/.minikube/cache/preloaded-tarball/preloaded-images.tar.lz4",
            4096,
        )
        .unwrap();
    // A `cache` folder with an `images` folder elsewhere is not minikube's.
    fx.file("home/site/cache/images/logo.png", "png")
        .unwrap()
        .file("home/site/cache/iso/x", "x")
        .unwrap();
    age_tree(&mut fx, "home", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    let mut want = vec![("minikube.cache", "home/.minikube/cache")];
    if Os::current() == Some(Os::Linux) {
        want.push(("podman.storage", "home/.local/share/containers/storage"));
    }
    assert_eq!(found(&root), pairs(&want));
    let s = scan(&root, &[]);
    let mut cands = Vec::new();
    for a in &s.found {
        assert!(
            a.projects.is_empty(),
            "tool-wide folders belong to no project"
        );
        let c = a.clone().into_candidate(&clean_caller(a));
        assert_eq!(c.tier, Tier::S3);
        assert_eq!(c.action, Action::None);
        cands.push(c);
    }
    check(&fx, &plan_inputs(&cands));
}
