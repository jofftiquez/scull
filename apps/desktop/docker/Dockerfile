# Linux build environment for the desktop's Rust side (apps/desktop/src-tauri).
#
# Tauri on Linux links WebKitGTK, which most dev hosts and the main CI job don't have. This
# image adds the system libraries so `cargo build/test` of scull-desktop runs anywhere Docker
# does. The repository is mounted, never copied; see apps/desktop/README.md.
FROM rust:1.97.1-bookworm

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      libwebkit2gtk-4.1-dev \
      libsoup-3.0-dev \
      libgtk-3-dev \
      libayatana-appindicator3-dev \
      librsvg2-dev \
      libssl-dev \
      pkg-config \
      file \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt

WORKDIR /work
