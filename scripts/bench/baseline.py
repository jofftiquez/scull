#!/usr/bin/env python3
"""Cross-platform baseline runner (P0-PERF-01): wall time + peak RSS of external scanners.

    python3 scripts/bench/baseline.py <tree> --runs 3 --tool gdu=/path/gdu --tool dua=/path/dua

Each tool is warmed once (warm cache), then run --runs times. Peak RSS uses psutil when
GNU time on Linux, else psutil polling (every 5 ms; install psutil on macOS/Windows). Prints a markdown table row per
tool: median and min seconds, median peak MB. Read-only: tools only stat the tree.
"""
import argparse, os, platform, statistics, subprocess, sys, time

ARGS = {
    "gdu": lambda b, t: [b, "-n", "-p", t],          # non-interactive, no progress
    "dua": lambda b, t: [b, "aggregate", t],
    "du": lambda b, t: [b, "-s", t],
}


def run(cmd):
    """Return (seconds, peak MB) for one run of cmd."""
    gnu_time = "/usr/bin/time"
    if sys.platform.startswith("linux") and os.path.exists(gnu_time):
        # GNU time reports the tool's own peak RSS (a forked Python child would inflate it).
        t0 = time.perf_counter()
        r = subprocess.run([gnu_time, "-f", "%M", *cmd], stdout=subprocess.DEVNULL,
                           stderr=subprocess.PIPE, text=True)
        secs = time.perf_counter() - t0
        kb = int(r.stderr.strip().splitlines()[-1])
        return secs, kb * 1024 / 1e6
    try:
        import psutil  # type: ignore
    except ImportError:
        psutil = None
    t0 = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    peak = 0
    if psutil:
        try:
            pp = psutil.Process(p.pid)
            while p.poll() is None:
                try:
                    peak = max(peak, pp.memory_info().rss)
                except psutil.Error:
                    break
                time.sleep(0.005)
        except psutil.Error:
            pass
    p.wait()
    return time.perf_counter() - t0, peak / 1e6


def count(tree):
    n = 1
    for _, ds, fs in os.walk(tree):
        n += len(ds) + len(fs)
    return n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tree")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--tool", action="append", default=[], help="name=path")
    a = ap.parse_args()
    n = count(a.tree)
    print(f"os={platform.system()} {platform.release()} machine={platform.machine()} cpus={os.cpu_count()} entries={n}")
    print("| tool | entries | median s | min s | peak MB |")
    print("|---|---|---|---|---|")
    for spec in a.tool:
        name, path = spec.split("=", 1)
        cmd = ARGS[name](path, a.tree)
        run(cmd)  # warm
        res = [run(cmd) for _ in range(a.runs)]
        secs = [r[0] for r in res]
        mem = [r[1] for r in res]
        print(f"| {name} | {n} | {statistics.median(secs):.2f} | {min(secs):.2f} | {statistics.median(mem):.1f} |", flush=True)


if __name__ == "__main__":
    main()
