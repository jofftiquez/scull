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

### macOS and Windows: GitHub-hosted runners

The `Bench baseline` workflow (`.github/workflows/bench-baseline.yml`, manual dispatch) runs the
same generator and runner on `ubuntu-latest`, `macos-latest` and `windows-latest`. Results are
appended below when it has run.

## Reference hardware (P0-PERF-01)

The PRD proposes an 8-core laptop, 16 GB RAM and NVMe SSD per OS. Until the maintainer records
physical machines, the **reference machines are GitHub's hosted runners**, because anyone can
reproduce a run and the workflow records the exact CPU/RAM it got:

| OS | Runner | Spec (as reported by the workflow's Hardware step) |
|---|---|---|
| Linux | `ubuntu-latest` | filled in from the first run |
| macOS | `macos-latest` | filled in from the first run |
| Windows | `windows-latest` | filled in from the first run (NTFS, Defender on) |

Physical laptop numbers stay a maintainer task (needs real hardware).

## What this means for the PRD §21.2 targets

- **Full scan, 1M entries, warm: ≤ 15 s.** gdu does it in ~3–4.5 s and `du` in ~6 s on this VM.
  15 s is comfortably achievable on Linux. Target for scull's scanner: **within 1.5× of gdu**
  (scull does more per entry: identity, hardlink dedupe, detector checks).
- **Peak memory, 1M entries: ≤ 400 MB.** gdu keeps the whole tree in memory at 75 MB; dua uses
  14.5 MB. 400 MB is generous; scull should aim for **≤ 150 MB** by aggregating per directory
  instead of holding every entry (it stores rollups, not per-file rows; DATA-02).
- Windows (NTFS + Defender) is expected to be several times slower per entry; the runner numbers
  will set its own target. Per-OS release gates are allowed (PRD §19.1).
- Revisit these targets once the scull scanner (P1-SCAN-01) has numbers on the same trees.
