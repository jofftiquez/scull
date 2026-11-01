#!/usr/bin/env bash
# Accessibility audit (P3-A11Y-01) in the Playwright container, against a running dev server:
#
#   SCULL_DEV_PORT=5291 pnpm --filter @scull/desktop dev &      # mock bridge
#   apps/desktop/a11y/audit.sh http://localhost:5291 <out-dir>
#
# Writes <out-dir>/a11y.json and one screenshot per route. A11Y_COLOR_SCHEME=dark audits the dark
# theme (P5-UI-01). Needs docker, the image
# mcr.microsoft.com/playwright:v1.64.1-noble, PW_NODE_MODULES (playwright@1.64.1, as for
# scripts/evidence/web.sh) and AXE_NODE_MODULES (axe-core@4.14.0). axe-core is MPL-2.0: it is
# a tool here, installed outside the repository (`npm install axe-core@4.14.0` in any scratch
# directory), never a dependency of the app.
set -euo pipefail
[ $# -ge 2 ] || { echo "usage: $0 <base-url> <out-dir>" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
base=$1
mkdir -p "$2"
out=$(cd "$2" && pwd)
pw=${PW_NODE_MODULES:-$root/scripts/evidence/node_modules}
axe=${AXE_NODE_MODULES:?set AXE_NODE_MODULES to a node_modules dir with axe-core@4.14.0}
docker run --rm --network host --ipc=host --user "$(id -u):$(id -g)" -e HOME=/tmp -e A11Y_COLOR_SCHEME \
  -v "$out:/out" -v "$pw:/pw/node_modules:ro" -v "$axe:/axe/node_modules:ro" \
  -v "$here/audit.mjs:/pw/audit.mjs:ro" -w /pw \
  mcr.microsoft.com/playwright:v1.64.1-noble \
  node /pw/audit.mjs "$base" /out/a11y.json /out/shots
