//! FX-FS nightly fixtures (docs/FIXTURES.md §3.3, mode **Nightly**; P1-FX-06): the two
//! filesystem fixtures that need mount privileges or minutes of runtime, so they never run in
//! the per-PR `cargo test --workspace` (they are `#[ignore]`d). Run them with:
//!
//! ```text
//! cargo test -p scull-fixtures --release --test fx_fs_nightly -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The nightly workflow (`.github/workflows/nightly-fixtures.yml`) runs exactly that on Linux
//! and macOS (FX-FS-15 on Windows too) with `SCULL_FIXTURES_REQUIRE_PRIVILEGED=1` and
//! `SCULL_FIXTURES_ALLOW_SUDO=1`, so a missing privilege fails the job instead of skipping.
//!
//! * **FX-FS-04** (Linux; macOS through a disk image): a filesystem mounted inside a candidate
//!   makes it S3, the mounted content is untouched, and I-1, I-2, I-3 and I-8 hold. Linux
//!   mounts a tmpfs (its own device number), bind-mounts the canary directory (same device:
//!   only the mount table shows it) and, as real root, loop-mounts an ext4 image; macOS
//!   attaches an HFS+ disk image with `hdiutil`. Privileges come from
//!   [`scull_fixtures::require_privileges!`] (see `scull_fixtures::privileged`).
//! * **FX-FS-15** (all OSs): a 1M-entry tree is scanned by the engine within the PRD §21.2
//!   targets: ≤ 15 s warm and ≤ 400 MB peak memory, measured in a child process (one warm-up
//!   run, then the judged run). The time target is enforced in optimized builds only.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// FX-FS-04 (and the helpers only it uses) is compiled for Linux and macOS only.
#![cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(dead_code, unused_imports)
)]

use scull_core::config::Config;
use scull_core::{Candidate, ItemId, ReasonCode, Tier};
use scull_engine::paths::canon;
use scull_engine::{
    CancelToken, Engine, EngineConfig, FixedInUse, InUseSource, ScanReport, ScanRequest, now_unix,
};
use scull_fixtures::{Fixture, InvariantInputs, PlanEntry, days_ago};
use scull_platform::Native;
use scull_safety::Tri;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// An engine with every signal observable in a fresh fixture: idle (nothing has a handle in a
/// tree this test just built), no Git, no history.
fn engine() -> Engine {
    let mut cfg = EngineConfig::new(Config::default(), Arc::new(Native));
    // Temporary directories are tmpfs or overlay on some machines and containers; the D-22
    // root policy is tested in scull-platform and scull-engine.
    cfg.allow_network_roots = true;
    cfg.in_use = InUseSource::Probe(Arc::new(FixedInUse(Tri::No)));
    Engine::new(cfg).unwrap()
}

fn scan(e: &Engine, root: &Path) -> ScanReport {
    e.scan(
        &[root.to_path_buf()],
        &ScanRequest::full().history(false).at(now_unix()),
        &CancelToken::new(),
        |_| {},
    )
    .unwrap()
}

fn rel(root: &Path, c: &Candidate) -> String {
    let p = c.path.to_path_buf();
    p.strip_prefix(root)
        .unwrap_or(&p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn at<'a>(r: &'a ScanReport, root: &Path, rel_path: &str) -> &'a Candidate {
    let hits: Vec<&Candidate> = r
        .candidates
        .iter()
        .filter(|c| rel(root, c) == rel_path)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected one candidate at {rel_path}, got {:?}",
        r.candidates
            .iter()
            .map(|c| rel(root, c))
            .collect::<Vec<_>>()
    );
    hits[0]
}

fn tiers(r: &ScanReport) -> BTreeMap<ItemId, Tier> {
    r.candidates
        .iter()
        .map(|c| (c.id.clone(), c.tier))
        .collect()
}

fn plan(r: &ScanReport) -> Vec<PlanEntry> {
    r.candidates
        .iter()
        .filter(|c| c.action.is_mutating())
        .map(PlanEntry::from)
        .collect()
}

// ---------------------------------------------------------------------------------------------

/// FX-FS-04: a mount inside a candidate → S3 (reason "crosses a mount"), never in a plan, and
/// the mounted content is untouched (I-1, I-2, I-3, I-8).
#[test]
#[ignore = "nightly: mounts filesystems (needs a user+mount namespace, root or hdiutil); run with --ignored"]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fx_fs_04_mount_inside_candidate_is_s3_and_mounted_content_untouched() {
    use scull_fixtures::privileged::{Need, is_real_root, tool_path};
    use scull_fixtures::{Mounts, TreeSnapshot, diff};

    let how = scull_fixtures::require_privileges!(Need::Mounts);
    println!("FX-FS-04: privileges: {how}");

    let mut fx = Fixture::new().unwrap();
    for p in ["tmpfs", "bound", "looped", "plain"] {
        fx.cargo_project(p, p)
            .unwrap()
            .cargo_target(p, p, 4096)
            .unwrap();
    }
    for p in ["tmpfs", "bound", "looped", "plain"] {
        fx.age(format!("{p}/target"), days_ago(3)).unwrap();
    }
    fx.dir("tmpfs/target/mnt")
        .unwrap()
        .dir("bound/target/mnt")
        .unwrap()
        .dir("looped/target/mnt")
        .unwrap();
    // Mounted after `fx`, so dropped (unmounted) before the fixture's directory is removed.
    let mut mounts = Mounts::new(&fx);
    let mut mounted: Vec<&str> = Vec::new();
    if cfg!(target_os = "linux") {
        // A tmpfs: its own device number, seen by the walk.
        mounts
            .tmpfs(&fx.root().join("tmpfs/target/mnt"), 8)
            .unwrap();
        mounted.push("tmpfs/target");
        // The canary directory, bind-mounted: same device number, seen only in the mount
        // table. I-2 then proves the canary's content survived.
        let canary = fx.canary().dir.clone();
        mounts
            .bind(&canary, &fx.root().join("bound/target/mnt"))
            .unwrap();
        mounted.push("bound/target");
        // Real root only: a loop-mounted ext4 image (the image file sits beside the root).
        if is_real_root() && tool_path("mkfs.ext4").is_some() {
            mounts
                .loop_image(
                    &fx.base().join("vol.img"),
                    "ext4",
                    16,
                    &fx.root().join("looped/target/mnt"),
                )
                .unwrap();
            mounted.push("looped/target");
        } else {
            println!(
                "FX-FS-04: loop-mounted ext4 sub-case skipped (needs real root and mkfs.ext4)"
            );
        }
    } else {
        mounts
            .disk_image(
                &fx.base().join("vol.dmg"),
                "scullfx",
                16,
                &fx.root().join("tmpfs/target/mnt"),
            )
            .unwrap();
        mounted.push("tmpfs/target");
    }
    // Content on the mounted filesystems (the bind mount already shows the canary's).
    let mut on_mounts = Vec::new();
    for m in &mounted {
        let mnt = fx.root().join(m).join("mnt");
        if !m.starts_with("bound") {
            std::fs::create_dir_all(mnt.join("data/nested")).unwrap();
            std::fs::write(mnt.join("data/precious.txt"), "irreplaceable\n").unwrap();
            std::fs::write(mnt.join("data/nested/more.bin"), [7u8; 4096]).unwrap();
        }
        on_mounts.push((mnt.clone(), TreeSnapshot::capture(&mnt).unwrap()));
    }
    fx.checkpoint().unwrap();
    println!("FX-FS-04: mounted inside candidates: {mounted:?}");

    let root = canon(fx.root());
    let e = engine();
    let first = scan(&e, &root);
    let second = scan(&e, &root);
    for r in [&first, &second] {
        for m in &mounted {
            let c = at(r, &root, m);
            assert_eq!(c.tier, Tier::S3, "{m}: {:?}", c.reasons);
            assert!(
                c.reasons.iter().any(|x| x.code == ReasonCode::CrossesMount),
                "{m}: {:?}",
                c.reasons
            );
            assert_eq!(c.flags.cross_mount, Some(true), "{m}");
            assert!(!c.action.is_mutating(), "{m}: {:?}", c.action);
            println!("FX-FS-04: {m} -> {:?} ({:?})", c.tier, c.reasons[0].code);
        }
        let control = at(r, &root, "plain/target");
        assert!(control.tier < Tier::S3, "{:?}", control.reasons);
        assert_ne!(control.flags.cross_mount, Some(true));
        println!("FX-FS-04: plain/target (no mount) -> {:?}", control.tier);
        if !mounted.contains(&"looped/target") {
            assert!(at(r, &root, "looped/target").tier < Tier::S3);
        }
    }
    // macOS keeps its own bookkeeping on an attached volume (FSEvents log, Spotlight index,
    // Trash folder) and may write it at any time; that is the OS, not scull.
    let bookkeeping: &[&str] = if cfg!(target_os = "macos") {
        &[".fseventsd", ".Spotlight-V100", ".Trashes"]
    } else {
        &[]
    };
    for (mnt, before) in &on_mounts {
        let after = TreeSnapshot::capture(mnt).unwrap();
        let d = diff(before, &after).without(bookkeeping);
        assert!(d.is_empty(), "mounted content changed under {mnt:?}: {d}");
    }
    let mut inputs = InvariantInputs::new()
        .plan(plan(&first))
        .tiers(tiers(&first), tiers(&second));
    for m in &mounted {
        for b in bookkeeping {
            inputs = inputs.allow(format!("{m}/mnt/{b}"));
        }
    }
    fx.check_invariants(&inputs).assert_ok();
    mounts.unmount_all().unwrap();
    println!("FX-FS-04: mounted content untouched; I-1, I-2, I-3, I-8 hold; unmounted");
}

// ---------------------------------------------------------------------------------------------

/// Set (to the tree root) in the FX-FS-15 child that runs one measured scan.
const FS15_CHILD_ENV: &str = "SCULL_FX_FS15_CHILD_ROOT";
/// Entry count (default 1,000,000; lower it only for a quick local try).
const FS15_ENTRIES_ENV: &str = "SCULL_FX_FS15_ENTRIES";
/// Time budget in seconds (default 15, PRD §21.2).
const FS15_MAX_SECS_ENV: &str = "SCULL_FX_FS15_MAX_SECS";
/// Peak memory budget in MB (default 400, PRD §21.2).
const FS15_MAX_MB_ENV: &str = "SCULL_FX_FS15_MAX_MB";
/// `1` enforces the time budget in an unoptimized build too.
const FS15_ENFORCE_TIME_ENV: &str = "SCULL_FX_FS15_ENFORCE_TIME";

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// One measured child run.
#[derive(Debug)]
struct Fs15Run {
    secs: f64,
    entries: u64,
    candidates: u64,
    peak_bytes: Option<u64>,
}

fn field(line: &str, key: &str) -> Option<String> {
    line.split_whitespace()
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        .map(str::to_owned)
}

/// The child: one engine scan of `root`; prints `FS15 ...` with its own peak memory where
/// the OS reports it to the process (Linux `VmHWM`), then, on Windows, waits on stdin so the
/// parent can read the peak working set of a live process.
fn fs15_child(root: &Path) {
    let e = engine();
    let t = std::time::Instant::now();
    let r = scan(&e, root);
    let secs = t.elapsed().as_secs_f64();
    // I-1 inside the child: nothing S3 is actionable.
    assert!(
        r.candidates
            .iter()
            .all(|c| c.tier != Tier::S3 || !c.action.is_mutating())
    );
    let hwm_kb = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        });
    println!(
        "FS15 secs={secs:.3} entries={} candidates={} vmhwm_kb={}",
        r.entries_scanned,
        r.candidates.len(),
        hwm_kb.map_or("-".to_owned(), |k| k.to_string())
    );
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    if cfg!(windows) {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
}

/// Run the child on `root` and measure it.
fn fs15_run(root: &Path) -> Fs15Run {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().unwrap();
    let test_args = [
        "--exact",
        "fx_fs_15_one_million_entries_within_time_and_memory_targets",
        "--nocapture",
        "--include-ignored",
        "--test-threads=1",
    ];
    let mac_time = cfg!(target_os = "macos") && Path::new("/usr/bin/time").is_file();
    let mut cmd = if mac_time {
        let mut c = Command::new("/usr/bin/time");
        c.arg("-l").arg(&exe);
        c
    } else {
        Command::new(&exe)
    };
    let mut child = cmd
        .args(test_args)
        .env(FS15_CHILD_ENV, root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut out = String::new();
    let mut line = String::new();
    let mut result = None;
    let mut peak_bytes = None;
    while stdout.read_line(&mut line).unwrap() > 0 {
        out.push_str(&line);
        // libtest prints `test <name> ... ` on the same line, before the child's output.
        if let Some(i) = line.find("FS15 secs=") {
            let line = line[i..].to_owned();
            peak_bytes = field(&line, "vmhwm_kb")
                .and_then(|k| k.parse::<u64>().ok())
                .map(|k| k * 1024);
            if cfg!(windows) {
                // The child waits on stdin: its peak working set is still readable.
                let q = Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-Command",
                        &format!("(Get-Process -Id {pid}).PeakWorkingSet64"),
                    ])
                    .output()
                    .unwrap();
                peak_bytes = String::from_utf8_lossy(&q.stdout).trim().parse().ok();
            }
            result = Some(line.clone());
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(b"\n");
            }
        }
        line.clear();
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "FX-FS-15 child failed ({}):\n{out}\n{stderr}",
        output.status
    );
    if mac_time {
        // BSD time -l: "<bytes>  maximum resident set size".
        peak_bytes = stderr.lines().find_map(|l| {
            l.trim()
                .strip_suffix("maximum resident set size")
                .and_then(|n| n.trim().parse().ok())
        });
    }
    let line = result.unwrap_or_else(|| panic!("no FS15 line from the child:\n{out}\n{stderr}"));
    Fs15Run {
        secs: field(&line, "secs").unwrap().parse().unwrap(),
        entries: field(&line, "entries").unwrap().parse().unwrap(),
        candidates: field(&line, "candidates").unwrap().parse().unwrap(),
        peak_bytes,
    }
}

/// FX-FS-15: a 1M-entry tree completes within the PRD §21.2 targets (full scan, warm cache,
/// ≤ 15 s; peak memory ≤ 400 MB), finds every artifact, and changes nothing (I-2, I-3).
#[test]
#[ignore = "nightly: builds and scans a 1M-entry tree (minutes); run with --release -- --ignored"]
fn fx_fs_15_one_million_entries_within_time_and_memory_targets() {
    if let Some(root) = std::env::var_os(FS15_CHILD_ENV) {
        fs15_child(Path::new(&root));
        return;
    }
    let entries: u64 = env_num(FS15_ENTRIES_ENV, 1_000_000);
    let max_secs: f64 = env_num(FS15_MAX_SECS_ENV, 15.0);
    let max_mb: u64 = env_num(FS15_MAX_MB_ENV, 400);
    let enforce_time =
        !cfg!(debug_assertions) || std::env::var(FS15_ENFORCE_TIME_ENV).is_ok_and(|v| v == "1");

    let mut fx = Fixture::new().unwrap();
    let t = std::time::Instant::now();
    let tree = fx.wide_tree("tree", entries).unwrap();
    println!(
        "FX-FS-15: built {} entries ({} projects) in {:.1} s",
        tree.entries,
        tree.projects,
        t.elapsed().as_secs_f64()
    );
    assert_eq!(tree.entries, entries);
    let t = std::time::Instant::now();
    fx.checkpoint().unwrap();
    println!(
        "FX-FS-15: I-3 baseline snapshot in {:.1} s",
        t.elapsed().as_secs_f64()
    );

    let root = canon(&fx.root().join("tree"));
    let warm = fs15_run(&root);
    println!("FX-FS-15: warm-up run: {warm:?}");
    let run = fs15_run(&root);
    println!("FX-FS-15: judged run: {run:?}");

    // Every entry below the root was counted, and every artifact found.
    assert!(
        run.entries + 1 >= entries,
        "scanned {} of {entries} entries",
        run.entries
    );
    assert_eq!(run.candidates, tree.artifacts.len() as u64);

    let peak = run
        .peak_bytes
        .expect("peak memory of the child (VmHWM / time -l / PeakWorkingSet64)");
    let peak_mb = peak as f64 / (1024.0 * 1024.0);
    println!(
        "FX-FS-15: {entries} entries in {:.2} s (target ≤ {max_secs} s{}), peak {peak_mb:.1} MB (target ≤ {max_mb} MB)",
        run.secs,
        if enforce_time {
            ""
        } else {
            ", not enforced in an unoptimized build: run with --release"
        }
    );
    assert!(
        peak_mb <= max_mb as f64,
        "peak memory {peak_mb:.1} MB > {max_mb} MB (PRD §21.2)"
    );
    if enforce_time {
        assert!(
            run.secs <= max_secs,
            "full scan took {:.2} s > {max_secs} s (PRD §21.2)",
            run.secs
        );
    }
    let t = std::time::Instant::now();
    fx.check_invariants(&InvariantInputs::new()).assert_ok();
    println!(
        "FX-FS-15: invariants checked in {:.1} s",
        t.elapsed().as_secs_f64()
    );
}
