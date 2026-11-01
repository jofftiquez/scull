#!/usr/bin/env bash
# Desktop E2E (P3-E2E-01): build the app, serve it and run the Playwright flows, all inside the
# official Playwright container (its browsers and system libraries; nothing is installed on
# the host beyond `pnpm install`).
#
#   apps/desktop/e2e/run.sh [--dev] [--shots DIR] [--repeat N] [-- <playwright test args>]
#
#   --dev        serve with the Vite dev server instead of building (faster while iterating)
#   --shots DIR  copy the flows' screenshots to DIR afterwards (e.g. .evidence/107)
#   --repeat N   run the whole suite N times (--repeat-each), to prove it isn't flaky
#
# Needs: docker, the image below, and `pnpm install --frozen-lockfile` (for @playwright/test,
# pinned to the image's version, and Vite). What runs in the container is e2e/in-container.sh
# (the CI job runs the same script): by default `vite build` with VITE_SCULL_E2E=1, which only
# adds the mock plan bridge's test hook, into e2e/.dist (dist/ is untouched), then the flows.
# The app runs on the mock bridge: no Tauri, nothing on disk changes, and requests outside the
# app's origin are aborted.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
app=$(cd "$here/.." && pwd)
root=$(cd "$app/../.." && pwd)
version=1.64.1
image=mcr.microsoft.com/playwright:v$version-noble

server=preview
shots=""
extra=()
while [ $# -gt 0 ]; do
  case "$1" in
    --dev) server=dev ;;
    --shots) shots=$2; shift ;;
    --repeat) extra+=("--repeat-each=$2"); shift ;;
    --) shift; extra+=("$@"); break ;;
    *) echo "usage: $0 [--dev] [--shots DIR] [--repeat N] [-- <playwright args>]" >&2; exit 2 ;;
  esac
  shift
done

pkg=$app/node_modules/@playwright/test/package.json
if [ ! -f "$pkg" ]; then
  echo "missing @playwright/test: run 'pnpm install --frozen-lockfile' first" >&2
  exit 2
fi
if ! grep -q "\"version\": \"$version\"" "$pkg"; then
  echo "@playwright/test must be $version to match $image" >&2
  exit 2
fi

rm -rf "$here/shots"
status=0
docker run --rm --init --ipc=host --user "$(id -u):$(id -g)" -e HOME=/tmp -e CI -e E2E_WORKERS \
  -v "$root:/work" -w /work/apps/desktop "$image" \
  bash e2e/in-container.sh "$server" "${extra[@]+"${extra[@]}"}" \
  || status=$?

if [ -n "$shots" ] && [ -d "$here/shots" ]; then
  mkdir -p "$shots"
  cp "$here"/shots/*.png "$shots"/
  echo "screenshots copied to $shots"
fi
exit "$status"
