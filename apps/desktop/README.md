# scull desktop

Tauri 2 + Vue 3 app. Built in Phase 3 (P3-APP-01). See `design/html/index.html` for the reference.

The Rust side (`src-tauri/`) is a thin bridge to the `scull-*` crates and never deletes files directly (SAFE-10).
