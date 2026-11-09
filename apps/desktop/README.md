# scull desktop

Tauri 2 + React app (restarted in #144; the earlier Vue app is tagged `desktop-v0`). The tokens
come from `design/tokens.json` (ink + magenta duotone, no shadows, DM Sans and DM Mono).

The MVP is one flow, **pick folders → scan → review → confirm → clean → result**, plus a
read-only **Caches** view. Everything else from `desktop-v0` (Overview, Explorer, Insights,
History, Projects and Worktrees screens, onboarding survey, agent approval, settings) is out of
scope for now. The Tauri shell has 7 commands, one window and no tray, notifications or deep
links. Unused bridge modules are removed next (#144, stage 3).

**The desktop deletes only through the core Executor (SAFE-10).** Cleaning runs reviewed plans
through `scull_cleaner::Executor` (`bridge/src/cleanup.rs`), the same engine as `scull clean`: the
execution lock, revalidation of every item right before it is touched, D-28 actions (delete S0
only, quarantine or Trash S1/S2, never S3), the refusal to run elevated, and the audit. No desktop
source calls a filesystem API itself, and the windows have no filesystem, shell or process
permissions. Tests enforce it (see [Safety checks](#safety-checks)).

## Layout

| Path | What |
|---|---|
| `src/` | React + TypeScript. `App.tsx` is the shell (Clean / Caches), `CleanFlow.tsx` the cleanup flow, `Caches.tsx` the caches view, `ui.tsx` the small primitives (native elements on the tokens). |
| `src/lib/plan.ts` | The selection rules (D-28: S0 preselected, S1/S2 opt-in, S2 only quarantine/Trash, S3 never selectable) and the typed-confirmation count (SAFE-07). Framework-free and unit-tested. |
| `src/bridge/` | `ScullBridge` and `PlanBridge`, the only way the UI reaches the core. `tauri.ts`/`tauriPlans.ts` call Tauri commands (`tauri.ts` is the only module allowed to import `@tauri-apps/api`); `mock.ts`/`planMock.ts` answer in a plain browser (development, tests, screenshots) and touch nothing. `index.ts` picks Tauri when `window.__TAURI_INTERNALS__`/`__TAURI__` exists. |
| `src/styles/theme.css` | The Tailwind v4 theme, **generated** from `design/tokens.json` by `scripts/gen-theme.mjs`. Never edit it by hand. |
| `bridge/` | `scull-desktop-bridge`: the Rust logic behind the commands (root Cargo workspace). |
| `src-tauri/` | `scull-desktop`: the Tauri app crate, thin `#[tauri::command]` wrappers over the bridge. A **separate Cargo workspace** (it links WebKitGTK on Linux), with its own `Cargo.lock` and `deny.toml`. |
| `docker/` | Linux build image (WebKitGTK) and `cargo.sh`, for hosts without those libraries. |

## Frontend

```bash
pnpm install                                   # from the repo root (pnpm workspace)
pnpm --filter @scull/desktop dev               # Vite on :1420, mock bridge in a plain browser
pnpm --filter @scull/desktop lint              # eslint + tsc + generated files up to date
pnpm --filter @scull/desktop test              # vitest
pnpm --filter @scull/desktop build             # type-check + bundle to dist/
pnpm --filter @scull/desktop tokens            # regenerate src/styles/theme.css after editing tokens.json
pnpm --filter @scull/desktop types             # regenerate src/bridge/generated.ts from schemas/
```

The fonts are bundled (`@fontsource`), so the app makes no network requests at runtime. Byte
counts use the CLI's rules (binary units, `41.2 GiB`).

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

## The core bridge

The UI gets the `ScullBridge` from `useBridges()` (`src/bridgeContext.tsx`). In Tauri every method is one command
of `src-tauri/src/lib.rs`, and each command is a thin wrapper over `scull-desktop-bridge`, which
calls the read-only `scull-engine` pipeline (D-29, the same one the CLI and MCP server use).

| Method | Command | Returns |
|---|---|---|
| `scan(params, {onProgress, signal})` | `scan` | `ScanReport` (the CLI's `scull/scan@1` payload) |
| `caches({onProgress, signal})` | `caches` | `CachesReport` (`scull caches`) |
| `settings()` | `settings` | `SettingsView`: effective configuration with origins (the scan roots) |
| (abort) | `cancel_operation` | `bool` |

- **Progress.** Long operations get a fresh operation ID. The core emits `scull://progress`
  events (`ProgressEvent`: `scanning` with entries and bytes, or `analyzing`), throttled to about
  10 per second, and the bridge forwards only the events of its own operation.
- **Cancellation.** Aborting the `AbortSignal` sends `cancel_operation`. The scan stops within
  the scanner's cancellation bound and resolves with `cancelled: true`. A cancel that arrives
  before its operation starts is remembered for 10 s. A cancelled (partial) scan never replaces
  a complete one for Projects and Worktrees.
- **Errors.** Every call rejects with `BridgeError`, whose `.error` is the core's `ScullError`,
  the same JSON as the CLI's `scull/error@1` payload (`kind`, `message`, `path?`, `item_id?`).
- **Config and history.** Configuration loads like the CLI's: `config.toml` in scull's config
  directory, then `SCULL_*` environment variables. Scan roots default to `scan.roots`, and
  broad roots need `allow_broad_roots` (CFG-04). Scans are recorded in the same history
  database as the CLI's.

**Types come from Rust.** `scull-desktop-bridge::schemas::all()` writes `schemas/*.schema.json`,
and `scripts/gen-types.mjs` compiles them into `src/bridge/generated.ts`. Tests keep the chain
honest:

- `bridge/tests/schemas.rs` checks that the committed schemas are current, and that every shared
  definition (`ScanReport`, `Candidate`, `CachesReport`, `ScullError`, ...) is identical to
  `crates/scull-cli/schemas`.
- `bridge/tests/session.rs` checks that the real payloads from a scan of a generated fixture tree
  validate against those schemas.
- `src/bridge/parity.test.ts` checks that `generated.ts` is current, and that the mock's answers
  and recorded fixtures validate against the same schemas (ajv).
- `bridge/tests/commands.rs` checks that the Rust commands, the one registered handler, build.rs's
  app manifest, the main window's capability and the frontend's tables name the same 7 commands.

To change a payload:

```bash
SCULL_UPDATE_SCHEMAS=1 cargo test -p scull-desktop-bridge --test schemas
SCULL_UPDATE_MOCKS=1 cargo test -p scull-desktop-bridge --test session   # re-record the mock data (Unix)
pnpm --filter @scull/desktop types
```

## Plan bridge

`PlanBridge` (`src/bridge/plan.ts`) keeps `ScullBridge` read-only:

| Method | What it does |
|---|---|
| `preparePlan({items?})` | `prepare_desktop_plan`: a recorded desktop plan from the app's last complete scan, with the SAFE-09 watermark taken before that scan; S3 and tool/Git actions left out |
| `executePlan({plan_id, hash, items, typed_count?}, {onEvent})` | `execute_desktop_plan`: exactly the items the human confirmed, through the core Executor; refused when elevated or when the typed count doesn't match |

## Safety checks

- `bridge/tests/safe10.rs` runs in the main workspace on every OS. It walks the manifests of the
  bridge and the app crate through every `scull-*` dependency and fails on `scull-cleaner` or
  `scull-platform/delete`. It also fails if desktop Rust sources use a filesystem-mutation API
  (`remove_*`, `rename`, `fs::write`, `create_dir`, `OpenOptions`, ...).
- `src-tauri/tests/safe10.rs` checks the resolved graph of the app crate:
  `cargo tree -e features,normal,build --target all` must show no `scull-cleaner`, no `delete`
  feature, and no fs/shell/process/http plugin. It also checks that every capability permission
  is a `core:` one.
- `src-tauri/tests/commands.rs` runs the commands through Tauri's IPC on the real capabilities:
  another window and any remote origin are refused before a command runs, and the typed
  confirmation count survives the frontend's invoke payload.
- The command surface is read-only: no command name suggests a mutation
  (`bridge/tests/commands.rs`), the frontend's `ScullBridge` has no mutating call (vitest), and
  eslint keeps `@tauri-apps/api` out of everything but `src/bridge/tauri.ts`.
- Every bridge test runs on generated fixture trees and checks invariants I-2 and I-3
  afterwards: the tree and the canary are unchanged.

## End-to-end tests (Playwright)

`e2e/cleanup.spec.ts` runs the flow in Chromium against the browser build with the mock bridge:
folders → scan → review (S0 preselected, S1/S2 not) → confirm → done, and the caches view.

```bash
apps/desktop/e2e/run.sh                         # build, serve and run in the Playwright container (Linux hosts / CI)
apps/desktop/e2e/run.sh --shots DIR             # also copy the screenshots there
```

On macOS the container can't use the host's `node_modules` (native binaries). Run the same
steps on the host instead:

```bash
cd apps/desktop
node node_modules/@playwright/test/cli.js install chromium      # once
VITE_SCULL_E2E=1 node node_modules/vite/bin/vite.js build --outDir e2e/.dist --emptyOutDir
E2E_SERVER=preview node node_modules/@playwright/test/cli.js test -c e2e/playwright.config.ts
```

Rules: wait on conditions, never sleep (retries are off); the auto `guard` fixture fails the test
on any request outside the app's origin and on page or console errors; the mock bridges touch
nothing on disk. Screenshots are attached to issues, never committed.

## Licenses

`src-tauri/deny.toml` uses the root policy, plus exceptions for five MPL-2.0 crates that Tauri 2
pulls in. Four of them are used only at build time, by Tauri's codegen; `option-ext` is linked at
runtime. On the npm side, the only non-permissive package is `lightningcss` (MPL-2.0). It is a
build-time dependency of Vite and Tailwind and is not in the shipped bundle.

**These exceptions wait on maintainer decision #210, and the desktop is not released until it
is answered.**
