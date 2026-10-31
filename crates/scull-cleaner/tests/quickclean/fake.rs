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

/// Write `<dir>/<tool>.behavior`.
pub fn behave(dir: &Path, tool: &str, lines: &[(&str, &str)]) {
    let text: String = lines.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    fs::write(dir.join(format!("{tool}.behavior")), text).expect("write behavior");
}

/// One recorded run of a fake.
#[derive(Debug, Clone)]
pub struct Call {
    pub argv: Vec<String>,
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
    if !TOOLS.contains(&stem.as_str()) {
        return None;
    }
    let me = if me.is_absolute() {
        me
    } else {
        std::env::current_exe().ok()?
    };
    let dir = me.parent()?.to_path_buf();
    Some(fake_main(&dir, &stem))
}

fn fake_main(dir: &Path, tool: &str) -> i32 {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a: OsString| a.to_string_lossy().into_owned())
        .collect();
    record(dir, tool, &args);
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

fn record(dir: &Path, tool: &str, args: &[String]) {
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
    let rec = serde_json::json!({ "argv": args, "cwd": cwd, "env": env });
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
