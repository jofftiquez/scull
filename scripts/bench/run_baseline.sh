#!/usr/bin/env bash
# Baseline: time + peak RSS of gdu, dua and du on a generated tree (P0-PERF-01).
#   scripts/bench/run_baseline.sh <tree> [runs=3]
# Expects `gdu`, `dua` on PATH (or GDU/DUA env vars). Read-only: tools only stat the tree.
set -euo pipefail
tree=$1; runs=${2:-3}
GDU=${GDU:-gdu}; DUA=${DUA:-dua}
entries=$(find "$tree" | wc -l)
echo "tree: $tree  entries: $entries"
measure() { # name cmd...
  local name=$1; shift
  "$@" >/dev/null 2>&1 # warm the cache
  for i in $(seq 1 "$runs"); do
    /usr/bin/time -f "%e %M" -o /tmp/.scull-bench-time "$@" >/dev/null 2>&1
    read -r secs kb < /tmp/.scull-bench-time
    printf '%-6s run %d  %6.2f s  %7.1f MB peak RSS\n' "$name" "$i" "$secs" "$(echo "$kb/1024" | bc -l)"
  done
}
measure gdu "$GDU" -n -p "$tree"
measure dua "$DUA" aggregate "$tree"
measure du  du -s "$tree"
rm -f /tmp/.scull-bench-time
