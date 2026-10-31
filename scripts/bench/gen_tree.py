#!/usr/bin/env python3
"""Generate a deterministic, developer-shaped directory tree for benchmarks (P0-PERF-01).

    python3 scripts/bench/gen_tree.py <dest> --entries 100000 [--seed 1]

Shape (per project): source files, a `target/` or `node_modules/` subtree with deep nesting
and many small files, a few larger build outputs. Total entries (files + dirs) ≈ --entries.
Writes only inside <dest>, which must not exist or must be empty.
"""
import argparse, os, random, sys


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("dest")
    ap.add_argument("--entries", type=int, default=100_000)
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    dest = os.path.abspath(a.dest)
    if os.path.exists(dest) and os.listdir(dest):
        sys.exit(f"{dest} exists and is not empty")
    os.makedirs(dest, exist_ok=True)
    rnd = random.Random(a.seed)
    made = 0
    proj = 0
    blob = b"\0" * (1 << 20)

    def mkfile(path, size):
        nonlocal made
        with open(path, "wb") as f:
            f.write(blob[:size])
        made += 1

    def mkdir(path):
        nonlocal made
        os.mkdir(path)
        made += 1

    while made < a.entries:
        proj += 1
        root = os.path.join(dest, f"project-{proj:04d}")
        mkdir(root)
        node = rnd.random() < 0.6
        mkfile(os.path.join(root, "package.json" if node else "Cargo.toml"), 600)
        mkfile(os.path.join(root, "package-lock.json" if node else "Cargo.lock"), 40_000)
        src = os.path.join(root, "src")
        mkdir(src)
        for i in range(rnd.randint(20, 80)):
            mkfile(os.path.join(src, f"mod{i}.{'ts' if node else 'rs'}"), rnd.randint(200, 12_000))
        art = os.path.join(root, "node_modules" if node else "target")
        mkdir(art)
        budget = rnd.randint(500, 4000)
        stack = [(art, 0)]
        while budget > 0 and made < a.entries:
            d, depth = stack[rnd.randrange(len(stack))]
            if depth < 8 and rnd.random() < 0.15:
                nd = os.path.join(d, f"pkg{budget}")
                mkdir(nd)
                stack.append((nd, depth + 1))
            else:
                size = rnd.choice([0, 120, 900, 2_500, 8_000, 30_000]) if rnd.random() > 0.01 else 600_000
                mkfile(os.path.join(d, f"f{budget}.js" if node else f"f{budget}.o"), size)
            budget -= 1
    print(f"{dest}: {made} entries in {proj} projects")


if __name__ == "__main__":
    main()
