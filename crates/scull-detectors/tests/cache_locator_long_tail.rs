//! Long-tail tool caches in the locator (P5-DET-05, PRD §7.1, §10.8): ccache, Conan, vcpkg,
//! PlatformIO, Arduino, ESP-IDF, SwiftPM, CocoaPods, Coursier, Deno, Zig, Playwright, Cypress,
//! Puppeteer, Electron, Homebrew and Nix. Environment and user-config overrides (injected
//! `CacheEnv`), the native commands chosen in Rust, the inspect-only tools, and T-23 refusals.
//! The default location of every tool on every OS layout is covered by `cache_locator.rs`
//! (FX-QC-01).
//!
//! Nothing reads the real home folder or environment and no tool is ever run. The universal
//! invariants (I-2 canary, I-3 tree unchanged, I-1 on the would-be plan) are checked after each
//! test.

#![allow(clippy::unwrap_used)]

mod common;

use common::tool_caches::{LONG_TAIL_TOOLS, build};
use common::{StdReader, check, identity_available, plan_inputs, rel};
use scull_core::{Action, Candidate, Evidence, ReasonCode, Tier};
use scull_detectors::caches::{CacheScan, CacheSource, CacheTool};
use scull_detectors::rules::Platform as Os;
use scull_detectors::{CacheEnv, CallerSignals, DetectContext, LocatedCache, locate_caches};
use scull_fixtures::Fixture;
use scull_platform::Native;
use scull_safety::{GitTracking, Tri};

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

/// Long-tail tools found, as `(tool, path relative to the fixture root)`.
fn found(fx: &Fixture, scan: &CacheScan) -> Vec<(CacheTool, String)> {
    scan.found
        .iter()
        .filter(|c| LONG_TAIL_TOOLS.contains(&c.tool))
        .map(|c| (c.tool, rel(fx.root(), &c.path)))
        .collect()
}

fn the(scan: &CacheScan, tool: CacheTool) -> &LocatedCache {
    let hits: Vec<_> = scan.found.iter().filter(|c| c.tool == tool).collect();
    assert_eq!(hits.len(), 1, "exactly one {tool} in {:?}", scan.found);
    hits[0]
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
// Commands and inspect-only tools

#[test]
fn native_commands_are_chosen_in_rust_and_the_rest_is_inspect_only() {
    let cmd = |t: CacheTool| {
        scull_detectors::commands_for_rule(t.rule_id()).map(|k| {
            (
                k.preferred.to_string(),
                k.preferred_prunes,
                k.wipe.map(|w| w.to_string()),
            )
        })
    };
    let s = |x: &str| x.to_owned();
    assert_eq!(
        cmd(CacheTool::Ccache),
        Some((s("ccache --clear"), false, None))
    );
    assert_eq!(
        cmd(CacheTool::Conan),
        Some((s("conan cache clean"), true, None))
    );
    assert_eq!(
        cmd(CacheTool::Arduino),
        Some((s("arduino-cli cache clean"), false, None))
    );
    assert_eq!(
        cmd(CacheTool::CocoaPods),
        Some((s("pod cache clean --all"), false, None))
    );
    assert_eq!(cmd(CacheTool::Deno), Some((s("deno clean"), false, None)));
    assert_eq!(
        cmd(CacheTool::Playwright),
        Some((s("playwright uninstall --all"), false, None))
    );
    assert_eq!(
        cmd(CacheTool::Cypress),
        Some((
            s("cypress cache prune"),
            true,
            Some(s("cypress cache clear"))
        ))
    );
    assert_eq!(
        cmd(CacheTool::Puppeteer),
        Some((s("puppeteer browsers clear"), false, None))
    );
    assert_eq!(
        cmd(CacheTool::Homebrew),
        Some((s("brew cleanup"), true, None))
    );
    for t in [
        CacheTool::Vcpkg,
        CacheTool::PlatformIo,
        CacheTool::EspIdf,
        CacheTool::SwiftPm,
        CacheTool::Coursier,
        CacheTool::Zig,
        CacheTool::Electron,
        CacheTool::Nix,
    ] {
        assert_eq!(cmd(t), None, "{t} is inspect-only");
    }
    // Rule IDs are unique across the locator and never shadow a scan rule.
    let rules = scull_detectors::rules::RuleSet::bundled().unwrap();
    for (i, t) in CacheTool::ALL.iter().enumerate() {
        assert_eq!(CacheTool::from_rule_id(t.rule_id()), Some(*t));
        assert!(
            rules.get(t.rule_id()).is_none(),
            "{} is also a scan rule",
            t.rule_id()
        );
        assert!(
            CacheTool::ALL[..i]
                .iter()
                .all(|o| o.rule_id() != t.rule_id())
        );
    }
}

#[test]
fn inspect_only_caches_say_why_and_are_never_planned() {
    let mut fx = Fixture::new().unwrap();
    let dirs = [
        (
            CacheTool::PlatformIo,
            "home/.platformio/.cache",
            "pio system prune",
        ),
        (CacheTool::EspIdf, "home/.espressif/dist", "idf_tools.py"),
        (
            CacheTool::SwiftPm,
            "home/.cache/org.swift.swiftpm",
            "purge-cache",
        ),
        (
            CacheTool::Coursier,
            "home/.cache/coursier/v1",
            "cs has no command",
        ),
        (CacheTool::Zig, "home/.cache/zig", "zig has no command"),
        (
            CacheTool::Vcpkg,
            "home/.cache/vcpkg/archives",
            "vcpkg has no command",
        ),
        (
            CacheTool::Electron,
            "home/.cache/electron",
            "electron has no command",
        ),
        (CacheTool::Nix, "nix/store", "nix-collect-garbage"),
    ];
    for (t, d, _) in dirs {
        build(&mut fx, t, d);
    }
    fx.checkpoint().unwrap();
    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[("NIX_STORE_DIR", abs(&fx, "nix/store"))],
    ));
    assert!(scan.refused.is_empty(), "{:?}", scan.refused);
    let mut cands = Vec::new();
    for (t, d, why) in dirs {
        let c = the(&scan, t);
        assert_eq!(rel(fx.root(), &c.path), d);
        assert!(c.recognized, "{t} layout recognized");
        let cand = c.clone().into_candidate(&idle(), None);
        assert!(cand.is_consistent());
        assert_eq!(cand.tier, Tier::S3, "{t}");
        assert_eq!(cand.action, Action::None);
        let inspect = cand
            .reasons
            .iter()
            .find(|r| r.code == ReasonCode::InspectOnly)
            .unwrap();
        assert!(
            inspect.message.contains(why) || notes(&cand).iter().any(|n| n.contains(why)),
            "{t}: {inspect:?} / {:?}",
            notes(&cand)
        );
        cands.push(cand);
    }
    check(&fx, &plan_inputs(&cands));
}

// ---------------------------------------------------------------------------------------------
// Environment overrides

#[test]
fn environment_overrides_every_tool() {
    let mut fx = Fixture::new().unwrap();
    // (tool, variable, value relative to the fixture root, cache folder relative to it)
    let table = [
        (CacheTool::Ccache, "CCACHE_DIR", "x/ccache", "x/ccache"),
        (CacheTool::Conan, "CONAN_HOME", "x/conan", "x/conan/p"),
        (
            CacheTool::Vcpkg,
            "VCPKG_DEFAULT_BINARY_CACHE",
            "x/vcpkg",
            "x/vcpkg",
        ),
        (
            CacheTool::PlatformIo,
            "PLATFORMIO_CORE_DIR",
            "x/pio",
            "x/pio/.cache",
        ),
        (
            CacheTool::Arduino,
            "ARDUINO_DIRECTORIES_DATA",
            "x/arduino",
            "x/arduino/staging",
        ),
        (CacheTool::EspIdf, "IDF_TOOLS_PATH", "x/esp", "x/esp/dist"),
        (CacheTool::Coursier, "COURSIER_CACHE", "x/cs", "x/cs"),
        (CacheTool::Deno, "DENO_DIR", "x/deno", "x/deno"),
        (CacheTool::Zig, "ZIG_GLOBAL_CACHE_DIR", "x/zig", "x/zig"),
        (
            CacheTool::Playwright,
            "PLAYWRIGHT_BROWSERS_PATH",
            "x/pw",
            "x/pw",
        ),
        (CacheTool::Cypress, "CYPRESS_CACHE_FOLDER", "x/cy", "x/cy"),
        (CacheTool::Puppeteer, "PUPPETEER_CACHE_DIR", "x/pp", "x/pp"),
        (CacheTool::Electron, "electron_config_cache", "x/el", "x/el"),
        (CacheTool::Homebrew, "HOMEBREW_CACHE", "x/brew", "x/brew"),
        (
            CacheTool::Nix,
            "NIX_STORE_DIR",
            "x/nix/store",
            "x/nix/store",
        ),
    ];
    for (t, _, _, dir) in table {
        build(&mut fx, t, dir);
    }
    // The defaults exist too and must be ignored once overridden.
    build(&mut fx, CacheTool::Deno, "home/.cache/deno");
    build(&mut fx, CacheTool::Cypress, "home/.cache/Cypress");
    fx.checkpoint().unwrap();
    let vars: Vec<(&str, String)> = table.iter().map(|(_, v, p, _)| (*v, abs(&fx, p))).collect();
    for os in [Os::Linux, Os::Macos, Os::Windows] {
        let scan = locate(&env(&fx, os, &vars));
        assert!(scan.refused.is_empty(), "{os:?}: {:?}", scan.refused);
        let want: Vec<(CacheTool, String)> = table
            .iter()
            .filter(|(t, ..)| !(os == Os::Windows && *t == CacheTool::Nix))
            .map(|(t, _, _, d)| (*t, (*d).to_owned()))
            .collect();
        // Nix's NIX_STORE_DIR is honoured wherever it is set (it's an absolute folder).
        let got: Vec<(CacheTool, String)> = found(&fx, &scan)
            .into_iter()
            .filter(|(t, _)| !(os == Os::Windows && *t == CacheTool::Nix))
            .collect();
        assert_eq!(got, want, "{os:?}");
        for (t, var, ..) in table {
            if os == Os::Windows && t == CacheTool::Nix {
                continue;
            }
            assert_eq!(
                the(&scan, t).source,
                CacheSource::Env { var: var.into() },
                "{t}"
            );
        }
    }
    // PLATFORMIO_CACHE_DIR names the cache itself, and wins over PLATFORMIO_CORE_DIR.
    let mut fx2 = Fixture::new().unwrap();
    build(&mut fx2, CacheTool::PlatformIo, "pio-cache");
    fx2.checkpoint().unwrap();
    let scan = locate(&env(
        &fx2,
        Os::Linux,
        &[
            ("PLATFORMIO_CACHE_DIR", abs(&fx2, "pio-cache")),
            ("PLATFORMIO_CORE_DIR", abs(&fx2, "elsewhere")),
        ],
    ));
    assert_eq!(
        rel(fx2.root(), &the(&scan, CacheTool::PlatformIo).path),
        "pio-cache"
    );
    check(&fx, &plan_inputs(&[]));
    check(&fx2, &plan_inputs(&[]));
}

#[test]
fn playwright_browsers_path_zero_means_no_global_cache() {
    let mut fx = Fixture::new().unwrap();
    build(&mut fx, CacheTool::Playwright, "home/.cache/ms-playwright");
    fx.checkpoint().unwrap();
    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[("PLAYWRIGHT_BROWSERS_PATH", "0".into())],
    ));
    assert!(scan.found.iter().all(|c| c.tool != CacheTool::Playwright));
    assert!(scan.refused.is_empty(), "{:?}", scan.refused);
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn nix_store_only_when_nix_is_set_up() {
    let mut fx = Fixture::new().unwrap();
    fx.checkpoint().unwrap();
    // Without Nix's environment the store is never looked up (the default is outside home).
    for os in [Os::Linux, Os::Macos, Os::Windows] {
        let scan = locate(&env(&fx, os, &[]));
        assert!(scan.found.iter().all(|c| c.tool != CacheTool::Nix));
    }
    // With it (and on Windows never), the default `/nix/store` is the spot; this machine's
    // store is not inspected here: the fixture's NIX_STORE_DIR test covers recognition.
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// ccache, Arduino, CocoaPods configuration

#[test]
fn ccache_legacy_folder_config_file_and_env() {
    // The legacy `~/.ccache` is used while it exists, instead of the XDG default.
    let mut fx = Fixture::new().unwrap();
    build(&mut fx, CacheTool::Ccache, "home/.ccache");
    build(&mut fx, CacheTool::Ccache, "home/.cache/ccache");
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::Ccache);
    assert_eq!(rel(fx.root(), &c.path), "home/.ccache");
    assert!(c.notes.iter().any(|n| n.contains("legacy")));
    check(&fx, &plan_inputs(&[]));

    // `cache_dir` in the user config file wins over the defaults; CCACHE_DIR over both.
    let mut fx = Fixture::new().unwrap();
    build(&mut fx, CacheTool::Ccache, "data/ccache");
    build(&mut fx, CacheTool::Ccache, "env/ccache");
    build(&mut fx, CacheTool::Ccache, "home/.ccache");
    let data = abs(&fx, "data/ccache");
    fx.file(
        "home/.config/ccache/ccache.conf",
        format!("# mine\nmax_size = 10G\ncache_dir = {data}\n"),
    )
    .unwrap();
    fx.file(
        "home/AppData/Roaming/ccache/ccache.conf",
        format!("cache_dir = {data}\n"),
    )
    .unwrap();
    fx.checkpoint().unwrap();
    for os in [Os::Linux, Os::Windows] {
        let scan = locate(&env(&fx, os, &[]));
        let c = the(&scan, CacheTool::Ccache);
        assert_eq!(rel(fx.root(), &c.path), "data/ccache", "{os:?}");
        assert!(matches!(&c.source, CacheSource::Config { key, .. } if key == "cache_dir"));
    }
    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[("CCACHE_DIR", abs(&fx, "env/ccache"))],
    ));
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::Ccache).path),
        "env/ccache"
    );
    check(&fx, &plan_inputs(&[]));

    // A config pointing at the home folder is refused (T-23): `ccache --clear` would act there.
    let mut fx = Fixture::new().unwrap();
    let home = abs(&fx, "home");
    fx.file(
        "home/.config/ccache/ccache.conf",
        format!("cache_dir = {home}\n"),
    )
    .unwrap();
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert!(scan.found.iter().all(|c| c.tool != CacheTool::Ccache));
    assert!(scan.refused.iter().any(|r| r.tool == CacheTool::Ccache));
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn arduino_config_that_moves_directories_is_refused() {
    let mut fx = Fixture::new().unwrap();
    build(&mut fx, CacheTool::Arduino, "home/.arduino15/staging");
    fx.file(
        "home/.arduino15/arduino-cli.yaml",
        "board_manager:\n  additional_urls: []\n",
    )
    .unwrap();
    fx.checkpoint().unwrap();
    // A config that doesn't move the folders: the default is used.
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::Arduino).clone();
    let cand = c.into_candidate(&idle(), None);
    assert_eq!(cand.tier, expected(Tier::S1));
    if identity_available() {
        assert_eq!(cand.action, tool_action("arduino-cli", &["cache", "clean"]));
    }
    check(&fx, &plan_inputs(&[cand]));

    let mut fx = Fixture::new().unwrap();
    build(&mut fx, CacheTool::Arduino, "home/.arduino15/staging");
    build(&mut fx, CacheTool::Arduino, "dl");
    fx.file(
        "home/.arduino15/arduino-cli.yaml",
        "directories:\n  data: /somewhere/else\n  downloads: /somewhere/else/staging\n",
    )
    .unwrap();
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert!(scan.found.iter().all(|c| c.tool != CacheTool::Arduino));
    let r = scan
        .refused
        .iter()
        .find(|r| r.tool == CacheTool::Arduino)
        .unwrap();
    assert!(
        r.reason.contains("sets the Arduino directories"),
        "{}",
        r.reason
    );
    // The environment variable wins over the config file, so the location is known again.
    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[("ARDUINO_DIRECTORIES_DOWNLOADS", abs(&fx, "dl"))],
    ));
    assert_eq!(rel(fx.root(), &the(&scan, CacheTool::Arduino).path), "dl");
    check(&fx, &plan_inputs(&[]));
}

#[test]
fn cocoapods_cache_root_from_config_and_macos_default() {
    let mut fx = Fixture::new().unwrap();
    build(
        &mut fx,
        CacheTool::CocoaPods,
        "home/Library/Caches/CocoaPods",
    );
    build(&mut fx, CacheTool::CocoaPods, "pods-cache");
    fx.checkpoint().unwrap();
    // Default: macOS only.
    let scan = locate(&env(&fx, Os::Macos, &[]));
    assert_eq!(
        rel(fx.root(), &the(&scan, CacheTool::CocoaPods).path),
        "home/Library/Caches/CocoaPods"
    );
    let scan = locate(&env(&fx, Os::Linux, &[]));
    assert!(scan.found.iter().all(|c| c.tool != CacheTool::CocoaPods));
    // `cache_root` in ~/.cocoapods/config.yaml moves it (on any OS).
    let p = abs(&fx, "pods-cache");
    fx.file("home/.cocoapods/config.yaml", format!("cache_root: {p}\n"))
        .unwrap();
    fx.checkpoint().unwrap();
    for os in [Os::Linux, Os::Macos] {
        let scan = locate(&env(&fx, os, &[]));
        let c = the(&scan, CacheTool::CocoaPods);
        assert_eq!(rel(fx.root(), &c.path), "pods-cache", "{os:?}");
        assert!(matches!(&c.source, CacheSource::Config { key, .. } if key == "cache_root"));
    }
    check(&fx, &plan_inputs(&[]));
}

// ---------------------------------------------------------------------------------------------
// Tiering

#[test]
fn deno_local_storage_makes_the_cache_s2_without_a_command() {
    let mut fx = Fixture::new().unwrap();
    build(&mut fx, CacheTool::Deno, "home/.cache/deno");
    fx.file("home/.cache/deno/location_data/0a1b/local_storage", "kv")
        .unwrap();
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::Deno).clone();
    assert_eq!(c.non_cache_data.len(), 1);
    let cand = c.into_candidate(&idle(), None);
    assert!(cand.is_consistent());
    assert_eq!(cand.tier, expected(Tier::S2));
    assert_eq!(cand.action, Action::None, "no tool command at S2");
    assert!(notes(&cand).iter().any(|n| n.contains("localStorage")));
    check(&fx, &plan_inputs(&[cand]));
}

#[test]
fn recognized_tool_caches_are_s1_with_their_command() {
    let mut fx = Fixture::new().unwrap();
    let table = [
        (
            CacheTool::Ccache,
            "home/.cache/ccache",
            "ccache",
            &["--clear"][..],
        ),
        (
            CacheTool::Conan,
            "home/.conan2/p",
            "conan",
            &["cache", "clean"][..],
        ),
        (CacheTool::Deno, "home/.cache/deno", "deno", &["clean"][..]),
        (
            CacheTool::Cypress,
            "home/.cache/Cypress",
            "cypress",
            &["cache", "prune"][..],
        ),
        (
            CacheTool::Homebrew,
            "home/.cache/Homebrew",
            "brew",
            &["cleanup"][..],
        ),
    ];
    for (t, d, ..) in table {
        build(&mut fx, t, d);
    }
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let mut cands = Vec::new();
    for (t, _, tool, args) in table {
        let cand = the(&scan, t).clone().into_candidate(&idle(), None);
        assert!(cand.is_consistent());
        assert_eq!(cand.tier, expected(Tier::S1), "{t}");
        assert!(
            cand.reasons
                .iter()
                .any(|r| r.code == ReasonCode::PackageCache)
        );
        if identity_available() {
            assert_eq!(cand.action, tool_action(tool, args), "{t}");
        }
        cands.push(cand);
    }
    let brew = cands.last().unwrap();
    assert!(
        notes(brew)
            .iter()
            .any(|n| n.contains("old versions of installed formulae"))
    );
    check(&fx, &plan_inputs(&cands));
}

#[test]
fn unrecognized_layout_is_s2_and_t23_refusals() {
    let mut fx = Fixture::new().unwrap();
    // A Cypress cache folder holding something else entirely.
    fx.file("home/.cache/Cypress/notes.txt", "mine").unwrap();
    fx.checkpoint().unwrap();
    let scan = locate(&env(&fx, Os::Linux, &[]));
    let c = the(&scan, CacheTool::Cypress).clone();
    assert!(!c.recognized);
    let cand = c.into_candidate(&idle(), None);
    assert_eq!(cand.tier, expected(Tier::S2));
    assert_eq!(cand.action, Action::None);

    // Relative, home, a shared folder: refused, nothing of the tool is offered.
    let home = abs(&fx, "home");
    let shared = abs(&fx, "home/.cache");
    let scan = locate(&env(
        &fx,
        Os::Linux,
        &[
            ("DENO_DIR", "relative/deno".into()),
            ("CYPRESS_CACHE_FOLDER", home),
            ("PUPPETEER_CACHE_DIR", shared),
        ],
    ));
    for t in [CacheTool::Deno, CacheTool::Cypress, CacheTool::Puppeteer] {
        assert!(scan.found.iter().all(|c| c.tool != t), "{t}");
        assert!(scan.refused.iter().any(|r| r.tool == t), "{t} refused");
    }
    check(&fx, &plan_inputs(&[cand]));
}
