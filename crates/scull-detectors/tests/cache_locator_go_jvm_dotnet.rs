//! Go, Gradle, Maven and NuGet caches in the Quick Clean locator (P5-DET-01, PRD §10.8):
//! environment and user-config overrides for every OS layout (injected `CacheEnv`), the native
//! commands chosen in Rust (`go clean -cache` / `-modcache`, `dotnet nuget locals
//! global-packages --clear`), Gradle and Maven inspect-only, and T-23 refusals.
//!
//! Nothing reads the real home folder or environment and no tool is ever run. The universal
//! invariants (I-2 canary, I-3 tree unchanged, I-1 on the would-be plan) are checked after each
//! test.

#![allow(clippy::unwrap_used)]

mod common;

use common::jvm_caches::{
    go_build_cache, go_modcache, gradle_caches, maven_repository, nuget_packages,
};
use common::{StdReader, check, identity_available, plan_inputs, rel};
use scull_core::{Action, Candidate, Evidence, ReasonCode, Tier};
use scull_detectors::caches::{CacheScan, CacheSource, CacheTool};
use scull_detectors::rules::Platform as Os;
use scull_detectors::{CacheEnv, CallerSignals, DetectContext, LocatedCache, locate_caches};
use scull_fixtures::Fixture;
use scull_platform::Native;
use scull_safety::{GitTracking, Tri};

const ALL_OS: [Os; 3] = [Os::Linux, Os::Macos, Os::Windows];
const NEW_TOOLS: [CacheTool; 5] = [
    CacheTool::GoBuild,
    CacheTool::GoMod,
    CacheTool::Gradle,
    CacheTool::Maven,
    CacheTool::NuGet,
];

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

fn locate(env: &CacheEnv) -> CacheScan {
    let reader = StdReader;
    locate_caches(
        env,
        &Native::new(),
        &DetectContext::new().with_config_reader(&reader),
    )
}

/// Only the tools this file is about, as `(tool, path relative to the fixture root)`.
fn found(fx: &Fixture, scan: &CacheScan) -> Vec<(CacheTool, String)> {
    scan.found
        .iter()
        .filter(|c| NEW_TOOLS.contains(&c.tool))
        .map(|c| (c.tool, rel(fx.root(), &c.path)))
        .collect()
}

fn the(scan: &CacheScan, tool: CacheTool) -> &LocatedCache {
    let hits: Vec<_> = scan.found.iter().filter(|c| c.tool == tool).collect();
    assert_eq!(hits.len(), 1, "exactly one {tool} in {:?}", scan.found);
    hits[0]
}

fn refused_tools(scan: &CacheScan) -> Vec<CacheTool> {
    scan.refused.iter().map(|r| r.tool).collect()
}

fn idle() -> CallerSignals {
    CallerSignals {
        git: GitTracking::NotTracked,
        in_protected_root: false,
        user_excluded: false,
        protect_marker: false,
        in_use: Tri::No,
        crosses_mount: false,
        link_escape: false,
        cloud_sync: false,
        recent_activity: Tri::Yes,
    }
}

fn expected(t: Tier) -> Tier {
    if identity_available() { t } else { Tier::S3 }
}

fn notes(c: &Candidate) -> Vec<&str> {
    c.evidence
        .iter()
        .filter_map(|e| match e {
            Evidence::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_action(tool: &str, args: &[&str]) -> Action {
    Action::ToolCommand {
        tool: tool.into(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// Go

#[test]
fn go_caches_default_per_os_and_commands() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        let build = match os {
            Os::Linux => "home/.cache/go-build",
            Os::Macos => "home/Library/Caches/go-build",
            Os::Windows => "home/AppData/Local/go-build",
        };
        go_build_cache(&mut fx, build);
        go_modcache(&mut fx, "home/go/pkg/mod");
        fx.checkpoint().unwrap();

        let scan = locate(&env(&fx, os, &[]));
        assert!(scan.refused.is_empty(), "{os:?}: {:?}", scan.refused);
        assert_eq!(
            found(&fx, &scan),
            [
                (CacheTool::GoBuild, build.to_owned()),
                (CacheTool::GoMod, "home/go/pkg/mod".to_owned())
            ],
            "{os:?}"
        );
        let mut candidates = Vec::new();
        for c in scan.found {
            assert!(c.recognized, "{} recognized", c.tool);
            let tool = c.tool;
            let cand = c.into_candidate(&idle(), None);
            assert!(cand.is_consistent());
            assert_eq!(cand.tier, expected(Tier::S1), "{tool}: S1 (QC-02)");
            assert!(
                cand.reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::PackageCache)
            );
            if identity_available() {
                let want = match tool {
                    CacheTool::GoBuild => tool_action("go", &["clean", "-cache"]),
                    _ => tool_action("go", &["clean", "-modcache"]),
                };
                assert_eq!(cand.action, want);
            }
            if tool == CacheTool::GoMod {
                assert!(
                    notes(&cand).iter().any(|n| n.contains("read-only")),
                    "{:?}",
                    notes(&cand)
                );
            }
            candidates.push(cand);
        }
        check(&fx, &plan_inputs(&candidates));
    }
}

#[test]
fn go_env_beats_env_file_beats_default() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        go_build_cache(&mut fx, "envcache");
        go_build_cache(&mut fx, "filecache");
        go_modcache(&mut fx, "filemod");
        go_modcache(&mut fx, "gopath1/pkg/mod");
        let file = match os {
            Os::Linux => "home/.config/go/env",
            Os::Macos => "home/Library/Application Support/go/env",
            Os::Windows => "home/AppData/Roaming/go/env",
        };
        fx.file(
            file,
            format!(
                "GOCACHE={}\nGOMODCACHE={}\n",
                abs(&fx, "filecache"),
                abs(&fx, "filemod")
            ),
        )
        .unwrap();
        fx.checkpoint().unwrap();

        // The env file (`go env -w`) applies when the environment says nothing.
        let scan = locate(&env(&fx, os, &[]));
        assert_eq!(
            found(&fx, &scan),
            [
                (CacheTool::GoBuild, "filecache".to_owned()),
                (CacheTool::GoMod, "filemod".to_owned())
            ],
            "{os:?}"
        );
        assert!(matches!(
            &the(&scan, CacheTool::GoBuild).source,
            CacheSource::Config { key, .. } if key == "GOCACHE"
        ));

        // The environment beats the file; GOPATH gives `<first entry>/pkg/mod`.
        let sep = if os == Os::Windows { ";" } else { ":" };
        // (A Unix layout checked on a Windows host can't hold drive-letter paths in a
        // `:`-separated list: one entry there.)
        let gopath = if cfg!(windows) && os != Os::Windows {
            abs(&fx, "gopath1")
        } else {
            format!("{}{sep}{}", abs(&fx, "gopath1"), abs(&fx, "gopath2"))
        };
        let scan = locate(&env(
            &fx,
            os,
            &[
                ("GOCACHE", abs(&fx, "envcache")),
                ("GOPATH", gopath),
                ("GOENV", "off".into()),
            ],
        ));
        assert_eq!(
            found(&fx, &scan),
            [
                (CacheTool::GoBuild, "envcache".to_owned()),
                (CacheTool::GoMod, "gopath1/pkg/mod".to_owned())
            ],
            "{os:?}"
        );
        assert_eq!(
            the(&scan, CacheTool::GoMod).source,
            CacheSource::Env {
                var: "GOPATH".into()
            }
        );

        // GOCACHE=off: the build cache is disabled, nothing is offered.
        let scan = locate(&env(&fx, os, &[("GOCACHE", "off".into())]));
        assert!(scan.found.iter().all(|c| c.tool != CacheTool::GoBuild));
        check(&fx, &plan_inputs(&[]));
    }
}

#[test]
fn go_modcache_at_a_gopath_root_or_home_is_refused() {
    let mut fx = Fixture::new().unwrap();
    go_modcache(&mut fx, "home/go/pkg/mod");
    fx.file("home/go/src/me/app/main.go", "package main\n")
        .unwrap();
    fx.checkpoint().unwrap();
    for (var, value) in [
        ("GOMODCACHE", abs(&fx, "home/go")),
        ("GOMODCACHE", abs(&fx, "home")),
        ("GOMODCACHE", "relative/mod".to_owned()),
        ("GOPATH", abs(&fx, "home/go") + "/../../.."),
    ] {
        let scan = locate(&env(&fx, Os::Linux, &[(var, value.clone())]));
        assert_eq!(
            refused_tools(&scan),
            [CacheTool::GoMod],
            "{var}={value}: {:?}",
            scan.found
        );
        assert!(scan.found.iter().all(|c| c.tool != CacheTool::GoMod));
    }
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn go_modcache_without_its_layout_is_s2_without_a_command() {
    let mut fx = Fixture::new().unwrap();
    fx.file("home/go/pkg/mod/notes.txt", "mine\n").unwrap();
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::GoMod).clone();
    assert!(!c.recognized);
    let cand = c.into_candidate(&idle(), None);
    assert!(cand.tier >= Tier::S2);
    assert_eq!(cand.action, Action::None);
    check(&fx, &plan_inputs(&[cand]));
}

// ---------------------------------------------------------------------------------------------
// Gradle and Maven: located, measured, inspect-only

#[test]
fn gradle_and_maven_caches_are_inspect_only() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        gradle_caches(&mut fx, "home/.gradle/caches");
        maven_repository(&mut fx, "home/.m2/repository");
        fx.checkpoint().unwrap();

        let scan = locate(&env(&fx, os, &[]));
        assert_eq!(
            found(&fx, &scan),
            [
                (CacheTool::Gradle, "home/.gradle/caches".to_owned()),
                (CacheTool::Maven, "home/.m2/repository".to_owned())
            ],
            "{os:?}"
        );
        let mut candidates = Vec::new();
        for c in scan.found {
            assert!(c.recognized, "{} recognized", c.tool);
            assert!(c.commands().is_none());
            let tool = c.tool;
            let cand = c.into_candidate(&idle(), None);
            assert!(cand.is_consistent());
            assert_eq!(cand.tier, Tier::S3, "{tool}: inspect-only");
            assert_eq!(cand.action, Action::None);
            assert!(
                cand.reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::InspectOnly)
            );
            assert!(
                notes(&cand)
                    .iter()
                    .any(|n| n.contains("has no command that cleans")),
                "{:?}",
                notes(&cand)
            );
            candidates.push(cand);
        }
        assert!(
            candidates.iter().all(|c| !c.action.is_mutating()),
            "never planned"
        );
        check(&fx, &plan_inputs(&candidates));
    }
}

#[test]
fn gradle_and_maven_overrides() {
    let mut fx = Fixture::new().unwrap();
    gradle_caches(&mut fx, "gradle-home/caches");
    maven_repository(&mut fx, "home/m2repo");
    maven_repository(&mut fx, "opts-repo");
    fx.file(
        "home/.m2/settings.xml",
        "<settings>\n  <!-- <localRepository>/commented</localRepository> -->\n  \
         <localRepository>${user.home}/m2repo</localRepository>\n</settings>\n",
    )
    .unwrap();
    fx.checkpoint().unwrap();

    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[("GRADLE_USER_HOME", abs(&fx, "gradle-home"))],
    ));
    assert_eq!(
        found(&fx, &scan),
        [
            (CacheTool::Gradle, "gradle-home/caches".to_owned()),
            (CacheTool::Maven, "home/m2repo".to_owned())
        ]
    );
    assert!(matches!(
        &the(&scan, CacheTool::Maven).source,
        CacheSource::Config { key, .. } if key == "localRepository"
    ));

    // `-Dmaven.repo.local` in MAVEN_OPTS beats settings.xml.
    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[(
            "MAVEN_OPTS",
            format!("-Xmx1g -Dmaven.repo.local={}", abs(&fx, "opts-repo")),
        )],
    ));
    assert_eq!(
        the(&scan, CacheTool::Maven).path,
        fx.path("opts-repo").unwrap()
    );

    // GRADLE_USER_HOME at the home folder itself is refused (T-23).
    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[("GRADLE_USER_HOME", abs(&fx, "home"))],
    ));
    assert!(refused_tools(&scan).contains(&CacheTool::Gradle));
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// NuGet

#[test]
fn nuget_packages_default_overrides_and_command() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        nuget_packages(&mut fx, "home/.nuget/packages");
        nuget_packages(&mut fx, "envpkgs");
        fx.checkpoint().unwrap();

        let scan = locate(&env(&fx, os, &[]));
        assert_eq!(
            found(&fx, &scan),
            [(CacheTool::NuGet, "home/.nuget/packages".to_owned())],
            "{os:?}"
        );
        let c = the(&scan, CacheTool::NuGet).clone();
        assert!(c.recognized);
        assert!(
            c.markers
                .iter()
                .any(|m| m.ends_with("newtonsoft.json/13.0.3/.nupkg.metadata")),
            "{:?}",
            c.markers
        );
        let cand = c.into_candidate(&idle(), None);
        assert!(cand.is_consistent());
        assert_eq!(cand.tier, expected(Tier::S1));
        if identity_available() {
            assert_eq!(
                cand.action,
                tool_action("dotnet", &["nuget", "locals", "global-packages", "--clear"])
            );
        }

        let scan = locate(&env(&fx, os, &[("NUGET_PACKAGES", abs(&fx, "envpkgs"))]));
        assert_eq!(
            found(&fx, &scan),
            [(CacheTool::NuGet, "envpkgs".to_owned())],
            "{os:?}"
        );
        check(&fx, &plan_inputs(&[cand]));
    }
}

#[test]
fn nuget_user_config_relative_value_and_unreadable_config() {
    for os in ALL_OS {
        let mut fx = Fixture::new().unwrap();
        let config_dir = match os {
            Os::Windows => "home/AppData/Roaming/NuGet",
            Os::Linux | Os::Macos => "home/.nuget/NuGet",
        };
        nuget_packages(&mut fx, &format!("{config_dir}/pkgs"));
        fx.file(
            format!("{config_dir}/NuGet.Config"),
            "<configuration><config>\
             <add key=\"globalPackagesFolder\" value=\"pkgs\" />\
             </config></configuration>",
        )
        .unwrap();
        // A project NuGet.Config is never read (T-23).
        fx.file(
            "proj/NuGet.Config",
            "<configuration><config><add key=\"globalPackagesFolder\" value=\"/\" /></config></configuration>",
        )
        .unwrap();
        fx.checkpoint().unwrap();

        let scan = locate(&env(&fx, os, &[]));
        assert_eq!(
            found(&fx, &scan),
            [(CacheTool::NuGet, format!("{config_dir}/pkgs"))],
            "{os:?}: relative to the config file's folder"
        );
        check(&fx, &plan_inputs(&[]));
    }

    // A NuGet.Config that can't be read as a small regular file refuses the tool.
    let mut fx = Fixture::new().unwrap();
    nuget_packages(&mut fx, "home/.nuget/packages");
    fx.dir("home/.nuget/NuGet/NuGet.Config").unwrap();
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert_eq!(refused_tools(&scan), [CacheTool::NuGet]);
    check(&fx, &plan_inputs(&[]));
}
