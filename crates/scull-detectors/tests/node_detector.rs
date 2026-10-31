//! JS/TS detector fixtures (P1-DET-03): FX-CLS-05, -06, -08, and CLS-03 marker gating for
//! `.next`, `.nuxt`, `.output`, `.turbo`, `.parcel-cache`. Every test builds its tree with
//! `scull-fixtures`, detects read-only through `scull-platform`, and checks the universal
//! invariants (I-2 canary, I-3 tree unchanged, I-1 on the would-be plan).

#![allow(clippy::unwrap_used)]

mod common;

use common::{
    age_tree, check, classify_expect, clean_caller, logical_bytes, plan_inputs, rel, scan,
};
use scull_core::{Evidence, ReasonCode, Tier};
use scull_detectors::rules::RuleSet;
use scull_detectors::{DetectContext, Detector, Detectors};
use scull_fixtures::{Fixture, NodeLock, days_ago};
use scull_platform::Native;
use scull_safety::Regen;
use std::fs;
use std::path::{Path, PathBuf};

fn notes(a: &scull_detectors::DetectedArtifact) -> Vec<&str> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn lockfiles(a: &scull_detectors::DetectedArtifact, root: &Path) -> Vec<String> {
    a.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Lockfile { path } => Some(rel(root, &path.to_path_buf())),
            _ => None,
        })
        .collect()
}

fn package_json(name: &str, dev_deps: &[&str]) -> String {
    let deps: Vec<String> = dev_deps.iter().map(|d| format!("\"{d}\": \"*\"")).collect();
    format!(
        "{{\n  \"name\": \"{name}\",\n  \"private\": true,\n  \"devDependencies\": {{ {} }}\n}}\n",
        deps.join(", ")
    )
}

/// Every top-level `node_modules` under `root` (not nested in another), found with `std::fs`
/// independently of the detectors. Links are not entered.
fn independent_node_modules(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let md = fs::symlink_metadata(e.path()).unwrap();
            if !md.is_dir() {
                continue;
            }
            if e.file_name() == "node_modules" {
                out.push(e.path());
            } else {
                stack.push(e.path());
            }
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-05

#[test]
fn fx_cls_05_node_modules_with_lockfile_is_s0() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("npm-app", "npm-app", NodeLock::Npm)
        .unwrap()
        .node_modules("npm-app", &["left-pad", "lodash"])
        .unwrap()
        .npm_project("pnpm-app", "pnpm-app", NodeLock::Pnpm)
        .unwrap()
        .node_modules("pnpm-app", &["react"])
        .unwrap();
    // The other lockfiles the rule accepts.
    for lock in ["yarn.lock", "bun.lock", "bun.lockb", "npm-shrinkwrap.json"] {
        let p = format!("with-{}", lock.replace('.', "-"));
        fx.npm_project(&p, &p, NodeLock::None)
            .unwrap()
            .file(format!("{p}/{lock}"), "lock\n")
            .unwrap()
            .node_modules(&p, &["dep"])
            .unwrap();
    }
    for p in independent_node_modules(fx.root()) {
        let r = rel(fx.root(), &p);
        age_tree(&mut fx, &r, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.found.len(), 6, "{:?}", s.rel_paths(&root));
    let mut candidates = Vec::new();
    for a in &s.found {
        assert_eq!(a.rule.id, "node.node-modules");
        assert!(a.signals.lockfile_present, "{}", a.path.display());
        // Reinstall needs network but is pinned and quick: S0 per FX-CLS-05.
        assert_eq!(a.signals.regen, Regen::Local);
        candidates.push(classify_expect(a, &clean_caller(a), Tier::S0));
    }
    assert_eq!(
        lockfiles(s.at(&root, "npm-app/node_modules"), &root),
        ["npm-app/package-lock.json"]
    );
    assert_eq!(
        lockfiles(s.at(&root, "pnpm-app/node_modules"), &root),
        ["pnpm-app/pnpm-lock.yaml"]
    );
    check(&fx, &plan_inputs(&candidates));
}

#[test]
fn fx_cls_05_node_modules_without_lockfile_is_s2() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("app", "app", NodeLock::None)
        .unwrap()
        .node_modules("app", &["left-pad"])
        .unwrap();
    age_tree(&mut fx, "app/node_modules", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    let a = s.at(&root, "app/node_modules");
    assert!(!a.signals.lockfile_present);
    let c = classify_expect(a, &clean_caller(a), Tier::S2);
    assert!(c.reasons.iter().any(|r| r.code == ReasonCode::NoLockfile));
    check(&fx, &plan_inputs(&[c]));
}

#[test]
fn fx_cls_05_node_modules_without_package_json_is_not_a_candidate() {
    let mut fx = Fixture::new().unwrap();
    fx.node_modules("loose", &["left-pad"])
        .unwrap()
        .file("loose/package-lock.json", "{}")
        .unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    assert!(scan(&root, &[]).found.is_empty());
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-06

#[test]
fn fx_cls_06_workspace_and_link_symlinks_are_recorded_never_followed() {
    let mut fx = Fixture::new().unwrap();
    fx.file(
        "web/package.json",
        "{\"name\":\"web\",\"private\":true,\"workspaces\":[\"packages/*\"]}\n",
    )
    .unwrap()
    .file("web/package-lock.json", "{}")
    .unwrap()
    .npm_project("web/packages/ui", "@acme/ui", NodeLock::None)
    .unwrap()
    .file("web/packages/ui/src/index.js", "export const ui = 1;\n")
    .unwrap()
    // A sibling source folder used through `npm link`.
    .npm_project("linked-lib", "linked-lib", NodeLock::None)
    .unwrap()
    .file("linked-lib/src/index.js", "module.exports = 1;\n")
    .unwrap()
    .node_modules("web", &["left-pad"])
    .unwrap();
    let ws = fx
        .symlink("web/node_modules/@acme/ui", "../../packages/ui")
        .unwrap();
    let npm_link = fx
        .symlink("web/node_modules/linked-lib", "../../linked-lib")
        .unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    let escape = fx.symlink("web/node_modules/escape", &canary).unwrap();
    age_tree(&mut fx, "web/node_modules", days_ago(3));
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["web/node_modules"]);
    let a = s.at(&root, "web/node_modules");
    // The walk stopped at the candidate and never went through a link.
    assert!(
        !s.visited
            .iter()
            .any(|v| v.starts_with(root.join("web/node_modules/")) && *v != a.path)
    );
    if ws.is_created() && npm_link.is_created() && escape.is_created() {
        assert_eq!(a.contains_links, Some(true));
        let note = notes(a)
            .into_iter()
            .find(|n| n.contains("symlink"))
            .expect("link evidence");
        assert!(note.contains("contains 3 symlink(s)"), "{note}");
        for name in ["@acme/ui", "linked-lib", "escape"] {
            assert!(note.contains(name), "{note}");
        }
        assert!(note.contains("targets are never followed or removed"));
        let c = a.clone().into_candidate(&clean_caller(a));
        assert_eq!(c.flags.symlink, Some(true));
    }
    let c = classify_expect(a, &clean_caller(a), Tier::S0);
    check(&fx, &plan_inputs(std::slice::from_ref(&c)));

    // Stand-in for the executor (P2): removing the candidate removes the links only. The
    // linked source folders and the canary must be intact (I-2, and I-3 with only the plan item
    // allowed to change). `remove_dir_all` does not follow links.
    fs::remove_dir_all(&a.path).unwrap();
    assert!(root.join("web/packages/ui/src/index.js").is_file());
    assert!(root.join("linked-lib/src/index.js").is_file());
    check(&fx, &plan_inputs(&[c]).allow("web/node_modules"));
}

// ---------------------------------------------------------------------------------------------
// FX-CLS-08

#[test]
fn fx_cls_08_pnpm_monorepo_with_30_packages_every_candidate_found_once() {
    let mut fx = Fixture::new().unwrap();
    let names: Vec<String> = (1..=30).map(|i| format!("p{i:02}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    fx.pnpm_workspace("mono", "mono", &refs).unwrap();
    // Root node_modules with pnpm's virtual store (nested node_modules belong to it).
    fx.file_of_size(
        "mono/node_modules/.pnpm/left-pad@1.3.0/node_modules/left-pad/index.js",
        700,
    )
    .unwrap()
    .file("mono/node_modules/.modules.yaml", "layoutVersion: 5\n")
    .unwrap();
    let mut links_made = true;
    for (i, n) in names.iter().enumerate() {
        let pkg = format!("mono/packages/{n}");
        fx.node_modules(&pkg, &[&format!("dep-{n}")]).unwrap();
        fx.file_of_size(
            format!("{pkg}/node_modules/dep-{n}/data.bin"),
            100 + i as u64,
        )
        .unwrap();
        links_made &= fx
            .symlink(
                format!("{pkg}/node_modules/left-pad"),
                "../../../node_modules/.pnpm/left-pad@1.3.0/node_modules/left-pad",
            )
            .unwrap()
            .is_created();
    }
    // Not listed in pnpm-workspace.yaml: the workspace lockfile does not cover it.
    fx.npm_project("mono/tools/script", "script", NodeLock::None)
        .unwrap()
        .node_modules("mono/tools/script", &["chalk"])
        .unwrap();
    for p in independent_node_modules(fx.root()) {
        let r = rel(fx.root(), &p);
        age_tree(&mut fx, &r, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    // Every candidate found once: exactly the independently found top-level node_modules.
    let mut found: Vec<PathBuf> = s.found.iter().map(|a| a.path.clone()).collect();
    found.sort();
    let expected = independent_node_modules(&root);
    assert_eq!(expected.len(), 32);
    assert_eq!(found, expected);
    // Totals match an independent measurement.
    let detected: u64 = s.found.iter().map(|a| logical_bytes(&a.path)).sum();
    let independent: u64 = expected.iter().map(|p| logical_bytes(p)).sum();
    assert_eq!(detected, independent);

    // The nested store node_modules is part of the root candidate, never its own.
    let detectors = Detectors::bundled().unwrap();
    let nested = root.join("mono/node_modules/.pnpm/left-pad@1.3.0/node_modules");
    assert!(
        detectors
            .detect(&nested, &Native::new(), &DetectContext::new())
            .is_none()
    );

    let mut candidates = Vec::new();
    for n in &names {
        let a = s.at(&root, &format!("mono/packages/{n}/node_modules"));
        assert_eq!(lockfiles(a, &root), ["mono/pnpm-lock.yaml"], "{n}");
        assert!(notes(a).iter().any(|t| t.contains("workspace member")));
        if links_made {
            assert_eq!(a.contains_links, Some(true));
        }
        candidates.push(classify_expect(a, &clean_caller(a), Tier::S0));
    }
    let a = s.at(&root, "mono/node_modules");
    candidates.push(classify_expect(a, &clean_caller(a), Tier::S0));
    let a = s.at(&root, "mono/tools/script/node_modules");
    candidates.push(classify_expect(a, &clean_caller(a), Tier::S2));
    check(&fx, &plan_inputs(&candidates));
}

// ---------------------------------------------------------------------------------------------
// CLS-03: framework outputs need the framework's marker.

#[test]
fn cls_03_next_needs_config_or_dependency() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("cfg", "cfg", NodeLock::Npm)
        .unwrap()
        .file("cfg/next.config.mjs", "export default {};\n")
        .unwrap()
        .file("cfg/.next/BUILD_ID", "abc")
        .unwrap();
    fx.npm_project("plain", "plain", NodeLock::Npm)
        .unwrap()
        .file("plain/.next/BUILD_ID", "abc")
        .unwrap();
    fx.file("dep/package.json", package_json("dep", &["next"]))
        .unwrap()
        .file("dep/pnpm-lock.yaml", "lockfileVersion: '9.0'\n")
        .unwrap()
        .file("dep/.next/BUILD_ID", "abc")
        .unwrap();
    for p in ["cfg", "plain", "dep"] {
        age_tree(&mut fx, &format!("{p}/.next"), days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(s.rel_paths(&root), ["cfg/.next", "dep/.next"]);
    let mut cs = Vec::new();
    for p in ["cfg/.next", "dep/.next"] {
        let a = s.at(&root, p);
        assert_eq!(a.rule.id, "node.next");
        cs.push(classify_expect(a, &clean_caller(a), Tier::S0));
    }
    assert!(
        notes(s.at(&root, "dep/.next"))
            .iter()
            .any(|n| n.contains("dependency on `next`"))
    );
    // Without a reader, package.json can't be read: only config-file markers count.
    let rules = RuleSet::bundled().unwrap();
    let d = Detectors::from_rules(&rules);
    let p = Native::new();
    assert!(
        d.detect(&root.join("dep/.next"), &p, &DetectContext::new())
            .is_none()
    );
    assert!(
        d.detect(&root.join("cfg/.next"), &p, &DetectContext::new())
            .is_some()
    );
    check(&fx, &plan_inputs(&cs));
}

#[test]
fn cls_03_nuxt_and_output_need_a_nuxt_or_nitro_project() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("shop", "shop", NodeLock::Pnpm)
        .unwrap()
        .file(
            "shop/nuxt.config.ts",
            "export default defineNuxtConfig({});\n",
        )
        .unwrap()
        .file("shop/.nuxt/nuxt.d.ts", "")
        .unwrap()
        .file("shop/.output/server/index.mjs", "")
        .unwrap();
    fx.npm_project("api", "api", NodeLock::Npm)
        .unwrap()
        .file("api/nitro.config.ts", "export default {};\n")
        .unwrap()
        .file("api/.output/server/index.mjs", "")
        .unwrap();
    // A generic `.output` beside a package.json is someone's data, not a build output.
    fx.npm_project("report", "report", NodeLock::Npm)
        .unwrap()
        .file("report/.output/results.csv", "a,b\n")
        .unwrap();
    // A Nuxt config without package.json: not a Node project.
    fx.file("odd/nuxt.config.ts", "")
        .unwrap()
        .file("odd/.output/x", "")
        .unwrap();
    for p in ["shop/.nuxt", "shop/.output", "api/.output"] {
        age_tree(&mut fx, p, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        ["api/.output", "shop/.nuxt", "shop/.output"]
    );
    assert_eq!(s.at(&root, "shop/.nuxt").rule.id, "node.nuxt");
    let mut cs = Vec::new();
    for a in &s.found {
        cs.push(classify_expect(a, &clean_caller(a), Tier::S0));
    }
    check(&fx, &plan_inputs(&cs));
}

#[test]
fn cls_03_turbo_at_root_and_in_workspace_packages() {
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("repo", "repo", NodeLock::Pnpm)
        .unwrap()
        .file("repo/pnpm-workspace.yaml", "packages:\n  - 'apps/*'\n")
        .unwrap()
        .file("repo/turbo.json", "{\"tasks\":{}}\n")
        .unwrap()
        .file("repo/.turbo/cache/abc.tar.zst", "cache")
        .unwrap()
        .npm_project("repo/apps/web", "web", NodeLock::None)
        .unwrap()
        .file("repo/apps/web/.turbo/turbo-build.log", "built\n")
        .unwrap()
        .file("repo/apps/web/next.config.js", "module.exports = {};\n")
        .unwrap()
        .file("repo/apps/web/.next/BUILD_ID", "x")
        .unwrap();
    // No turbo.json and no turbo dependency: not a Turborepo cache.
    fx.npm_project("other", "other", NodeLock::Npm)
        .unwrap()
        .file("other/.turbo/x", "")
        .unwrap();
    for p in ["repo/.turbo", "repo/apps/web/.turbo", "repo/apps/web/.next"] {
        age_tree(&mut fx, p, days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        ["repo/.turbo", "repo/apps/web/.next", "repo/apps/web/.turbo"]
    );
    let mut cs = Vec::new();
    for a in &s.found {
        // Package-level outputs use the workspace root's lockfile.
        cs.push(classify_expect(a, &clean_caller(a), Tier::S0));
    }
    assert_eq!(
        lockfiles(s.at(&root, "repo/apps/web/.turbo"), &root),
        ["repo/pnpm-lock.yaml"]
    );
    check(&fx, &plan_inputs(&cs));
}

#[test]
fn cls_03_parcel_cache_needs_parcelrc_or_dependency() {
    let mut fx = Fixture::new().unwrap();
    fx.file("dep/package.json", package_json("dep", &["parcel"]))
        .unwrap()
        .file("dep/yarn.lock", "# yarn\n")
        .unwrap()
        .file("dep/.parcel-cache/data.mdb", "x")
        .unwrap();
    fx.npm_project("rc", "rc", NodeLock::Npm)
        .unwrap()
        .file("rc/.parcelrc", "{\"extends\":\"@parcel/config-default\"}\n")
        .unwrap()
        .file("rc/.parcel-cache/data.mdb", "x")
        .unwrap();
    fx.file("none/package.json", package_json("none", &["vite"]))
        .unwrap()
        .file("none/package-lock.json", "{}")
        .unwrap()
        .file("none/.parcel-cache/data.mdb", "x")
        .unwrap();
    for p in ["dep", "rc", "none"] {
        age_tree(&mut fx, &format!("{p}/.parcel-cache"), days_ago(3));
    }
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();

    let s = scan(&root, &[]);
    assert_eq!(
        s.rel_paths(&root),
        ["dep/.parcel-cache", "rc/.parcel-cache"]
    );
    let mut cs = Vec::new();
    for a in &s.found {
        assert_eq!(a.rule.id, "node.parcel-cache");
        cs.push(classify_expect(a, &clean_caller(a), Tier::S0));
    }
    check(&fx, &plan_inputs(&cs));
}

#[test]
fn registry_uses_node_detectors_for_node_rules() {
    let d = Detectors::bundled().unwrap();
    let ids: Vec<&str> = d.ids().collect();
    for id in [
        "node.next",
        "node.node-modules",
        "node.nuxt",
        "node.output",
        "node.parcel-cache",
        "node.turbo",
        "rust.target",
    ] {
        assert!(ids.contains(&id), "{id} missing from {ids:?}");
    }
    // A node detector on its own still refuses a link named node_modules.
    let mut fx = Fixture::new().unwrap();
    fx.npm_project("app", "app", NodeLock::Npm).unwrap();
    let canary = fx.canary().symlink_target_dir.clone();
    let made = fx.symlink("app/node_modules", &canary).unwrap();
    fx.checkpoint().unwrap();
    let root = fx.root().to_path_buf();
    if made.is_created() {
        let rules = RuleSet::bundled().unwrap();
        let nm = scull_detectors::NodeModulesDetector::new(
            rules.get("node.node-modules").unwrap().clone(),
        );
        assert!(
            nm.detect(
                &root.join("app/node_modules"),
                &Native::new(),
                &DetectContext::new()
            )
            .is_none()
        );
    }
    assert!(scan(&root, &[]).found.is_empty());
    check(&fx, &plan_inputs(&[]));
}
