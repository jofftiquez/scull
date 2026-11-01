# scull benchmarks

Baselines that calibrate the performance targets in PRD §21.2. Owned by the scanner/perf lane.
Every result lists hardware, filesystem and conditions. Numbers from shared or virtualized
machines are noisy: compare **min** and **median** over several runs, never a single run.

## Method

- **Trees** are generated, never real user data: `scripts/bench/gen_tree.py <dest> --entries N`
  builds a deterministic, developer-shaped tree (projects with `src/`, lockfiles, and deep
  `node_modules/` or `target/` subtrees with many small files). Seed 1.
  - 100K entries: 42 projects.
  - 1M entries: 428 projects, 11 GB logical / 13 GB allocated.
- **Runner**: `scripts/bench/baseline.py <tree> --runs N --tool gdu=… --tool dua=…`. Each tool
  is run once to warm the cache, then N timed runs. Wall time via `perf_counter`; peak RSS via
  GNU `time` on Linux and `psutil` polling elsewhere.
- **Tools**: gdu v5.38.0 (`gdu -n -p`, non-interactive), dua v2.46.0 (`dua aggregate`), GNU
  `du -s` as a floor reference. All only stat the tree (read-only).
- **Warm cache only** for now. Cold-cache numbers need root to drop caches (`nightly`).

## Results

### Linux: development VM (Oct 31, 2026)

| | |
|---|---|
| CPU | AMD EPYC (virtualized, QEMU), 18 vCPU |
| RAM | 94 GB |
| Disk / FS | virtual disk (`QEMU HARDDISK`, non-rotational), ext4, 84% full |
| OS | Linux 6.8.0 |
| Load | shared VM; other builds were running, so there's noise |

5 runs each, warm cache:

| Tree | Tool | median s | min s | peak MB |
|---|---|---|---|---|
| 100K | gdu | 0.75 | 0.62 | 32.0 |
| 100K | dua | 1.39 | 1.36 | 4.0 |
| 100K | du | 0.67 | 0.59 | 2.1 |
| 1M | gdu | 4.43 | 2.82 | 75.3 |
| 1M | dua | 13.54 | 12.58 | 14.5 |
| 1M | du | 5.83 | 5.57 | 2.1 |

### GitHub-hosted runners (Oct 31, 2026)

Run: [Bench baseline #41193392870](https://github.com/jofftiquez/scull/actions/runs/41193392870)
(`.github/workflows/bench-baseline.yml`, manual dispatch). 2 runs each after one warm-up.

| Runner | Reported hardware | Tree | gdu median / min s | gdu MB | dua median / min s | dua MB |
|---|---|---|---|---|---|---|
| `macos-latest` | macOS 26.6 arm64, 5 CPUs, 15 GB, APFS | 100K | 0.21 / 0.19 | 30.8 | 0.16 / 0.15 | 9.5 |
| `macos-latest` | | 1M | 35.91 / 26.63 | 71.2 | 3.59 / 3.51 | 22.5 |
| `windows-latest` | Windows Server 2025, 2 CPUs, 8.6 GB, NTFS, Defender on | 100K | 0.61 / 0.54 | 20.5 | 0.90 / 0.88 | 13.6 |
| `windows-latest` | | 1M | 6.56 / 6.53 | 44.6 | 7.99 / 7.98 | 87.4 |
| `ubuntu-latest` | Linux 7.0 (Azure), 2 CPUs, 8.3 GB, ext4 | 100K | 0.37 / 0.36 | 22.3 | 0.82 / 0.79 | 4.3 |
| `ubuntu-latest` | | 1M | not run: runner disk too small for the 13 GB tree | | | |

Notes:
- gdu on macOS degrades sharply at 1M entries (27–36 s) while dua stays at ~3.5 s; the tool
  matters more than the OS there. Per-entry work on APFS is not the bottleneck.
- Windows with Defender is ~2× slower per entry than Linux at 1M for gdu, and dua's memory grows
  on NTFS (87 MB).
- Generating the 1M tree takes ~98 s on macOS and several minutes on Windows; the 1M Linux
  runner case needs a smaller-payload tree (follow-up: `gen_tree.py --payload small`).

### scull scanner (P1-SCAN-01, same 100K/1M trees, Linux dev VM)

Release build of `crates/scull-scanner/examples/scan.rs`, 16 threads, warm cache:
100K entries in 0.08–0.21 s; **1M entries in 1.1–2.1 s at ~3 MB peak RSS**; cancellation in
0.5–13 ms; first directory summary after 1–60 ms. That beats gdu (2.8–4.4 s) on the same tree.
Hardlink dedupe (#37) and detectors will add per-entry work; re-measure after them.

## Reference hardware (P0-PERF-01)

The PRD proposes an 8-core laptop, 16 GB RAM and NVMe SSD per OS. Until the maintainer records
physical machines, the **reference machines are GitHub's hosted runners**, because anyone can
reproduce a run and the workflow records the exact CPU/RAM it got:

| OS | Runner | Spec (reported by the workflow, Oct 31, 2026) |
|---|---|---|
| Linux | `ubuntu-latest` | 2 CPUs, 8.3 GB RAM, Azure VM, ext4 |
| macOS | `macos-latest` | Apple Silicon arm64, 5 CPUs, 15 GB RAM, APFS |
| Windows | `windows-latest` | 2 CPUs, 8.6 GB RAM, Windows Server 2025, NTFS, Defender on |

Physical laptop numbers stay a maintainer task (needs real hardware).

**Runner change (Nov 1, 2026).** `macos-latest` now reports `Apple M1 (Virtual)`, **3 vCPUs,
7.5 GB RAM**, APFS, smaller than the row above. On it FX-FS-15 (1M entries, judged warm run)
measured 13.5–16.5 s for the same code and up to 21 s with cold caches. A `sample` profile of the
scan shows ~87% of busy worker time inside the kernel's `getattrlistbulk` (APFS directory
enumeration); scull's own code is under 2%. The same engine scans the same tree in 1–1.7 s on the
2-CPU Linux runner and 7–13 s on Windows.

So the nightly keeps **≤ 15 s** on Linux and Windows, and gives `macos-latest` a
runner-calibrated bound of **≤ 25 s** (`SCULL_FX_FS15_MAX_SECS` in `nightly-fixtures.yml`,
about 1.2× the worst judged time seen). The ≤ 15 s target for an 8-core Mac (PRD §21.2) still
stands and needs a measurement on physical hardware (#91).

## What this means for the PRD §21.2 targets

- **Full scan, 1M entries, warm: ≤ 15 s.** gdu does it in ~3–4.5 s and `du` in ~6 s on this VM.
  15 s is comfortably achievable on Linux. Target for scull's scanner: **within 1.5× of gdu**
  (scull does more per entry: identity, hardlink dedupe, detector checks).
- **Peak memory, 1M entries: ≤ 400 MB.** gdu keeps the whole tree in memory at 75 MB; dua uses
  14.5 MB. 400 MB is generous; scull should aim for **≤ 150 MB** by aggregating per directory
  instead of holding every entry (it stores rollups, not per-file rows; DATA-02).
- Windows (NTFS + Defender, 2 CPUs): gdu 6.6 s for 1M. A ≤ 15 s Windows target is realistic on
  runner-class hardware; per-OS release gates are allowed (PRD §19.1).
- macOS: dua 3.6 s for 1M on 5 cores, so ≤ 15 s holds on Apple Silicon too. On today's 3-vCPU
  runner scull is kernel-bound at 13.5–21 s; see the runner change note above.
- Revisit these targets once the scull scanner (P1-SCAN-01) has numbers on the same trees.
