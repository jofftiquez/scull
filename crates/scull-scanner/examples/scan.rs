//! Scan a directory and print throughput: a small benchmark harness for the traversal.
//!
//! ```text
//! cargo run --release -p scull-scanner --example scan -- <path> [--threads N] [--listings]
//!     [--runs N] [--max-depth N] [--cancel-after-ms N]
//! ```
//!
//! `--listings` also builds every per-directory entry listing (what detectors consume).
//! `--cancel-after-ms` cancels the scan after N ms and reports how long it took to stop.
//! Read-only: nothing is modified.

use scull_platform::Native;
use scull_scanner::{CancelToken, ScanEvent, ScanOptions, scan};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn parse<T: std::str::FromStr>(v: Option<String>, flag: &str) -> Result<T, String> {
    v.ok_or_else(|| format!("{flag} needs a value"))?
        .parse()
        .map_err(|_| format!("bad value for {flag}"))
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut root: Option<PathBuf> = None;
    let mut opts = ScanOptions::default();
    let mut runs = 1usize;
    let mut cancel_after: Option<u64> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--threads" => opts.threads = parse(args.next(), "--threads")?,
            "--listings" => opts.emit_listings = true,
            "--runs" => runs = parse(args.next(), "--runs")?,
            "--max-depth" => opts.max_depth = Some(parse(args.next(), "--max-depth")?),
            "--cancel-after-ms" => cancel_after = Some(parse(args.next(), "--cancel-after-ms")?),
            _ if root.is_none() => root = Some(PathBuf::from(a)),
            _ => return Err(format!("unexpected argument {a}")),
        }
    }
    let root = root.ok_or("usage: scan <path> [--threads N] [--listings] [--runs N]")?;

    for run in 1..=runs {
        let cancel = CancelToken::new();
        let listed = Arc::new(AtomicU64::new(0));
        let first_result = Arc::new(AtomicU64::new(0));
        let (l, f) = (Arc::clone(&listed), Arc::clone(&first_result));
        let started = Instant::now();
        let sink = move |e: ScanEvent| match e {
            ScanEvent::Listing(d) => {
                l.fetch_add(d.entries.len() as u64, Ordering::Relaxed);
            }
            ScanEvent::DirDone(_) => {
                let us = started.elapsed().as_micros() as u64;
                let _ = f.compare_exchange(0, us.max(1), Ordering::Relaxed, Ordering::Relaxed);
            }
            _ => {}
        };
        let canceller = cancel_after.map(|ms| {
            let c = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                let at = Instant::now();
                c.cancel();
                at
            })
        });
        let out = scan(Arc::new(Native), &root, &opts, &cancel, sink).map_err(|e| e.to_string())?;
        let t = out.totals;
        let secs = out.elapsed.as_secs_f64();
        println!(
            "run {run}: {:?} in {:.3} s with {} threads: {} entries ({} files, {} dirs, {} links, \
             {} special), {:.1} MiB logical, {:.1} MiB allocated, {} issues, {} boundaries, \
             {:.0} entries/s, first DirDone after {:.1} ms{}",
            out.status,
            secs,
            out.threads,
            t.entries,
            t.files,
            t.dirs,
            t.links,
            t.specials,
            t.logical as f64 / 1048576.0,
            t.allocated as f64 / 1048576.0,
            t.issues,
            t.boundaries,
            t.entries as f64 / secs,
            first_result.load(Ordering::Relaxed) as f64 / 1000.0,
            if opts.emit_listings {
                format!(", {} entries in listings", listed.load(Ordering::Relaxed))
            } else {
                String::new()
            }
        );
        if let Some(r) = &out.root {
            println!(
                "  hard links: {} multi-linked paths, {} deduplicated, peak {} identities \
                 tracked; root reclaimable {:.1} MiB, shared {:.1} MiB ({:?} confidence)",
                t.multi_linked,
                t.hardlinks_deduped,
                out.peak_link_identities,
                r.unique_allocated as f64 / 1048576.0,
                r.shared_allocated as f64 / 1048576.0,
                r.reclaim_confidence,
            );
        }
        if let Some(h) = canceller {
            let at = h.join().map_err(|_| "canceller panicked")?;
            println!(
                "  cancel -> return: {:.1} ms",
                at.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    Ok(())
}
