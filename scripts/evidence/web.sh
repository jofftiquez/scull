#!/usr/bin/env bash
# Browser screenshot for Evidence comments, via the official Playwright
# container (no system browser libraries needed on the host).
#
#   scripts/evidence/web.sh <url-or-repo-relative-html> <out.png> [width] [height] [fullPage]
# Write <out.png> under .evidence/<issue>/ (gitignored); post.py attaches it to the issue.
#
# Local files are mounted read-only at /work; use repo-relative paths or file:// URLs under the
# repo. Uses host networking so http://localhost:<port> dev servers work.
# Needs: docker, image mcr.microsoft.com/playwright:v1.64.1-noble, and a node_modules dir with
# playwright@1.64.1 (PW_NODE_MODULES, default: scripts/evidence/node_modules).
set -euo pipefail
[ $# -ge 2 ] || { echo "usage: $0 <url|file> <out.png> [w] [h] [full]" >&2; exit 2; }
root=$(git rev-parse --show-toplevel)
src=$1; out=$2; w=${3:-1100}; h=${4:-1400}; full=${5:-false}
mods=${PW_NODE_MODULES:-$root/scripts/evidence/node_modules}
case "$src" in
  http://*|https://*) url=$src ;;
  file://*) url="file:///work/${src#file://$root/}" ;;
  *) url="file:///work/$src" ;;
esac
outdir=$(cd "$(dirname "$out")" && pwd); outname=$(basename "$out")
docker run --rm --network host --user "$(id -u):$(id -g)" -e HOME=/tmp \
  -v "$root:/work:ro" -v "$outdir:/out" -v "$mods:/pw/node_modules:ro" \
  -v "$root/scripts/evidence/shot.mjs:/pw/shot.mjs:ro" -w /pw \
  mcr.microsoft.com/playwright:v1.64.1-noble \
  node /pw/shot.mjs "$url" "/out/$outname" "$w" "$h" "$full"
