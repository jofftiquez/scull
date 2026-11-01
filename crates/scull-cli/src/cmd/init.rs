//! `scull init` (P1-CLI-03; ONB-01…04, ONB-07, CFG-01, CFG-04): first-run onboarding.
//!
//! 1. **Detect first** (ONB-01), metadata only: toolchain binaries on `PATH` (absolute entries
//!    only), cache folders (every tool of the cache locator, [`CacheTool::ALL`], at its
//!    environment-aware location, plus a few well-known folders it doesn't cover), likely
//!    project folders under the home directory, and AI-client config folders. Nothing is listed
//!    recursively, sized or scanned, and no configuration file is read.
//! 2. **Ask at most five questions** (ONB-02), pre-filled from what was found: what you build,
//!    scan roots, AI-agent use, what matters most, caution. On a terminal they are prompts on
//!    stderr (Enter accepts the default); with `--yes` (or `--json`) the flags and detected
//!    defaults answer them, and without a terminal `--yes` is required.
//! 3. **Write the profile** (ONB-03): scan roots go to the user config
//!    (`<config dir>/config.toml`, `[scan] roots`, other settings in an existing file are kept
//!    and the previous file is saved as `config.toml.bak`), the answers to
//!    `<config dir>/profile.toml`. Both are created owner-only and replaced atomically.
//!
//! Roots are checked with [`validate_roots`]: a filesystem root, the home directory or one of
//! its parents is never added without explicit confirmation (typing `yes` at the prompt, or
//! `--allow-broad-roots`). Caution and the other answers never change tiers or safety checks
//! (ONB-07). Rerunning needs `--force` once a profile exists (ONB-04). Disk size and free space
//! are not shown yet (no volumes API).

use super::{emit_machine, emit_text};
use crate::cli::InitArgs;
use crate::output::Ui;
use crate::output::json::SchemaId;
use crate::output::style::Style;
use crate::output::table::Cell;
use schemars::JsonSchema;
use scull_core::config::{ConfigLayer, expand_home, validate_roots};
use scull_core::{ErrorKind, FsPath, ScullError};
use scull_detectors::caches::CacheTool;
use scull_detectors::rules::Platform as RulesOs;
use scull_detectors::{CacheEnv, DetectContext, locate_caches};
use serde::Serialize;
use std::ffi::OsStr;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};

/// Schema of `scull init --json`.
pub const INIT_SCHEMA: SchemaId = SchemaId::new("init", 1);

/// File name of the onboarding profile inside the config directory.
pub const PROFILE_FILE_NAME: &str = "profile.toml";

/// Answers to "What do you build?".
pub const BUILDS: [&str; 9] = [
    "web", "rust", "python", "ml", "mobile", "game", "native", "infra", "other",
];
/// Answers to "Do you use AI coding agents?".
pub const AGENTS: [&str; 3] = ["none", "sometimes", "daily"];
/// Answers to "What matters most right now?".
pub const PRIORITIES: [&str; 3] = ["free-space", "tidy", "understand"];
/// Answers to "How cautious should scull be?".
pub const CAUTION: [&str; 3] = ["cautious", "balanced", "expert"];

/// Folders under the home directory that often hold projects (ONB-01).
const PROJECT_FOLDERS: &[&str] = &[
    "code",
    "Code",
    "Projects",
    "projects",
    "dev",
    "Developer",
    "src",
    "work",
    "repos",
    "git",
    "workspace",
];

/// AI clients and the config folder that reveals each (ONB-01).
const AI_CLIENTS: &[(&str, &str)] = &[
    ("claude-code", ".claude"),
    ("codex", ".codex"),
    ("cursor", ".cursor"),
];

/// A toolchain: id, label, build kind it suggests, binaries, and well-known folders under home
/// that the cache locator doesn't cover (its caches are added by [`toolchain_for`]).
struct ToolchainSpec {
    id: &'static str,
    label: &'static str,
    build: &'static str,
    binaries: &'static [&'static str],
    caches: &'static [&'static str],
}

const TOOLCHAINS: &[ToolchainSpec] = &[
    ToolchainSpec {
        id: "rust",
        label: "Rust",
        build: "rust",
        binaries: &["cargo", "rustc", "rustup"],
        caches: &[".cargo/registry", ".cargo/git", ".rustup"],
    },
    ToolchainSpec {
        id: "node",
        label: "Node.js",
        build: "web",
        binaries: &["node", "npm", "pnpm", "yarn", "bun", "deno"],
        caches: &[
            ".npm",
            ".cache/pnpm",
            "Library/pnpm",
            "AppData/Local/pnpm",
            ".bun/install/cache",
            ".cache/yarn",
            "Library/Caches/Yarn",
            "AppData/Local/Yarn",
        ],
    },
    ToolchainSpec {
        id: "python",
        label: "Python",
        build: "python",
        binaries: &["python3", "python", "uv", "conda", "pip", "pip3"],
        caches: &[
            ".cache/pip",
            ".cache/uv",
            "Library/Caches/pip",
            "AppData/Local/pip",
            ".conda",
        ],
    },
    ToolchainSpec {
        id: "ml",
        label: "ML/AI",
        build: "ml",
        binaries: &["ollama"],
        caches: &[".ollama/models", ".cache/huggingface", ".cache/torch"],
    },
    ToolchainSpec {
        id: "containers",
        label: "Containers",
        build: "infra",
        binaries: &["docker", "podman"],
        caches: &[".docker"],
    },
    ToolchainSpec {
        id: "go",
        label: "Go",
        build: "other",
        binaries: &["go"],
        caches: &["go/pkg/mod", ".cache/go-build", "Library/Caches/go-build"],
    },
    ToolchainSpec {
        id: "jvm",
        label: "Java/JVM",
        build: "other",
        binaries: &["java", "gradle", "mvn"],
        caches: &[".gradle/caches", ".m2/repository"],
    },
    ToolchainSpec {
        id: "dotnet",
        label: ".NET",
        build: "other",
        binaries: &["dotnet"],
        caches: &[".nuget/packages"],
    },
    ToolchainSpec {
        id: "mobile",
        label: "Mobile (Xcode, Android, Flutter)",
        build: "mobile",
        binaries: &["xcodebuild", "adb", "sdkmanager", "flutter", "pod", "swift"],
        caches: &[
            "Library/Developer/Xcode/DerivedData",
            "Library/Android/sdk",
            "Android/Sdk",
            "AppData/Local/Android/Sdk",
            ".gradle/caches",
        ],
    },
    ToolchainSpec {
        id: "native",
        label: "C/C++",
        build: "native",
        binaries: &["cmake", "clang", "gcc", "cl", "zig", "conan", "vcpkg"],
        caches: &[".ccache", ".cache/ccache"],
    },
    ToolchainSpec {
        id: "embedded",
        label: "Embedded (PlatformIO, Arduino, ESP-IDF)",
        build: "native",
        binaries: &["pio", "platformio", "arduino-cli", "idf.py"],
        caches: &[],
    },
    ToolchainSpec {
        id: "system",
        label: "System package managers (Homebrew, Nix)",
        build: "other",
        binaries: &["brew", "nix"],
        caches: &[],
    },
];

/// The onboarding toolchain a located cache belongs to. Exhaustive on purpose: a new
/// [`CacheTool`] doesn't compile until it is given one.
pub fn toolchain_for(tool: CacheTool) -> &'static str {
    match tool {
        CacheTool::Npm
        | CacheTool::Pnpm
        | CacheTool::Bun
        | CacheTool::YarnClassic
        | CacheTool::YarnBerry
        | CacheTool::Deno
        | CacheTool::Playwright
        | CacheTool::Cypress
        | CacheTool::Puppeteer
        | CacheTool::Electron => "node",
        CacheTool::Pip | CacheTool::Uv | CacheTool::Conda => "python",
        CacheTool::GoBuild | CacheTool::GoMod => "go",
        CacheTool::Gradle | CacheTool::Maven | CacheTool::Coursier => "jvm",
        CacheTool::NuGet => "dotnet",
        CacheTool::SwiftPm | CacheTool::CocoaPods => "mobile",
        CacheTool::Ccache | CacheTool::Conan | CacheTool::Vcpkg | CacheTool::Zig => "native",
        CacheTool::PlatformIo | CacheTool::Arduino | CacheTool::EspIdf => "embedded",
        CacheTool::Homebrew | CacheTool::Nix => "system",
    }
}

/// One detected toolchain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Toolchain {
    /// Stable identifier, e.g. `rust`.
    pub id: String,
    /// Display name.
    pub label: String,
    /// The "What do you build?" answer it suggests.
    pub build: String,
    /// Binaries found on `PATH` (first match of each).
    pub binaries: Vec<FsPath>,
    /// Known cache folders that exist (not sized).
    pub caches: Vec<FsPath>,
}

/// What the metadata-only probe found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Detection {
    /// The home directory probed.
    pub home: FsPath,
    /// Toolchains with at least one binary or cache folder present.
    pub toolchains: Vec<Toolchain>,
    /// Likely project folders that exist, as `~/...`.
    pub project_folders: Vec<String>,
    /// AI clients whose config folder exists.
    pub ai_clients: Vec<String>,
}

/// The onboarding answers, as saved in `profile.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Profile format version.
    pub version: u32,
    /// What you build.
    pub builds: Vec<String>,
    /// Scan roots as written to the config (`~/...` under the home directory).
    pub roots: Vec<String>,
    /// AI coding agents: none, sometimes, daily.
    pub agents: String,
    /// What matters most: free-space, tidy, understand.
    pub priority: String,
    /// Caution: cautious, balanced, expert. Never changes tiers or safety checks (ONB-07).
    pub caution: String,
    /// Toolchain IDs detected when the profile was made.
    pub detected_toolchains: Vec<String>,
    /// AI clients detected when the profile was made.
    pub detected_ai_clients: Vec<String>,
}

/// `scull init` output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitReport {
    /// What the probe found.
    pub detected: Detection,
    /// The saved answers.
    pub profile: Profile,
    /// The user config file written.
    pub config_path: FsPath,
    /// The profile file written.
    pub profile_path: FsPath,
    /// Backup of the previous config, if one was replaced.
    pub config_backup: Option<FsPath>,
    /// Broad roots kept because they were explicitly confirmed.
    pub confirmed_broad_roots: Vec<String>,
    /// Things worth knowing (missing roots, skipped broad roots, ...).
    pub notes: Vec<String>,
}

/// Asks the onboarding questions.
pub trait Prompter {
    /// Show `question` with `default` and return the answer (the default on an empty line).
    fn ask(&mut self, question: &str, default: &str) -> io::Result<String>;
    /// Show a line of information.
    fn say(&mut self, text: &str);
}

/// Prompts on stderr, answers from stdin.
struct Terminal;

impl Prompter for Terminal {
    fn ask(&mut self, question: &str, default: &str) -> io::Result<String> {
        let mut err = io::stderr().lock();
        write!(err, "{} [{default}]: ", Cell::text(question).as_str())?;
        err.flush()?;
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "input closed before onboarding finished",
            ));
        }
        let line = line.trim();
        Ok(if line.is_empty() { default } else { line }.to_owned())
    }

    fn say(&mut self, text: &str) {
        let _ = writeln!(io::stderr(), "{}", Cell::text(text).as_str());
    }
}

/// `scull init`.
pub fn run(ui: &Ui, args: &InitArgs) -> Result<(), ScullError> {
    let interactive = !args.yes && !ui.format.is_machine() && io::stdin().is_terminal();
    if !interactive && !args.yes {
        return Err(usage(
            "scull init asks questions on a terminal; pass --yes to accept the detected \
             defaults (and --roots/--builds/... to override them)",
        ));
    }
    let home = std::env::home_dir()
        .filter(|h| h.is_absolute())
        .ok_or_else(|| usage("cannot locate the home directory"))?;
    let dirs = scull_platform::app_dirs()
        .map_err(|e| ScullError::new(ErrorKind::Config, e.to_string()))?;
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    let cache_env =
        RulesOs::current().map(|os| CacheEnv::new(home.clone(), os, |k| std::env::var(k).ok()));
    let detected = detect(&home, &path_env, cache_env.as_ref());
    let mut terminal = Terminal;
    let prompter: Option<&mut dyn Prompter> = if interactive {
        Some(&mut terminal)
    } else {
        None
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.clone());
    let (profile, confirmed, mut notes) = answer(&detected, args, &home, &cwd, prompter)?;
    let report = write(
        &dirs.config,
        detected,
        profile,
        confirmed,
        &mut notes,
        args.force,
    )?;
    if ui.format.is_machine() {
        emit_machine(ui.format, INIT_SCHEMA, "init", &report)
    } else {
        emit_text(&render(&report, ui.color_out))
    }
}

fn usage(msg: &str) -> ScullError {
    ScullError::new(ErrorKind::Usage, msg)
}

/// The metadata-only probe (ONB-01). `path_env` is a `PATH` value; relative entries are
/// ignored so the current directory never counts. `cache_env` (home, environment, OS layout)
/// locates every [`CacheTool`] the way `scull caches` does, but without reading any
/// configuration file: a tool whose user config would decide the location is left out here.
pub fn detect(home: &Path, path_env: &OsStr, cache_env: Option<&CacheEnv>) -> Detection {
    let path_dirs: Vec<PathBuf> = std::env::split_paths(path_env)
        .filter(|d| d.is_absolute())
        .collect();
    let exists = |rel: &str| {
        let p = rel.split('/').fold(home.to_path_buf(), |p, c| p.join(c));
        std::fs::symlink_metadata(&p).is_ok().then_some(p)
    };
    let located: Vec<(CacheTool, PathBuf)> = cache_env
        .map(|env| {
            locate_caches(env, &scull_platform::Native::new(), &DetectContext::new())
                .found
                .into_iter()
                .map(|c| (c.tool, c.path))
                .collect()
        })
        .unwrap_or_default();
    let toolchains = TOOLCHAINS
        .iter()
        .filter_map(|t| {
            let binaries: Vec<FsPath> = t
                .binaries
                .iter()
                .filter_map(|b| find_binary(&path_dirs, b))
                .map(|p| FsPath::from_path(&p))
                .collect();
            let mut found: Vec<PathBuf> = t.caches.iter().filter_map(|c| exists(c)).collect();
            for (tool, path) in &located {
                // A located cache inside a folder already listed (`~/.npm/_cacache` under
                // `~/.npm`) adds nothing.
                if toolchain_for(*tool) == t.id && !found.iter().any(|f| path.starts_with(f)) {
                    found.push(path.clone());
                }
            }
            let caches: Vec<FsPath> = found.iter().map(|p| FsPath::from_path(p)).collect();
            (!binaries.is_empty() || !caches.is_empty()).then(|| Toolchain {
                id: t.id.to_owned(),
                label: t.label.to_owned(),
                build: t.build.to_owned(),
                binaries,
                caches,
            })
        })
        .collect();
    let mut project_folders: Vec<String> = Vec::new();
    for f in PROJECT_FOLDERS {
        let p = home.join(f);
        // Case-insensitive filesystems report `code` and `Code` as the same folder.
        if p.is_dir()
            && !project_folders
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&format!("~/{f}")))
        {
            project_folders.push(format!("~/{f}"));
        }
    }
    let ai_clients = AI_CLIENTS
        .iter()
        .filter(|(_, dir)| exists(dir).is_some())
        .map(|(id, _)| (*id).to_owned())
        .collect();
    Detection {
        home: FsPath::from_path(home),
        toolchains,
        project_folders,
        ai_clients,
    }
}

fn find_binary(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat", "com"]
            .iter()
            .map(|ext| format!("{name}.{ext}"))
            .collect()
    } else {
        vec![name.to_owned()]
    };
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

/// Answer the five questions (ONB-02) from flags, the prompter, or detected defaults. Returns
/// the profile, the broad roots that were explicitly confirmed, and notes.
pub fn answer(
    det: &Detection,
    args: &InitArgs,
    home: &Path,
    cwd: &Path,
    mut prompter: Option<&mut dyn Prompter>,
) -> Result<(Profile, Vec<String>, Vec<String>), ScullError> {
    let mut notes = Vec::new();
    if let Some(p) = prompter.as_deref_mut() {
        p.say(&detected_summary(det));
    }

    // 1. What do you build?
    let mut detected_builds: Vec<String> = Vec::new();
    for t in &det.toolchains {
        if !detected_builds.contains(&t.build) {
            detected_builds.push(t.build.clone());
        }
    }
    if detected_builds.is_empty() {
        detected_builds.push("other".to_owned());
    }
    let builds = if args.builds.is_empty() {
        let default = detected_builds.join(",");
        ask_valid(
            prompter.as_deref_mut(),
            &format!("What do you build? ({})", BUILDS.join(", ")),
            &default,
            |s| parse_list(s, &BUILDS, "build kind"),
        )?
    } else {
        parse_list(&args.builds.join(","), &BUILDS, "build kind")?
    };

    // 2. Where do your projects live?
    let roots_in: Vec<String> = if args.roots.is_empty() {
        let default = det.project_folders.join(",");
        ask_valid(
            prompter.as_deref_mut(),
            "Where do your projects live? (comma-separated folders, `-` for none)",
            if default.is_empty() { "-" } else { &default },
            |s| {
                Ok(s.split(',')
                    .map(str::trim)
                    .filter(|r| !r.is_empty() && *r != "-")
                    .map(str::to_owned)
                    .collect::<Vec<_>>())
            },
        )?
    } else {
        args.roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect()
    };
    let (roots, confirmed) = check_roots(
        &roots_in,
        home,
        cwd,
        args.allow_broad_roots,
        prompter.as_deref_mut(),
        &mut notes,
    )?;
    if roots.is_empty() {
        notes.push(
            "no scan roots set: pass folders to `scull scan`, or rerun `scull init --force`"
                .to_owned(),
        );
    }

    // 3–5.
    let agents_default = if det.ai_clients.is_empty() {
        "none"
    } else {
        "sometimes"
    };
    let mut one = |flag: &Option<String>, q: &str, options: &[&str], default: &str| match flag {
        Some(v) => parse_one(v, options, q),
        None => ask_valid(
            prompter.as_deref_mut(),
            &format!("{q} ({})", options.join(", ")),
            default,
            |s| parse_one(s, options, q),
        ),
    };
    let agents = one(
        &args.agents,
        "Do you use AI coding agents?",
        &AGENTS,
        agents_default,
    )?;
    let priority = one(
        &args.priority,
        "What matters most right now?",
        &PRIORITIES,
        "understand",
    )?;
    let caution = one(
        &args.caution,
        "How cautious should scull be? Never changes tiers or safety checks;",
        &CAUTION,
        "balanced",
    )?;

    Ok((
        Profile {
            version: 1,
            builds,
            roots,
            agents,
            priority,
            caution,
            detected_toolchains: det.toolchains.iter().map(|t| t.id.clone()).collect(),
            detected_ai_clients: det.ai_clients.clone(),
        },
        confirmed,
        notes,
    ))
}

/// Ask until the answer parses (three tries), or parse the default without a prompter.
fn ask_valid<'p, T>(
    prompter: Option<&mut (dyn Prompter + 'p)>,
    question: &str,
    default: &str,
    parse: impl Fn(&str) -> Result<T, ScullError>,
) -> Result<T, ScullError> {
    let Some(p) = prompter else {
        return parse(default);
    };
    let mut last = None;
    for _ in 0..3 {
        let answer = p.ask(question, default).map_err(|e| prompt_error(&e))?;
        match parse(&answer) {
            Ok(v) => return Ok(v),
            Err(e) => {
                p.say(&e.message);
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| usage("no valid answer")))
}

fn prompt_error(e: &io::Error) -> ScullError {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        ScullError::new(ErrorKind::Cancelled, e.to_string())
    } else {
        ScullError::from_io(e)
    }
}

fn parse_one(s: &str, options: &[&str], what: &str) -> Result<String, ScullError> {
    let v = s.trim().to_ascii_lowercase();
    if options.contains(&v.as_str()) {
        Ok(v)
    } else {
        Err(usage(&format!(
            "`{s}` is not a valid answer to \"{what}\"; expected one of: {}",
            options.join(", ")
        )))
    }
}

fn parse_list(s: &str, options: &[&str], what: &str) -> Result<Vec<String>, ScullError> {
    let mut out: Vec<String> = Vec::new();
    for item in s.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let v = parse_one(item, options, what)?;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    if out.is_empty() {
        return Err(usage(&format!(
            "pick at least one {what}: {}",
            options.join(", ")
        )));
    }
    Ok(out)
}

/// Expand, normalize and check roots (CFG-04). Broad roots need `allow_broad` or a typed
/// `yes`; without either they are refused (non-interactive) or skipped (interactive).
fn check_roots<'p>(
    given: &[String],
    home: &Path,
    cwd: &Path,
    allow_broad: bool,
    mut prompter: Option<&mut (dyn Prompter + 'p)>,
    notes: &mut Vec<String>,
) -> Result<(Vec<String>, Vec<String>), ScullError> {
    let mut roots: Vec<(PathBuf, String)> = Vec::new();
    for r in given {
        let expanded = expand_home(Path::new(r), home);
        let abs = normalize(&if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        });
        let shown = tilde(&abs, home);
        if !roots.iter().any(|(p, _)| *p == abs) {
            roots.push((abs, shown));
        }
    }
    if roots.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let paths: Vec<PathBuf> = roots.iter().map(|(p, _)| p.clone()).collect();
    let concerns = validate_roots(&paths, home, &[])?;
    let mut confirmed = Vec::new();
    let mut keep = Vec::new();
    for (path, shown) in roots {
        if let Some(c) = concerns.iter().find(|c| c.root == path) {
            let ok = if allow_broad {
                true
            } else if let Some(p) = prompter.as_deref_mut() {
                let a = p
                    .ask(&format!("{} Type `yes` to scan it anyway", c.message), "no")
                    .map_err(|e| prompt_error(&e))?;
                a.trim().eq_ignore_ascii_case("yes")
            } else {
                return Err(usage(&format!(
                    "{}; pass --allow-broad-roots to confirm it",
                    c.message
                )));
            };
            if !ok {
                notes.push(format!("skipped `{shown}`: not confirmed"));
                continue;
            }
            confirmed.push(shown.clone());
        }
        if !path.is_dir() {
            notes.push(format!("`{shown}` does not exist (yet)"));
        }
        keep.push(shown);
    }
    Ok((keep, confirmed))
}

/// Lexical normalization: drop `.`, resolve `..` against the preceding component.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `~/rest` (with `/`) for paths under `home`, otherwise the path as is.
fn tilde(p: &Path, home: &Path) -> String {
    match p.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => {
            let parts: Vec<String> = rest
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            format!("~/{}", parts.join("/"))
        }
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

fn detected_summary(det: &Detection) -> String {
    let names: Vec<&str> = det.toolchains.iter().map(|t| t.label.as_str()).collect();
    let list = |v: &[String]| {
        if v.is_empty() {
            "none".to_owned()
        } else {
            v.join(", ")
        }
    };
    format!(
        "Found toolchains: {}. Project folders: {}. AI clients: {}.",
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        },
        list(&det.project_folders),
        list(&det.ai_clients)
    )
}

/// Write `config.toml` (scan roots) and `profile.toml` owner-only into `config_dir`.
pub fn write(
    config_dir: &Path,
    detected: Detection,
    profile: Profile,
    confirmed: Vec<String>,
    notes: &mut Vec<String>,
    force: bool,
) -> Result<InitReport, ScullError> {
    let config_path = config_dir.join(super::doctor::CONFIG_FILE_NAME);
    let profile_path = config_dir.join(PROFILE_FILE_NAME);
    let fs_err = |path: &Path, e: &dyn std::fmt::Display| {
        ScullError::new(ErrorKind::Io, format!("{e}")).with_path(FsPath::from_path(path))
    };
    if !force && std::fs::symlink_metadata(&profile_path).is_ok() {
        return Err(usage(&format!(
            "scull is already set up (profile {}); rerun with --force to redo onboarding",
            profile_path.display()
        )));
    }
    let existing = match std::fs::read_to_string(&config_path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(fs_err(&config_path, &e)),
    };
    let mut layer = match &existing {
        Some(text) => ConfigLayer::from_toml(text).map_err(|e| {
            ScullError::new(
                ErrorKind::Config,
                format!("{}; fix or remove it first", super::one_line(&e.message)),
            )
            .with_path(FsPath::from_path(&config_path))
        })?,
        None => ConfigLayer::default(),
    };
    if !force && layer.scan.roots.is_some() {
        return Err(usage(&format!(
            "{} already sets scan roots; rerun with --force to replace them",
            config_path.display()
        )));
    }
    if !profile.roots.is_empty() {
        layer.scan.roots = Some(profile.roots.iter().map(PathBuf::from).collect());
    }

    scull_platform::create_dir_owner_only(config_dir).map_err(|e| fs_err(config_dir, &e))?;
    let config_text = format!(
        "# scull user config (CFG-01). Scan roots written by `scull init`; edit freely.\n{}",
        toml::to_string(&layer).map_err(|e| ScullError::new(ErrorKind::Internal, e.to_string()))?
    );
    ConfigLayer::from_toml(&config_text)
        .map_err(|e| ScullError::new(ErrorKind::Internal, e.message))?;
    let profile_text = format!(
        "# scull onboarding profile (ONB-03), written by `scull init`.\n# It personalizes \
         emphasis only: tiers and safety checks are the same for everyone (ONB-07).\n{}",
        toml::to_string(&profile)
            .map_err(|e| ScullError::new(ErrorKind::Internal, e.to_string()))?
    );

    let mut config_backup = None;
    if let Some(old) = &existing {
        let backup = config_dir.join("config.toml.bak");
        replace_owner_only(&backup, old).map_err(|e| fs_err(&backup, &e))?;
        config_backup = Some(FsPath::from_path(&backup));
        notes.push(format!("previous config saved as {}", backup.display()));
    }
    replace_owner_only(&config_path, &config_text).map_err(|e| fs_err(&config_path, &e))?;
    replace_owner_only(&profile_path, &profile_text).map_err(|e| fs_err(&profile_path, &e))?;

    Ok(InitReport {
        detected,
        profile,
        config_path: FsPath::from_path(&config_path),
        profile_path: FsPath::from_path(&profile_path),
        config_backup,
        confirmed_broad_roots: confirmed,
        notes: std::mem::take(notes),
    })
}

/// Write `text` to a new owner-only temp file next to `path`, then rename it over `path`.
pub(crate) fn replace_owner_only(path: &Path, text: &str) -> io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut f = scull_platform::owner_only::create_file(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The human summary ("Here's what scull will focus on", ONB-03).
pub fn render(r: &InitReport, color: bool) -> String {
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_owned()
        } else {
            v.join(", ")
        }
    };
    let mut out = format!("{}\n", Style::Bold.paint("Detected", color));
    let tools: Vec<String> = r
        .detected
        .toolchains
        .iter()
        .map(|t| {
            let bins: Vec<String> = t
                .binaries
                .iter()
                .filter_map(|b| {
                    Path::new(b.as_str())
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                })
                .collect();
            if bins.is_empty() {
                format!("{} (caches only)", t.label)
            } else {
                format!("{} ({})", t.label, bins.join(", "))
            }
        })
        .collect();
    let home = Path::new(r.detected.home.as_str());
    let caches: Vec<String> = r
        .detected
        .toolchains
        .iter()
        .flat_map(|t| t.caches.iter())
        .map(|c| tilde(Path::new(c.as_str()), home))
        .collect();
    for (k, v) in [
        ("toolchains", list(&tools)),
        ("caches", list(&caches)),
        ("project folders", list(&r.detected.project_folders)),
        ("AI clients", list(&r.detected.ai_clients)),
        (
            "disk",
            "size and free space not shown yet (no volumes API in this build)".to_owned(),
        ),
    ] {
        out.push_str(&format!("  {k:<16} {}\n", Cell::text(&v).as_str()));
    }
    out.push_str(&format!(
        "\n{}\n",
        Style::Bold.paint("Here's what scull will focus on", color)
    ));
    let p = &r.profile;
    for (k, v) in [
        ("builds", list(&p.builds)),
        ("scan roots", list(&p.roots)),
        ("AI agents", p.agents.clone()),
        ("priority", p.priority.clone()),
        (
            "caution",
            format!("{} (never changes tiers or safety checks)", p.caution),
        ),
    ] {
        out.push_str(&format!("  {k:<16} {}\n", Cell::text(&v).as_str()));
    }
    out.push('\n');
    out.push_str(&format!(
        "Wrote {} and {}\n",
        Cell::path(r.config_path.as_str()).as_str(),
        Cell::path(r.profile_path.as_str()).as_str()
    ));
    for n in &r.notes {
        out.push_str(&Style::Warn.paint(&format!("note: {}", Cell::text(n).as_str()), color));
        out.push('\n');
    }
    match p.roots.first() {
        Some(root) => out.push_str(&format!(
            "Next: scull scan {}   (or `scull doctor` to check this machine)\n",
            Cell::text(root).as_str()
        )),
        None => out.push_str("Next: scull scan <folder>   (or `scull doctor`)\n"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers questions from a script and records what was asked.
    struct Script {
        answers: Vec<&'static str>,
        asked: Vec<String>,
    }

    impl Prompter for Script {
        fn ask(&mut self, question: &str, default: &str) -> io::Result<String> {
            self.asked.push(format!("{question} [{default}]"));
            if self.answers.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
            }
            let a = self.answers.remove(0);
            Ok(if a.is_empty() { default } else { a }.to_owned())
        }
        fn say(&mut self, _text: &str) {}
    }

    fn args() -> InitArgs {
        InitArgs {
            yes: false,
            roots: Vec::new(),
            allow_broad_roots: false,
            builds: Vec::new(),
            agents: None,
            priority: None,
            caution: None,
            force: false,
        }
    }

    fn fake_home() -> (tempfile::TempDir, Detection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let exe = if cfg!(windows) { ".exe" } else { "" };
        for b in ["cargo", "node"] {
            std::fs::write(bin.join(format!("{b}{exe}")), "").expect("bin file");
        }
        for d in ["Projects", "code", ".claude", ".npm"] {
            std::fs::create_dir_all(home.join(d)).expect("dir");
        }
        let path = std::env::join_paths([bin, PathBuf::from("relative/bin")]).expect("path");
        let det = detect(home, &path, None);
        (dir, det)
    }

    /// #38: every tool of the cache locator belongs to an onboarding toolchain, and every
    /// toolchain suggests a valid "What do you build?" answer.
    #[test]
    fn every_cache_tool_maps_to_a_toolchain() {
        for t in CacheTool::ALL {
            let id = toolchain_for(t);
            assert!(
                TOOLCHAINS.iter().any(|spec| spec.id == id),
                "{t:?} maps to unknown toolchain {id}"
            );
        }
        for spec in TOOLCHAINS {
            assert!(BUILDS.contains(&spec.build), "{}: {}", spec.id, spec.build);
        }
    }

    /// #38: caches the locator finds (defaults and environment overrides, long-tail tools
    /// included) show up under their toolchain; a located cache inside a folder already listed
    /// is not repeated; nothing outside the temp home is consulted.
    #[test]
    fn located_caches_join_their_toolchain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path();
        for d in [
            ".npm/_cacache/index-v5",
            "custom-deno/deps",
            ".cache/deno/deps",
            ".conan2/p",
            ".platformio/.cache/downloads",
            ".cache/ms-playwright/chromium-1140",
            ".cache/Homebrew/downloads",
            ".cache/org.swift.swiftpm/repositories",
        ] {
            std::fs::create_dir_all(home.join(d)).expect("dir");
        }
        let custom_deno = home.join("custom-deno");
        let env = CacheEnv::with_vars(
            home,
            RulesOs::Linux,
            [("DENO_DIR", custom_deno.to_string_lossy().into_owned())],
        );
        let det = detect(home, OsStr::new(""), Some(&env));
        let caches = |id: &str| -> Vec<String> {
            det.toolchains
                .iter()
                .find(|t| t.id == id)
                .unwrap_or_else(|| panic!("{id} detected: {:?}", det.toolchains))
                .caches
                .iter()
                .map(|p| {
                    p.to_path_buf()
                        .strip_prefix(home)
                        .expect("under home")
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .collect()
        };
        // `~/.npm` (hint) already covers `~/.npm/_cacache` (located); DENO_DIR wins over the
        // default `~/.cache/deno`.
        assert_eq!(
            caches("node"),
            [".npm", "custom-deno", ".cache/ms-playwright"]
        );
        assert_eq!(caches("native"), [".conan2/p"]);
        assert_eq!(caches("embedded"), [".platformio/.cache"]);
        assert_eq!(caches("system"), [".cache/Homebrew"]);
        assert_eq!(caches("mobile"), [".cache/org.swift.swiftpm"]);
        let ids: Vec<&str> = det.toolchains.iter().map(|t| t.id.as_str()).collect();
        assert!(!ids.contains(&"python"), "{ids:?}");
    }

    #[test]
    fn detection_is_prefilled_into_defaults() {
        let (dir, det) = fake_home();
        let ids: Vec<&str> = det.toolchains.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["rust", "node"]);
        assert!(det.project_folders.contains(&"~/Projects".to_owned()));
        assert!(det.project_folders.contains(&"~/code".to_owned()));
        assert_eq!(det.ai_clients, ["claude-code"]);
        let node = &det.toolchains[1];
        assert_eq!(node.caches.len(), 1, "~/.npm");

        // Enter on every question accepts what was detected.
        let mut s = Script {
            answers: vec!["", "", "", "", ""],
            asked: Vec::new(),
        };
        let (p, confirmed, _) =
            answer(&det, &args(), dir.path(), dir.path(), Some(&mut s)).expect("answers");
        assert_eq!(
            s.asked.len(),
            5,
            "five questions max (ONB-02): {:?}",
            s.asked
        );
        assert_eq!(p.builds, ["rust", "web"]);
        assert_eq!(p.roots.len(), 2);
        assert_eq!(p.agents, "sometimes", "AI client detected");
        assert_eq!(p.priority, "understand");
        assert_eq!(p.caution, "balanced");
        assert!(confirmed.is_empty());
    }

    #[test]
    fn interactive_answers_and_retries() {
        let (dir, det) = fake_home();
        let mut s = Script {
            answers: vec![
                "rust, ml",
                "~/Projects",
                "sometimes",
                "nope",
                "tidy",
                "cautious",
            ],
            asked: Vec::new(),
        };
        let (p, _, _) =
            answer(&det, &args(), dir.path(), dir.path(), Some(&mut s)).expect("answers");
        assert_eq!(p.builds, ["rust", "ml"]);
        assert_eq!(p.roots, ["~/Projects"]);
        assert_eq!(p.priority, "tidy", "invalid answer re-asked");
        assert_eq!(p.caution, "cautious");
    }

    #[test]
    fn broad_roots_need_explicit_confirmation() {
        let (dir, det) = fake_home();
        let home = dir.path();
        // Interactive: anything but `yes` skips the home root.
        let mut a = args();
        a.roots = vec![PathBuf::from("~"), PathBuf::from("~/Projects")];
        let mut s = Script {
            answers: vec!["", "no", "", "", ""],
            asked: Vec::new(),
        };
        let (p, confirmed, notes) = answer(&det, &a, home, home, Some(&mut s)).expect("ok");
        assert_eq!(p.roots, ["~/Projects"]);
        assert!(confirmed.is_empty());
        assert!(notes.iter().any(|n| n.contains("skipped `~`")), "{notes:?}");
        // Typing yes keeps it.
        let mut s = Script {
            answers: vec!["", "yes", "", "", ""],
            asked: Vec::new(),
        };
        let (p, confirmed, _) = answer(&det, &a, home, home, Some(&mut s)).expect("ok");
        assert_eq!(p.roots, ["~", "~/Projects"]);
        assert_eq!(confirmed, ["~"]);
        // Non-interactive: refused without the flag, kept with it.
        let err = answer(&det, &a, home, home, None).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Usage);
        assert!(err.message.contains("--allow-broad-roots"));
        a.allow_broad_roots = true;
        let (p, confirmed, _) = answer(&det, &a, home, home, None).expect("ok");
        assert_eq!(p.roots, ["~", "~/Projects"]);
        assert_eq!(confirmed, ["~"]);
        // A filesystem root too.
        a.allow_broad_roots = false;
        let root = if cfg!(windows) { "C:\\" } else { "/" };
        a.roots = vec![PathBuf::from(root)];
        assert!(answer(&det, &a, home, home, None).is_err());
    }

    #[test]
    fn eof_cancels_and_relative_roots_resolve() {
        let (dir, det) = fake_home();
        let mut s = Script {
            answers: vec![],
            asked: Vec::new(),
        };
        let err = answer(&det, &args(), dir.path(), dir.path(), Some(&mut s)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);

        let mut a = args();
        a.roots = vec![PathBuf::from("./code/../Projects")];
        let (p, _, _) = answer(&det, &a, dir.path(), dir.path(), None).expect("ok");
        assert_eq!(p.roots, ["~/Projects"]);
        assert!(parse_list("rust,bogus", &BUILDS, "build kind").is_err());
    }
}
