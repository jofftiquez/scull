//! Print what scull sees in Docker on this machine, read-only (P5-DET-03).
//!
//! `cargo run -p scull-detectors --example docker_report`
//!
//! Talks to the Docker daemon with `GET` requests on read-only endpoints only and never runs a
//! command; the proposed actions are printed, not executed.

use scull_detectors::caches::CacheEnv;
use scull_detectors::detector::ConfigReader;
use scull_detectors::rules::Platform as Os;
use scull_detectors::{DockerOptions, DockerStatus, inspect_docker};
use scull_platform::Native;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Reads small regular files (example only; the library never touches `std::fs`).
struct StdReader;

impl ConfigReader for StdReader {
    fn read_text(&self, path: &Path, max_bytes: u64) -> Option<String> {
        let md = std::fs::symlink_metadata(path).ok()?;
        if !md.is_file() || md.len() > max_bytes {
            return None;
        }
        let mut s = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(max_bytes)
            .read_to_string(&mut s)
            .ok()?;
        Some(s)
    }
}

fn human(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn main() {
    let Some(os) = Os::current() else {
        eprintln!("unsupported OS");
        return;
    };
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_default();
    let env = CacheEnv::new(home, os, |k| std::env::var(k).ok());
    let started = Instant::now();
    let r = inspect_docker(&env, &Native, Some(&StdReader), &DockerOptions::default());
    println!("scull docker report (read-only, inspect-only)");
    println!("status: {}", r.status.summary());
    if let DockerStatus::Connected { daemon, .. } = &r.status {
        println!(
            "daemon: {} | kernel {} | in VM: {}",
            daemon.platform, daemon.kernel, daemon.in_vm
        );
    }
    if let Some(i) = r.images {
        println!(
            "images: {} ({} summed; layers are shared, so disk use is lower)",
            i.count,
            human(i.bytes)
        );
    }
    for g in &r.groups {
        let total = g.total_bytes.map_or("not measured".to_owned(), human);
        let action = match &g.action {
            scull_core::Action::ToolCommand { tool, args } => {
                format!("{tool} {} (proposed; not run)", args.join(" "))
            }
            _ => "none (inspect-only)".to_owned(),
        };
        println!(
            "\n[{:?}] {}: {} objects, {total}; reclaimable {} ({:?} confidence)",
            g.tier,
            g.kind,
            g.count,
            human(g.reclaimable.bytes),
            g.reclaimable.confidence
        );
        println!("  action: {action}");
        println!("  why: {}", g.reason);
        if !g.notes.is_empty() {
            println!("  notes: {:?}", g.notes);
        }
    }
    for (kind, why) in &r.unavailable {
        println!("\n{kind}: unavailable ({why})");
    }
    if r.vm_disks.is_empty() {
        println!("\nVM disk images: none found for this user");
    }
    for d in &r.vm_disks {
        println!(
            "\n[{:?}] {}: {} (logical {}, allocated {}) action: none (never offered)",
            d.tier,
            d.kind,
            d.path.display(),
            human(d.logical),
            d.allocated.map_or("unknown".to_owned(), human)
        );
        println!("  caveat: {}", d.kind.caveat());
        println!("  compaction: {}", d.kind.compaction());
    }
    println!("\ntook {:.1} s", started.elapsed().as_secs_f64());
}
