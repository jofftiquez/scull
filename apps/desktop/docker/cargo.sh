#!/usr/bin/env bash
# Run cargo for the desktop app crate (apps/desktop/src-tauri) inside the Linux build image,
# for hosts without WebKitGTK. The repository is mounted; build output and the registry cache
# live in Docker volumes, so the host's target/ is never touched.
#
#   apps/desktop/docker/cargo.sh build --locked
#   apps/desktop/docker/cargo.sh test --locked
#   apps/desktop/docker/cargo.sh clippy --locked --all-targets -- -D warnings
#
# Needs the frontend bundle first (Tauri embeds ../dist): pnpm --filter @scull/desktop build
set -euo pipefail
root=$(git rev-parse --show-toplevel)
image=scull-desktop-build:1
[ -d "$root/apps/desktop/dist" ] || {
  echo "apps/desktop/dist is missing: run 'pnpm --filter @scull/desktop build' first" >&2
  exit 2
}
docker image inspect "$image" >/dev/null 2>&1 || docker build -t "$image" "$root/apps/desktop/docker"
# Runs as root (the image's toolchain and the cache volumes are root's), then hands the files
# tauri-build generates in the mounted tree (src-tauri/gen, gitignored) back to you.
# RUSTUP_TOOLCHAIN pins the image's 1.97.x toolchain (rust-toolchain.toml says "1.97").
exec docker run --rm \
  -e RUSTUP_TOOLCHAIN=1.97.1 -e CARGO_TARGET_DIR=/target \
  -v scull-desktop-target:/target -v scull-desktop-registry:/usr/local/cargo/registry \
  -v "$root:/work" -w /work/apps/desktop/src-tauri \
  -e HOST_IDS="$(id -u):$(id -g)" \
  "$image" sh -c 'cargo "$@"; s=$?; chown -R "$HOST_IDS" gen 2>/dev/null; exit $s' cargo "$@"
