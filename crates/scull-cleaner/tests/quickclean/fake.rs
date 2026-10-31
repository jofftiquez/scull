//! Fake package managers.
//!
//! The test executable itself plays `npm`, `pnpm`, `bun` and `yarn`: [`install`] hard-links (or
//! copies) it into a temp folder under the tool's name, and [`dispatch`] (called first thing in
//! `main`) notices the name it was started under. A fake never touches anything but:
//!
//! * `<its folder>/<tool>.calls.jsonl`: one JSON record per run (argv, cwd, full environment),
//!   which doubles as the "it ran" sentinel;
//! * `<its folder>/<tool>.finished`: written after a configured sleep (the timeout test checks
//!   that a killed fake never gets there);
//! * the cache folder it is pointed at through the tool's own location option or variable, and
//!   only if that folder holds a `FAKE_CACHE` guard file (which it never removes). A prune
//!   removes files whose name starts with `unused`; a wipe removes everything else.
//!
//! Behavior is configured by `<its folder>/<tool>.behavior` (`key=value` lines): `version`,
//! `sleep_ms`, `exit`, `evil` (print control/bidi characters), `spam` (bytes of output).
//!
//! **Windows `.cmd` shims (P2-QC-02).** Started as `node`, the fake takes its first argument
//! as the script a shim named (written by [`install_shim`]), plays the tool that script belongs
//! to (`npm-cli.js` → npm, `pnpm.cjs` → pnpm, `yarn.js` → yarn) with the remaining arguments,
//! and records the script in the call. Nothing here runs a shim or starts a shell.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// Tool names the fake answers to.
pub const TOOLS: [&str; 4] = ["npm", "pnpm", "bun", "yarn"];

/// The guard file a fake requires in a cache before touching it.
pub const GUARD: &str = "FAKE_CACHE";

static MASTER: OnceLock<PathBuf> = OnceLock::new();

/// Copy this executable once into `dir` (a temp dir owned by `main`); [`install`] links to it.
pub fn prepare_master(dir: &Path) {
    let exe = std::env::current_exe().expect("current exe");
    let master = dir.join(format!("fake{}", std::env::consts::EXE_SUFFIX));
    fs::copy(&exe, &master).expect("copy test executable");
    MASTER.set(master).expect("master set once");
}

/// Install a fake `tool` into `dir` (created if needed); returns its path.
pub fn install(dir: &Path, tool: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("mkdir tools");
    let master = MASTER.get().expect("master prepared");
    let p = dir.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
    if fs::hard_link(master, &p).is_err() {
        fs::copy(master, &p).expect("copy fake");
    }
    p
}

/// The shim formats [`install_shim`] writes (real-world layouts, see `quickclean/shim.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimStyle {
    /// cmd-shim 4+ (`npm i -g`): Node next to the shim, else from the search path.
    CmdShim,
    /// Node's bundled `npm.cmd` (`NODE_EXE` / `NPM_CLI_JS`).
    NodeNpm,
    /// Yarn's installer: `node "%~dp0\yarn.js" %*` (Node from the search path only).
    YarnMsi,
}

/// The script a shim for `tool` runs, relative to its folder (`/`-separated).
pub fn shim_script(tool: &str, style: ShimStyle) -> String {
    match (style, tool) {
        (ShimStyle::YarnMsi, _) => format!("{tool}.js"),
        (_, "npm") => "node_modules/npm/bin/npm-cli.js".to_owned(),
        (_, "pnpm") => "node_modules/pnpm/bin/pnpm.cjs".to_owned(),
        (_, t) => format!("node_modules/{t}/bin/{t}.js"),
    }
}

/// Install a `<tool>.cmd` shim of `style` into `dir` with its script (a placeholder: the fake
/// `node` only reads its name). Returns the shim's path. Node is installed separately
/// (`install(dir, "node")`), or not at all.
pub fn install_shim(dir: &Path, tool: &str, style: ShimStyle) -> PathBuf {
    let rel = shim_script(tool, style);
    let script = rel.split('/').fold(dir.to_path_buf(), |p, c| p.join(c));
    fs::create_dir_all(script.parent().expect("parent")).expect("mkdir script");
    fs::write(&script, "// fake entry point\n").expect("write script");
    let win = rel.replace('/', r"\");
    let lines: Vec<String> = match style {
        ShimStyle::CmdShim => vec![
            "@ECHO off".into(),
            "GOTO start".into(),
            ":find_dp0".into(),
            "SET dp0=%~dp0".into(),
            "EXIT /b".into(),
            ":start".into(),
            "SETLOCAL".into(),
            "CALL :find_dp0".into(),
            String::new(),
            r#"IF EXIST "%dp0%\node.exe" ("#.into(),
            r#"  SET "_prog=%dp0%\node.exe""#.into(),
            ") ELSE (".into(),
            r#"  SET "_prog=node""#.into(),
            "  SET PATHEXT=%PATHEXT:;.JS;=;%".into(),
            ")".into(),
            String::new(),
            format!(
                r#"endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\{win}" %*"#
            ),
        ],
        ShimStyle::NodeNpm => vec![
            ":: Created by npm, please don't edit manually.".into(),
            "@ECHO OFF".into(),
            "SETLOCAL".into(),
            r#"SET "NODE_EXE=%~dp0\node.exe""#.into(),
            r#"IF NOT EXIST "%NODE_EXE%" ("#.into(),
            r#"  SET "NODE_EXE=node""#.into(),
            ")".into(),
            format!(r#"SET "NPM_CLI_JS=%~dp0\{win}""#),
            r#""%NODE_EXE%" "%NPM_CLI_JS%" %*"#.into(),
        ],
        ShimStyle::YarnMsi => vec!["@echo off".into(), format!(r#"node "%~dp0\{win}" %*"#)],
    };
    let shim = dir.join(format!("{tool}.cmd"));
    fs::write(&shim, lines.join("\r\n") + "\r\n").expect("write shim");
    shim
}

/// Write `<dir>/<tool>.behavior`.
pub fn behave(dir: &Path, tool: &str, lines: &[(&str, &str)]) {
    let text: String = lines.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    fs::write(dir.join(format!("{tool}.behavior")), text).expect("write behavior");
}

/// One recorded run of a fake.
#[derive(Debug, Clone)]
pub struct Call {
    pub argv: Vec<String>,
    /// The script, when the fake was started as `node <script>` (a resolved shim).
    pub script: Option<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// Every recorded run of the fake `tool` installed in `dir`.
pub fn calls(dir: &Path, tool: &str) -> Vec<Call> {
    let Ok(text) = fs::read_to_string(dir.join(format!("{tool}.calls.jsonl"))) else {
        return Vec::new();
    };
    text.lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("record");
            Call {
                argv: v["argv"]
                    .as_array()
                    .expect("argv")
                    .iter()
                    .map(|a| a.as_str().expect("str").to_owned())
                    .collect(),
                script: v["script"].as_str().map(str::to_owned),
                cwd: PathBuf::from(v["cwd"].as_str().expect("cwd")),
                env: v["env"]
                    .as_object()
                    .expect("env")
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().expect("str").to_owned()))
                    .collect(),
            }
        })
        .collect()
}

/// Whether any fake left a trace (calls or finished marker) in `dir`.
pub fn ran_in(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.ends_with(".calls.jsonl") || n.ends_with(".finished")
        })
    })
}

/// If this process was started as a fake tool, act as it and return the exit code.
pub fn dispatch() -> Option<i32> {
    let argv0 = std::env::args_os().next()?;
    let me = PathBuf::from(&argv0);
    let stem = me.file_stem()?.to_str()?.to_owned();
    if !TOOLS.contains(&stem.as_str()) && stem != "node" {
        return None;
    }
    let me = if me.is_absolute() {
        me
    } else {
        std::env::current_exe().ok()?
    };
    let dir = me.parent()?.to_path_buf();
    let mut args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a: OsString| a.to_string_lossy().into_owned())
        .collect();
    if stem != "node" {
        return Some(fake_main(&dir, &stem, args, None));
    }
    // `node <script> <args>`: play the tool the script belongs to.
    if args.is_empty() {
        eprintln!("fake node: no script");
        return Some(66);
    }
    let script = args.remove(0);
    let name = Path::new(&script)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_owned();
    let tool = match name.as_str() {
        "npm-cli.js" => "npm",
        "pnpm.cjs" => "pnpm",
        "yarn.js" => "yarn",
        _ => {
            eprintln!("fake node: unknown script {script}");
            return Some(67);
        }
    };
    Some(fake_main(&dir, tool, args, Some(&script)))
}

fn fake_main(dir: &Path, tool: &str, args: Vec<String>, script: Option<&str>) -> i32 {
    record(dir, tool, &args, script);
    let behavior = read_behavior(dir, tool);
    let get = |k: &str| behavior.get(k).map(String::as_str);

    if args == ["--version"] {
        let default = if tool == "yarn" { "1.22.22" } else { "10.0.0" };
        println!("{}", get("version").unwrap_or(default));
        return 0;
    }
    if let Some(ms) = get("sleep_ms").and_then(|v| v.parse::<u64>().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
        let _ = fs::write(dir.join(format!("{tool}.finished")), b"");
    }
    if get("evil") == Some("1") {
        println!("ok\x1b[2Jcleared\u{202e}txt.exe");
        eprintln!("warn\x07\r\nnext");
    }
    if let Some(n) = get("spam").and_then(|v| v.parse::<usize>().ok()) {
        let mut out = std::io::stdout().lock();
        let line = [b'x'; 99];
        for _ in 0..n / 100 {
            let _ = out.write_all(&line);
            let _ = out.write_all(b"\n");
        }
    }

    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
    };
    let words: Vec<&str> = args
        .iter()
        .take_while(|a| !a.starts_with("--"))
        .map(String::as_str)
        .collect();
    let job = match (tool, words.as_slice()) {
        ("npm", ["cache", "verify"]) => flag("--cache").map(|c| (c.join("_cacache"), false)),
        ("npm", ["cache", "clean"]) if args.iter().any(|a| a == "--force") => {
            flag("--cache").map(|c| (c.join("_cacache"), true))
        }
        ("pnpm", ["store", "prune"]) => flag("--store-dir").map(|c| (c, false)),
        ("bun", ["pm", "cache", "rm"]) => env("BUN_INSTALL_CACHE_DIR").map(|c| (c, true)),
        ("yarn", ["cache", "clean"]) if args.iter().any(|a| a == "--mirror") => {
            env("YARN_GLOBAL_FOLDER").map(|g| (g.join("cache"), true))
        }
        ("yarn", ["cache", "clean"]) => env("YARN_CACHE_FOLDER").map(|c| (c, true)),
        _ => None,
    };
    let Some((target, wipe)) = job else {
        eprintln!("fake {tool}: unsupported invocation {args:?}");
        return 64;
    };
    if !target.join(GUARD).is_file() {
        eprintln!("fake {tool}: {} is not a fake cache", target.display());
        return 65;
    }
    clean(&target, wipe);
    println!("fake {tool}: cleaned {}", target.display());
    get("exit").and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Prune: remove files named `unused*`; wipe: remove every entry but the guard. Never follows
/// links (they are removed as entries).
fn clean(dir: &Path, wipe: bool) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if name == GUARD {
            continue;
        }
        if ft.is_dir() {
            if wipe {
                let _ = fs::remove_dir_all(&p);
            } else {
                clean(&p, false);
            }
        } else if wipe || name.starts_with("unused") {
            let _ = fs::remove_file(&p);
        }
    }
}

fn record(dir: &Path, tool: &str, args: &[String], script: Option<&str>) {
    let env: serde_json::Map<String, serde_json::Value> = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                serde_json::Value::String(v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let cwd = std::env::current_dir()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    let rec = serde_json::json!({ "argv": args, "cwd": cwd, "env": env, "script": script });
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{tool}.calls.jsonl")))
    {
        let _ = writeln!(f, "{rec}");
    }
}

fn read_behavior(dir: &Path, tool: &str) -> BTreeMap<String, String> {
    fs::read_to_string(dir.join(format!("{tool}.behavior")))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}
