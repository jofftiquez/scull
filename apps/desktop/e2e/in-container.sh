#!/usr/bin/env bash
# The part of the E2E run that happens inside mcr.microsoft.com/playwright:v1.64.1-noble (run.sh
# starts the container locally; the CI job runs in it). From apps/desktop:
#
#   bash e2e/in-container.sh [preview|dev] [<playwright test args>]
#
# preview (default): `vite build` with VITE_SCULL_E2E=1 into e2e/.dist (the shipped dist/ is
# untouched), served by `vite preview`; dev: the Vite dev server. Playwright starts the server.
set -euo pipefail
cd "$(dirname "$0")/.."
server=${1:-preview}
shift || true
if [ "$server" = preview ]; then
  VITE_SCULL_E2E=1 node node_modules/vite/bin/vite.js build --outDir e2e/.dist --emptyOutDir --logLevel warn
fi
E2E_SERVER=$server exec node node_modules/@playwright/test/cli.js test -c e2e/playwright.config.ts "$@"
