# scull desktop

Tauri 2 + Vue 3 app. The design reference is `design/html/index.html`. The tokens come from
`design/tokens.json`, and the layout follows D-16…D-21: a full-height left sidebar, ink +
magenta duotone, no shadows, DM Sans and DM Mono.

**The desktop never deletes (SAFE-10).** The Rust side is a read-only bridge to the scull
crates. It doesn't depend on `scull-cleaner`, the only code that deletes, and it doesn't enable
`scull-platform`'s `delete` feature. Its window has no filesystem, shell or process
permissions. Tests enforce all three (see [Safety checks](#safety-checks)).

## Layout

| Path | What |
|---|---|
| `src/` | Vue 3 + TypeScript. `App.vue` is the shell. Screens live in `src/screens/<Screen>/`, one folder per screen lane. Shared parts go in `src/components/`. |
| `src/bridge/` | `ScullBridge`, the only way screens reach the core. `tauri.ts` calls Tauri commands (it is the only module allowed to import `@tauri-apps/api`). `mock.ts` serves the design's imagined machine. `index.ts` picks Tauri when `window.__TAURI_INTERNALS__`/`__TAURI__` exists and the mock otherwise. |
| `src/styles/theme.css` | The Tailwind v4 theme, **generated** from `design/tokens.json` by `scripts/gen-theme.mjs`. Never edit it by hand. Tailwind's default palette and every shadow utility are removed, so only token classes exist. |
| `bridge/` | `scull-desktop-bridge`: all the Rust logic behind the commands. It is a member of the root Cargo workspace, so it builds and is tested on every OS without WebKit. |
| `src-tauri/` | `scull-desktop`: the Tauri app crate, thin `#[tauri::command]` wrappers over the bridge. It is a **separate Cargo workspace**, because on Linux it links WebKitGTK and the root workspace's builds and CI must not need it. It has its own `Cargo.lock` and `deny.toml`. |
| `docker/` | Linux build image (WebKitGTK and friends) and `cargo.sh`, for hosts without those libraries. |

## Frontend

```bash
pnpm install                                   # from the repo root (pnpm workspace)
pnpm --filter @scull/desktop dev               # Vite on :1420, mock bridge in a plain browser
pnpm --filter @scull/desktop lint              # eslint + vue-tsc
pnpm --filter @scull/desktop test              # vitest
pnpm --filter @scull/desktop build             # type-check + bundle to dist/
pnpm --filter @scull/desktop tokens            # regenerate src/styles/theme.css after editing tokens.json
```

`SCULL_DEV_PORT=5291 pnpm --filter @scull/desktop dev` picks another port.

The fonts are bundled from `@fontsource/dm-sans` and `@fontsource/dm-mono`, so the app makes no
network requests at runtime. The font files are OFL-1.1 and the packages are MIT. Every other
dependency is MIT or Apache-2.0.

Byte counts use the CLI's rules (binary units, `41.2 GiB`), so the same number reads the same in
both places. An unknown size is shown as "size unknown", never 0.

## Rust side

The bridge crate is part of the normal workspace bar:

```bash
cargo test -p scull-desktop-bridge
```

The app crate needs WebKitGTK on Linux. macOS and Windows build it natively:

```bash
pnpm --filter @scull/desktop build             # Tauri embeds dist/
cargo build --manifest-path apps/desktop/src-tauri/Cargo.toml --locked
cargo fetch --manifest-path apps/desktop/src-tauri/Cargo.toml --locked   # once; the SAFE-10 test runs offline
cargo test  --manifest-path apps/desktop/src-tauri/Cargo.toml --locked
cargo deny  --manifest-path apps/desktop/src-tauri/Cargo.toml check
pnpm --filter @scull/desktop tauri dev         # the app, with Vite hot reload
pnpm --filter @scull/desktop tauri build       # installers for this OS
```

On a Linux host without `libwebkit2gtk-4.1-dev`, use the Docker image. The repo is mounted, and
build output goes to a Docker volume, not the host's `target/`:

```bash
pnpm --filter @scull/desktop build
apps/desktop/docker/cargo.sh build --locked
apps/desktop/docker/cargo.sh fetch --locked      # once; the SAFE-10 test runs offline
apps/desktop/docker/cargo.sh test --locked
apps/desktop/docker/cargo.sh clippy --locked --all-targets -- -D warnings
```

`cargo.sh` builds the image on first use (`docker build -t scull-desktop-build:1 apps/desktop/docker`).

## Safety checks

- `bridge/tests/safe10.rs` runs in the main workspace on every OS. It walks the manifests of the
  bridge and the app crate through every `scull-*` dependency and fails on `scull-cleaner` or
  `scull-platform/delete`. It also fails if desktop Rust sources use a filesystem-mutation API
  (`remove_*`, `rename`, `fs::write`, `create_dir`, `OpenOptions`, ...).
- `src-tauri/tests/safe10.rs` checks the resolved graph of the app crate:
  `cargo tree -e features,normal,build --target all` must show no `scull-cleaner`, no `delete`
  feature, and no fs/shell/process/http plugin. It also checks that every capability permission
  is a `core:` one.
- The frontend's `ScullBridge` has no mutating call. A vitest test checks the mock, and eslint
  keeps `@tauri-apps/api` out of everything but `src/bridge/tauri.ts`.

## Licenses

`src-tauri/deny.toml` uses the root policy, plus exceptions for five MPL-2.0 crates that Tauri 2
pulls in. Four of them are used only at build time, by Tauri's codegen; `option-ext` is linked at
runtime. On the npm side, the only non-permissive package is `lightningcss` (MPL-2.0). It is a
build-time dependency of Vite and Tailwind and is not in the shipped bundle. **These exceptions wait on maintainer decision #210, and the desktop is not released
until it is answered.**
