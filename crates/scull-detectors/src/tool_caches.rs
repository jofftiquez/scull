//! Long-tail tool caches (P5-DET-05, PRD §7.1, §10.8): where they are and what their layout
//! looks like. Classification, validation and commands are shared with every other cache in
//! [`crate::caches`].
//!
//! | Rule | Tool | Overrides, highest first | Default (Linux / macOS / Windows) | Cleanup |
//! |---|---|---|---|---|
//! | `ccache.cache` | ccache | `CCACHE_DIR`; `cache_dir` in the ccache config (`CCACHE_CONFIGPATH`, else `$XDG_CONFIG_HOME/ccache/ccache.conf` or `~/.config/ccache/ccache.conf` / `~/Library/Preferences/ccache/ccache.conf` / `%AppData%\ccache\ccache.conf`) | the legacy `~/.ccache` when it exists, else `$XDG_CACHE_HOME/ccache` or `~/.cache/ccache` / `~/Library/Caches/ccache` / `%LocalAppData%\ccache` | `ccache --clear` |
//! | `conan.cache` | Conan 2 | `CONAN_HOME` (the cache is its `p/`) | `~/.conan2/p` | `conan cache clean` (temporary folders only) |
//! | `vcpkg.binary-cache` | vcpkg | `VCPKG_DEFAULT_BINARY_CACHE` | `$XDG_CACHE_HOME/vcpkg/archives` or `~/.cache/vcpkg/archives` (Linux, macOS) / `%LocalAppData%\vcpkg\archives` | inspect-only |
//! | `platformio.cache` | PlatformIO | `PLATFORMIO_CACHE_DIR`; `$PLATFORMIO_CORE_DIR/.cache` | `~/.platformio/.cache` | inspect-only |
//! | `arduino.staging` | Arduino CLI / IDE 2 | `ARDUINO_DIRECTORIES_DOWNLOADS`; `$ARDUINO_DIRECTORIES_DATA/staging`; refused when `arduino-cli.yaml` sets directories | `~/.arduino15/staging` / `~/Library/Arduino15/staging` / `%LocalAppData%\Arduino15\staging` | `arduino-cli cache clean` |
//! | `espidf.dist` | ESP-IDF | `IDF_TOOLS_PATH` (the archives are its `dist/`) | `~/.espressif/dist` | inspect-only |
//! | `swiftpm.cache` | SwiftPM | — | `$XDG_CACHE_HOME/org.swift.swiftpm` or `~/.cache/org.swift.swiftpm` / `~/Library/Caches/org.swift.swiftpm` / — | inspect-only |
//! | `cocoapods.cache` | CocoaPods | `cache_root` in `config.yaml` of `CP_HOME_DIR` or `~/.cocoapods` | — / `~/Library/Caches/CocoaPods` / — | `pod cache clean --all` |
//! | `coursier.cache` | Coursier (sbt, Mill, Metals) | `COURSIER_CACHE` | `$XDG_CACHE_HOME/coursier/v1` or `~/.cache/coursier/v1` / `~/Library/Caches/Coursier/v1` / `%LocalAppData%\Coursier\Cache\v1` | inspect-only |
//! | `deno.cache` | Deno | `DENO_DIR` | `$XDG_CACHE_HOME/deno` or `~/.cache/deno` / `~/Library/Caches/deno` / `%LocalAppData%\deno` | `deno clean` |
//! | `zig.global-cache` | Zig | `ZIG_GLOBAL_CACHE_DIR` | `$XDG_CACHE_HOME/zig` or `~/.cache/zig` (Linux, macOS) / `%LocalAppData%\zig` | inspect-only |
//! | `playwright.browsers` | Playwright | `PLAYWRIGHT_BROWSERS_PATH` (`0`: browsers live in each project, nothing global) | `$XDG_CACHE_HOME/ms-playwright` or `~/.cache/ms-playwright` / `~/Library/Caches/ms-playwright` / `%LocalAppData%\ms-playwright` | `playwright uninstall --all` |
//! | `cypress.cache` | Cypress | `CYPRESS_CACHE_FOLDER` | `$XDG_CACHE_HOME/Cypress` or `~/.cache/Cypress` / `~/Library/Caches/Cypress` / `%LocalAppData%\Cypress\Cache` | `cypress cache prune` (wipe: `cypress cache clear`) |
//! | `puppeteer.browsers` | Puppeteer | `PUPPETEER_CACHE_DIR` | `~/.cache/puppeteer` | `puppeteer browsers clear` |
//! | `electron.cache` | Electron downloads | `electron_config_cache` | `$XDG_CACHE_HOME/electron` or `~/.cache/electron` / `~/Library/Caches/electron` / `%LocalAppData%\electron\Cache` | inspect-only |
//! | `homebrew.cache` | Homebrew | `HOMEBREW_CACHE` | `$XDG_CACHE_HOME/Homebrew` or `~/.cache/Homebrew` / `~/Library/Caches/Homebrew` / — | `brew cleanup` |
//! | `nix.store` | Nix | `NIX_STORE_DIR`; `/nix/store` when Nix is set up (`NIX_PROFILES` or `NIX_PATH`) | `/nix/store` (Linux, macOS) | inspect-only (`nix-collect-garbage` is user-run) |
//!
//! "Inspect-only" means located, measured and shown at S3 with no action: either the tool has
//! no command that cleans exactly this folder, or its command can't be pointed at it (SwiftPM's
//! `purge-cache` needs a package, PlatformIO's prune also removes platforms, ESP-IDF's
//! `idf_tools.py` needs an ESP-IDF checkout, Nix garbage collection is the user's call).

use crate::caches::{
    CacheEnv, CacheSource, CacheTool, Spot, Spots, UserRc, configured, parse_yarnrc_yml, refused,
};
use crate::detector::{DetectContext, Listing};
use crate::rules::Platform as Os;
use scull_platform::{EntryKind, Platform};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The spots of a long-tail tool, or `None` if `tool` isn't one.
pub(crate) fn spots(
    tool: CacheTool,
    env: &CacheEnv,
    platform: &dyn Platform,
    ctx: &DetectContext<'_>,
) -> Option<Spots> {
    let xdg = |sub: &str| env.xdg_cache().join(sub);
    let mac = |sub: &str| env.home.join("Library").join("Caches").join(sub);
    let lad = |sub: &str| env.local_app_data().join(sub);
    Some(match tool {
        CacheTool::Ccache => ccache_spots(env, platform, ctx),
        CacheTool::Conan => Ok(vec![
            from_var_or(env, tool, &["CONAN_HOME"], env.home.join(".conan2")).sub("p"),
        ]),
        CacheTool::Vcpkg => Ok(vec![from_var_or(
            env,
            tool,
            &["VCPKG_DEFAULT_BINARY_CACHE"],
            match env.os {
                Os::Windows => lad("vcpkg").join("archives"),
                Os::Linux | Os::Macos => xdg("vcpkg").join("archives"),
            },
        )]),
        CacheTool::PlatformIo => Ok(vec![platformio_spot(env)]),
        CacheTool::Arduino => arduino_spots(env, platform, ctx),
        CacheTool::EspIdf => Ok(vec![
            from_var_or(env, tool, &["IDF_TOOLS_PATH"], env.home.join(".espressif")).sub("dist"),
        ]),
        CacheTool::SwiftPm => Ok(match env.os {
            Os::Linux => vec![Spot::new(
                tool,
                xdg("org.swift.swiftpm"),
                CacheSource::Default,
            )],
            Os::Macos => vec![Spot::new(
                tool,
                mac("org.swift.swiftpm"),
                CacheSource::Default,
            )],
            Os::Windows => Vec::new(),
        }),
        CacheTool::CocoaPods => cocoapods_spots(env, platform, ctx),
        CacheTool::Coursier => Ok(vec![from_var_or(
            env,
            tool,
            &["COURSIER_CACHE"],
            match env.os {
                Os::Linux => xdg("coursier").join("v1"),
                Os::Macos => mac("Coursier").join("v1"),
                Os::Windows => lad("Coursier").join("Cache").join("v1"),
            },
        )]),
        CacheTool::Deno => Ok(vec![from_var_or(
            env,
            tool,
            &["DENO_DIR"],
            by_os(env, xdg("deno"), mac("deno"), lad("deno")),
        )]),
        CacheTool::Zig => Ok(vec![from_var_or(
            env,
            tool,
            &["ZIG_GLOBAL_CACHE_DIR"],
            match env.os {
                Os::Windows => lad("zig"),
                Os::Linux | Os::Macos => xdg("zig"),
            },
        )]),
        CacheTool::Playwright => Ok(
            if env.var("PLAYWRIGHT_BROWSERS_PATH").as_deref() == Some("0") {
                Vec::new() // browsers are installed inside each project's node_modules
            } else {
                vec![from_var_or(
                    env,
                    tool,
                    &["PLAYWRIGHT_BROWSERS_PATH"],
                    by_os(
                        env,
                        xdg("ms-playwright"),
                        mac("ms-playwright"),
                        lad("ms-playwright"),
                    ),
                )]
            },
        ),
        CacheTool::Cypress => Ok(vec![from_var_or(
            env,
            tool,
            &["CYPRESS_CACHE_FOLDER"],
            by_os(
                env,
                xdg("Cypress"),
                mac("Cypress"),
                lad("Cypress").join("Cache"),
            ),
        )]),
        CacheTool::Puppeteer => Ok(vec![from_var_or(
            env,
            tool,
            &["PUPPETEER_CACHE_DIR"],
            env.home.join(".cache").join("puppeteer"),
        )]),
        CacheTool::Electron => Ok(vec![from_var_or(
            env,
            tool,
            &["electron_config_cache"],
            by_os(
                env,
                xdg("electron"),
                mac("electron"),
                lad("electron").join("Cache"),
            ),
        )]),
        CacheTool::Homebrew => Ok(match (env.first_var(&["HOMEBREW_CACHE"]), env.os) {
            (Some((var, v)), _) => {
                vec![Spot::new(tool, PathBuf::from(v), CacheSource::Env { var })]
            }
            (None, Os::Linux) => vec![Spot::new(tool, xdg("Homebrew"), CacheSource::Default)],
            (None, Os::Macos) => vec![Spot::new(tool, mac("Homebrew"), CacheSource::Default)],
            (None, Os::Windows) => Vec::new(),
        }),
        CacheTool::Nix => Ok(nix_spots(env)),
        _ => return None,
    })
}

fn by_os(env: &CacheEnv, linux: PathBuf, macos: PathBuf, windows: PathBuf) -> PathBuf {
    match env.os {
        Os::Linux => linux,
        Os::Macos => macos,
        Os::Windows => windows,
    }
}

/// The first set variable of `vars`, taken literally (relative values are refused by
/// validation), else `default`.
fn from_var_or(env: &CacheEnv, tool: CacheTool, vars: &[&str], default: PathBuf) -> Spot {
    match env.first_var(vars) {
        Some((var, v)) => Spot::new(tool, PathBuf::from(v), CacheSource::Env { var }),
        None => Spot::new(tool, default, CacheSource::Default),
    }
}

/// `key = value` lines of a ccache configuration file (`#` starts a comment line).
pub(crate) fn parse_ccache_conf(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

fn ccache_spots(env: &CacheEnv, platform: &dyn Platform, ctx: &DetectContext<'_>) -> Spots {
    let tool = CacheTool::Ccache;
    if let Some((var, v)) = env.first_var(&["CCACHE_DIR"]) {
        return Ok(vec![Spot::new(
            tool,
            PathBuf::from(v),
            CacheSource::Env { var },
        )]);
    }
    let conf = match env.var("CCACHE_CONFIGPATH") {
        Some(p) => PathBuf::from(p),
        None => match env.os {
            Os::Linux => env.xdg_config().join("ccache").join("ccache.conf"),
            Os::Macos => env
                .abs_var("XDG_CONFIG_HOME")
                .map(|x| x.join("ccache"))
                .unwrap_or_else(|| env.home.join("Library").join("Preferences").join("ccache"))
                .join("ccache.conf"),
            Os::Windows => env.roaming_app_data().join("ccache").join("ccache.conf"),
        },
    };
    let rc = if conf.is_absolute() {
        UserRc::read(conf, platform, ctx)
    } else {
        UserRc::Unreadable { file: conf }
    };
    if let Some((file, v)) = rc.lookup(tool, "cache_dir", parse_ccache_conf)? {
        let source = CacheSource::Config {
            file,
            key: "cache_dir".into(),
        };
        let (root, source) = configured(env, tool, &v, source)?;
        return Ok(vec![Spot::new(tool, root, source)]);
    }
    // ccache keeps using the legacy folder when it exists.
    let legacy = env.home.join(".ccache");
    if platform
        .metadata(&legacy)
        .is_ok_and(|m| m.kind == EntryKind::Dir)
    {
        let mut s = Spot::new(tool, legacy, CacheSource::Default);
        s.notes
            .push("legacy ccache location (`~/.ccache`), which ccache uses while it exists".into());
        return Ok(vec![s]);
    }
    let root = match env.os {
        Os::Linux => env.xdg_cache().join("ccache"),
        Os::Macos => env
            .abs_var("XDG_CACHE_HOME")
            .map(|x| x.join("ccache"))
            .unwrap_or_else(|| env.home.join("Library").join("Caches").join("ccache")),
        Os::Windows => env.local_app_data().join("ccache"),
    };
    Ok(vec![Spot::new(tool, root, CacheSource::Default)])
}

fn platformio_spot(env: &CacheEnv) -> Spot {
    let tool = CacheTool::PlatformIo;
    if let Some((var, v)) = env.first_var(&["PLATFORMIO_CACHE_DIR"]) {
        return Spot::new(tool, PathBuf::from(v), CacheSource::Env { var });
    }
    from_var_or(
        env,
        tool,
        &["PLATFORMIO_CORE_DIR"],
        env.home.join(".platformio"),
    )
    .sub(".cache")
}

fn arduino_spots(env: &CacheEnv, platform: &dyn Platform, ctx: &DetectContext<'_>) -> Spots {
    let tool = CacheTool::Arduino;
    if let Some((var, v)) = env.first_var(&["ARDUINO_DIRECTORIES_DOWNLOADS"]) {
        return Ok(vec![Spot::new(
            tool,
            PathBuf::from(v),
            CacheSource::Env { var },
        )]);
    }
    let data = from_var_or(
        env,
        tool,
        &["ARDUINO_DIRECTORIES_DATA"],
        match env.os {
            Os::Linux => env.home.join(".arduino15"),
            Os::Macos => env.home.join("Library").join("Arduino15"),
            Os::Windows => env.local_app_data().join("Arduino15"),
        },
    );
    // The configuration file can move the folders too. Its YAML isn't resolved here: if it
    // sets any directory, the effective location is unknown and the tool is refused (T-23).
    let conf = match env.var("ARDUINO_CONFIG_FILE") {
        Some(p) => PathBuf::from(p),
        None => data.root.join("arduino-cli.yaml"),
    };
    let rc = if conf.is_absolute() {
        UserRc::read(conf, platform, ctx)
    } else {
        UserRc::Unreadable { file: conf }
    };
    let source = |file: &PathBuf| CacheSource::Config {
        file: file.clone(),
        key: "directories".into(),
    };
    match &rc {
        UserRc::Absent => {}
        UserRc::Text { file, text } => {
            let sets_dirs = text.lines().any(|l| {
                let t = l.trim_start();
                t.starts_with("downloads:") || t.starts_with("data:")
            });
            if sets_dirs {
                return Err(refused(
                    tool,
                    None,
                    source(file),
                    format!(
                        "{} sets the Arduino directories; scull doesn't resolve them, so the \
                         effective download cache is unknown",
                        file.display()
                    ),
                ));
            }
        }
        UserRc::Unreadable { file } => {
            return Err(refused(
                tool,
                None,
                source(file),
                format!(
                    "{} exists but can't be read as a small regular file, so the effective \
                     download cache is unknown",
                    file.display()
                ),
            ));
        }
    }
    Ok(vec![data.sub("staging")])
}

fn cocoapods_spots(env: &CacheEnv, platform: &dyn Platform, ctx: &DetectContext<'_>) -> Spots {
    let tool = CacheTool::CocoaPods;
    let home = env
        .abs_var("CP_HOME_DIR")
        .unwrap_or_else(|| env.home.join(".cocoapods"));
    let rc = UserRc::read(home.join("config.yaml"), platform, ctx);
    if let Some((file, v)) = rc.lookup(tool, "cache_root", parse_yarnrc_yml)? {
        let source = CacheSource::Config {
            file,
            key: "cache_root".into(),
        };
        let (root, source) = configured(env, tool, &v, source)?;
        return Ok(vec![Spot::new(tool, root, source)]);
    }
    Ok(match env.os {
        Os::Macos => vec![Spot::new(
            tool,
            env.home.join("Library").join("Caches").join("CocoaPods"),
            CacheSource::Default,
        )],
        Os::Linux | Os::Windows => Vec::new(),
    })
}

fn nix_spots(env: &CacheEnv) -> Vec<Spot> {
    let tool = CacheTool::Nix;
    if let Some((var, v)) = env.first_var(&["NIX_STORE_DIR"]) {
        return vec![Spot::new(tool, PathBuf::from(v), CacheSource::Env { var })];
    }
    let set_up = env.first_var(&["NIX_PROFILES", "NIX_PATH"]).is_some();
    match env.os {
        Os::Linux | Os::Macos if set_up => {
            vec![Spot::new(
                tool,
                PathBuf::from("/nix/store"),
                CacheSource::Default,
            )]
        }
        _ => Vec::new(),
    }
}

/// Entries that identify the tool's layout, or `None` if `tool` isn't a long-tail tool.
pub(crate) fn markers(tool: CacheTool, inside: &Listing) -> Option<Vec<&str>> {
    let dirs = || {
        inside
            .iter()
            .filter(|(_, k)| **k == EntryKind::Dir)
            .map(|(n, _)| n.as_str())
    };
    let files = || {
        inside
            .iter()
            .filter(|(_, k)| **k == EntryKind::File)
            .map(|(n, _)| n.as_str())
    };
    let named = |names: &'static [&'static str]| -> Vec<&str> {
        dirs().filter(|n| names.contains(n)).collect()
    };
    Some(match tool {
        CacheTool::Ccache => dirs()
            .filter(|n| n.len() == 1 && n.bytes().all(|b| b.is_ascii_hexdigit()))
            .take(1)
            .collect(),
        CacheTool::Conan => files().filter(|n| *n == "cache.sqlite3").collect(),
        CacheTool::Vcpkg => dirs()
            .filter(|n| n.len() == 2 && n.bytes().all(|b| b.is_ascii_hexdigit()))
            .take(1)
            .collect(),
        CacheTool::PlatformIo => dirs()
            .filter(|n| matches!(*n, "downloads" | "http"))
            .chain(files().filter(|n| *n == "db.data"))
            .collect(),
        CacheTool::Arduino => named(&["packages", "libraries"]),
        CacheTool::EspIdf => files()
            .filter(|n| {
                [".tar.gz", ".tar.xz", ".tar.bz2", ".zip"]
                    .iter()
                    .any(|e| n.ends_with(e))
            })
            .take(1)
            .collect(),
        CacheTool::SwiftPm => named(&["repositories", "artifacts", "manifests", "registry"]),
        CacheTool::CocoaPods => named(&["Pods"]),
        CacheTool::Coursier => named(&["https", "http"]),
        CacheTool::Deno => named(&["deps", "gen", "npm", "remote", "registries"]),
        CacheTool::Zig => named(&["o", "h", "z", "p"]),
        CacheTool::Playwright => dirs()
            .filter(|n| {
                ["chromium", "firefox", "webkit", "ffmpeg"]
                    .iter()
                    .any(|b| n.starts_with(b))
                    && n.rsplit_once('-').is_some_and(|(_, v)| {
                        !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit())
                    })
            })
            .collect(),
        CacheTool::Cypress => dirs()
            .filter(|n| n.contains('.') && n.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
            .collect(),
        CacheTool::Puppeteer => {
            named(&["chrome", "chrome-headless-shell", "chromedriver", "firefox"])
        }
        CacheTool::Electron => inside
            .keys()
            .map(String::as_str)
            .filter(|n| {
                n.contains("electron-v")
                    || (n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit()))
            })
            .take(1)
            .collect(),
        CacheTool::Homebrew => named(&["downloads", "Cask", "api"]),
        CacheTool::Nix => inside
            .keys()
            .map(String::as_str)
            .filter(|n| *n == ".links" || is_nix_store_name(n))
            .take(2)
            .collect(),
        _ => return None,
    })
}

/// `<32 characters of Nix's base-32 alphabet>-<name>`.
fn is_nix_store_name(n: &str) -> bool {
    const ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    n.len() > 33
        && n.as_bytes()[32] == b'-'
        && n.as_bytes()[..32].iter().all(|b| ALPHABET.contains(b))
}

/// Reasons a long-tail cache folder may also hold data that isn't a cache (each raises the
/// candidate to S2, where no tool command is offered).
pub(crate) fn non_cache_data(tool: CacheTool, inside: &Listing) -> Vec<String> {
    match tool {
        CacheTool::Deno if inside.get("location_data") == Some(&EntryKind::Dir) => vec![
            "holds `location_data/`: the localStorage of Deno programs, which `deno clean` \
             also removes"
                .into(),
        ],
        _ => Vec::new(),
    }
}

/// A tool-specific fact for the evidence (cleanup scope, what stays).
pub(crate) fn note(tool: CacheTool) -> Option<&'static str> {
    Some(match tool {
        CacheTool::Ccache => {
            "`ccache --clear` empties the cache and keeps its configuration (ccache.conf); \
             compilations are slower until it refills"
        }
        CacheTool::Conan => {
            "`conan cache clean` removes only the temporary source, build and download folders \
             of cached packages; the packages stay (some may have been built locally and are \
             slow to rebuild)"
        }
        CacheTool::Arduino => {
            "`arduino-cli cache clean` empties the download cache (staging/); installed cores \
             and libraries stay"
        }
        CacheTool::CocoaPods => {
            "`pod cache clean --all` empties the download cache; projects' Pods/ folders stay"
        }
        CacheTool::Deno => "`deno clean` removes the whole Deno cache folder (DENO_DIR)",
        CacheTool::Playwright => {
            "`playwright uninstall --all` removes the browsers of every Playwright installation \
             on this machine; `npx playwright install` downloads them again"
        }
        CacheTool::Cypress => {
            "`cypress cache prune` keeps only the binary of the installed Cypress version; \
             `cypress cache clear` removes every version"
        }
        CacheTool::Puppeteer => {
            "`puppeteer browsers clear` removes every browser in this folder; \
             `npx puppeteer browsers install` downloads them again"
        }
        CacheTool::Homebrew => {
            "`brew cleanup` removes outdated downloads from this cache, and also uninstalls old \
             versions of installed formulae (pinned formulae are kept)"
        }
        _ => return None,
    })
}

/// Why a long-tail cache with no offered command is inspect-only (`None`: the generic "the tool
/// has no command" wording applies).
pub(crate) fn inspect_only_reason(tool: CacheTool) -> Option<&'static str> {
    Some(match tool {
        CacheTool::PlatformIo => {
            "PlatformIO's `pio system prune` also removes unused platforms and tools, so scull \
             doesn't offer it and only shows this cache"
        }
        CacheTool::EspIdf => {
            "these are the archives ESP-IDF's installer downloaded (installed tools stay in \
             tools/); ESP-IDF's `idf_tools.py uninstall --remove-archives` removes them from an \
             ESP-IDF checkout, so scull only shows them"
        }
        CacheTool::SwiftPm => {
            "SwiftPM's `swift package purge-cache` runs inside a package, so scull doesn't \
             offer it and only shows this cache"
        }
        CacheTool::Nix => {
            "the Nix store is managed by Nix: run `nix-collect-garbage` yourself to remove \
             paths nothing refers to. scull never touches the store"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ccache_conf() {
        let m = parse_ccache_conf("# comment\nmax_size = 5G\ncache_dir = /data/ccache\nbad\n");
        assert_eq!(m.get("cache_dir").map(String::as_str), Some("/data/ccache"));
        assert_eq!(m.get("max_size").map(String::as_str), Some("5G"));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn nix_store_names() {
        assert!(is_nix_store_name(
            "0c5wdpx2rjjv3mbvsqgbdbm1w2rs1a3l-glibc-2.39"
        ));
        assert!(!is_nix_store_name("short-name"));
        // `e`, `o`, `t`, `u` are not in Nix's alphabet.
        assert!(!is_nix_store_name(
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-glibc-2.39"
        ));
    }
}
