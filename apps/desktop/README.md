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
| `src/bridge/` | `ScullBridge`, the only way screens reach the core. `tauri.ts` calls Tauri commands (it is the only module allowed to import `@tauri-apps/api`). `mock.ts` replays recorded real payloads (`fixtures/`). `index.ts` picks Tauri when `window.__TAURI_INTERNALS__`/`__TAURI__` exists and the mock otherwise. |
| `src/styles/theme.css` | The Tailwind v4 theme, **generated** from `design/tokens.json` by `scripts/gen-theme.mjs`. Never edit it by hand. Tailwind's default palette and every shadow utility are removed, so only token classes exist. |
| `bridge/` | `scull-desktop-bridge`: all the Rust logic behind the commands. It is a member of the root Cargo workspace, so it builds and is tested on every OS without WebKit. |
| `src-tauri/` | `scull-desktop`: the Tauri app crate, thin `#[tauri::command]` wrappers over the bridge. It is a **separate Cargo workspace**, because on Linux it links WebKitGTK and the root workspace's builds and CI must not need it. It has its own `Cargo.lock` and `deny.toml`. |
| `docker/` | Linux build image (WebKitGTK and friends) and `cargo.sh`, for hosts without those libraries. |

## Frontend

```bash
pnpm install                                   # from the repo root (pnpm workspace)
pnpm --filter @scull/desktop dev               # Vite on :1420, mock bridge in a plain browser
pnpm --filter @scull/desktop lint              # eslint + vue-tsc + generated files up to date
pnpm --filter @scull/desktop test              # vitest
pnpm --filter @scull/desktop build             # type-check + bundle to dist/
pnpm --filter @scull/desktop tokens            # regenerate src/styles/theme.css after editing tokens.json
pnpm --filter @scull/desktop types             # regenerate src/bridge/generated.ts from schemas/
```

`SCULL_DEV_PORT=5291 pnpm --filter @scull/desktop dev` picks another port.

The fonts are bundled from `@fontsource/dm-sans` and `@fontsource/dm-mono`, so the app makes no
network requests at runtime. The font files are OFL-1.1 and the packages are MIT. Every other
dependency is MIT or Apache-2.0.

Byte counts use the CLI's rules (binary units, `41.2 GiB`), so the same number reads the same in
both places. An unknown size is shown as "size unknown", never 0.

### Projects screen

Projects (`#/projects`) reads `projects()`: the last complete scan. Its actions don't exist in
the core bridge yet, so they go through `src/components/projects/bridge.ts` (`ProjectActions`),
which is kept out of `ScullBridge`:

- `cleanRegenerable(root, bytes)`: will become a plan for the project's S0 candidates that the
  core Executor runs (planned commands `plan_project_clean` and `execute_plan`).
- `neverSuggest(root)` and `reveal(root)` (Show in Finder/Explorer/Files).

The mock only animates and touches nothing. Inside Tauri these calls reject until the commands
exist. The recorded fixture has no S0 bytes (in-use probes aren't available in that build), so
with the mock bridge `#/projects?demo=design` loads the design's eight projects to show the
clean and its animation.

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

Screens call `useBridge()`, which returns a `ScullBridge`. In Tauri every method is one command
of `src-tauri/src/lib.rs`, and each command is a thin wrapper over `scull-desktop-bridge`, which
calls the read-only `scull-engine` pipeline (D-29, the same one the CLI and MCP server use).

| Method | Command | Returns |
|---|---|---|
| `appInfo()` | `app_info` | `AppInfo` |
| `drives()` | `drives` | `Drive[]` (sidebar widget) |
| `scan(params, {onProgress, signal})` | `scan` | `ScanReport` (the CLI's `scull/scan@1` payload) |
| `projects()` / `worktrees()` | `projects` / `worktrees` | the last complete scan, in the shape of `scull projects/worktrees --json` |
| `caches({onProgress, signal})` | `caches` | `CachesReport` (`scull caches`) |
| `status()` | `status` | `DesktopStatus`: last recorded scan and free-space samples |
| `capabilities()` | `capabilities` | `CapabilitiesReport`: signals, Git, elevation, history (doctor-like) |
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
  database as the CLI's. `status` never creates the database.

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
- `bridge/tests/commands.rs` checks that the Rust commands, the registered handler and the
  frontend's `COMMANDS` table name the same commands.

To change a payload:

```bash
SCULL_UPDATE_SCHEMAS=1 cargo test -p scull-desktop-bridge --test schemas
SCULL_UPDATE_MOCKS=1 cargo test -p scull-desktop-bridge --test session   # re-record the mock data (Unix)
pnpm --filter @scull/desktop types
```

## Screen stubs for calls the bridge doesn't have yet

Some screens need calls that the core bridge doesn't offer yet. Each screen declares them in
its own folder as a small interface with two implementations: a mock that answers in a plain
browser (development, tests, screenshots), and a "pending" one used inside Tauri that reports
the call isn't available yet. These stubs stay out of `ScullBridge`, so its command table, its
parity tests and the SAFE-10 name check are unchanged. None of them deletes anything; when the
real commands land they move into `ScullBridge`.

| Screen | Stub | Calls | Planned command |
|---|---|---|---|
| Onboarding | `src/components/onboarding/bridge.ts` | `detect()`: the `scull init` probe (ONB-01). Inside Tauri it is derived from `projects()` and `caches()` for now. | `onboarding_detect` |
| | | `saveProfile(profile)`: writes the profile and `scan.roots` like `scull init` (ONB-03). Inside Tauri it returns `saved: false` with the reason. | `onboarding_save_profile` |

Onboarding opens at `#/onboarding`. The shell sends you there on first open, when `status()`
reports `no_scan` or `no_database`. The route has `meta.bare`, so the sidebar is hidden. Scan
roots follow CFG-04: a filesystem root, the home folder or a folder that contains it is never
ticked by default. Ticking one asks for confirmation first, and only then does the scan send
`allow_broad_roots`.

## Plan bridge stubs (Cleanup Review, Git Worktrees)

Preparing, approving and executing a cleanup plan are **not** core-bridge commands yet:
execution will go through the core Executor behind a reviewed plan, never through the desktop.
Until it is wired, the screens talk to a separate `PlanBridge` (`src/bridge/plan.ts`), which
keeps `ScullBridge` read-only:

| Method | What it will do |
|---|---|
| `preparePlan({items?})` | Build a dry-run plan from the last scan (`PlanPreview`: items as full `Candidate`s, skipped, left-out S3, hash, expiry) |
| `executePlan({plan_id, hash, items}, {onEvent, signal})` | Execute the selected items of the reviewed plan through the Executor; per-item events, measured result |
| `approvalRequest(planId)` | An agent's MCP plan waiting for the human (PRD §13.3) and its confirmation code |
| `approvePlan` / `denyPlan` / `revokeApproval` | Record the human's decision (single use, bound to the hash) |
| `watchApproval(planId, onEvent)` | Broker events: agent started, per-item outcomes, finished, invalidated (D-34) |

`usePlanBridge()` returns the mock (`src/bridge/planMock.ts`: the design's illustrative plans,
timers instead of execution, nothing on disk) next to the mock core bridge, and inside Tauri a
bridge whose every call rejects with `unsupported_platform`, so the screen shows an error rather
than pretending to clean. The payload types are local and provisional; replace them with
generated ones when the core exposes plans. In development the mock is also reachable as
`globalThis.__scullMockPlans` (e.g. `simulateTargetChanged(planId)` for the "plan changed" state).

Cleanup Review has two modes: `/cleanup` (the user's own plan, design 03) and
`/cleanup?request=<plan-id>` (agent request, design 12; the OS notification will deep-link here).
Selection rules (D-28: S0 preselected, S1/S2 opt-in, S2 only quarantine/Trash, S3 never
selectable) are in `src/components/review/plan.ts`; the approval state machine in
`src/components/review/approval.ts`. Both are unit-tested.

## Safety checks

- `bridge/tests/safe10.rs` runs in the main workspace on every OS. It walks the manifests of the
  bridge and the app crate through every `scull-*` dependency and fails on `scull-cleaner` or
  `scull-platform/delete`. It also fails if desktop Rust sources use a filesystem-mutation API
  (`remove_*`, `rename`, `fs::write`, `create_dir`, `OpenOptions`, ...).
- `src-tauri/tests/safe10.rs` checks the resolved graph of the app crate:
  `cargo tree -e features,normal,build --target all` must show no `scull-cleaner`, no `delete`
  feature, and no fs/shell/process/http plugin. It also checks that every capability permission
  is a `core:` one.
- The command surface is read-only: no command name suggests a mutation
  (`bridge/tests/commands.rs`), the frontend's `ScullBridge` has no mutating call (vitest), and
  eslint keeps `@tauri-apps/api` out of everything but `src/bridge/tauri.ts`.
- Every bridge test runs on generated fixture trees and checks invariants I-2 and I-3
  afterwards: the tree and the canary are unchanged.

## Caches screen and the Quick Clean stub

Caches (`#/caches`) reads `caches()`. The Quick Clean preview and run aren't in the core
bridge yet. They go through `src/components/caches/bridge.ts` (`CacheActions`), which is kept
out of `ScullBridge` so the read-only surface stays as it is:

- `plan(report)`: per tool, the resolved trusted binary, the prune and wipe arguments, and the
  skipped tools with scull-cleaner's reasons. Skips include S2/S3 caches (D-28), Windows `.cmd`
  shims that would need a shell (#218), caches in use, and orphaned caches left for manual
  removal. It mirrors `QuickCleanPlan`. Planned command: `quick_clean_plan`.
- `run(preview, choices, onProgress)`: runs the tools one after another, each with its own
  command and no shell. Orphaned caches the user confirms are removed through the Executor
  (D-36, provisional). Planned command: `quick_clean_run`.

The mock only animates, using the design's timing of 650 ms per tool plus 300 ms. Inside
Tauri the preview is built from the report and marked unavailable, so the button explains
that `scull clean caches` works for now. The recorded fixture's only cache (npm) is classified
S2 and therefore skipped. With the mock bridge, `#/caches?demo=design` loads the design's
machine plus the skipped states.

## Licenses

`src-tauri/deny.toml` uses the root policy, plus exceptions for five MPL-2.0 crates that Tauri 2
pulls in. Four of them are used only at build time, by Tauri's codegen; `option-ext` is linked at
runtime. On the npm side, the only non-permissive package is `lightningcss` (MPL-2.0). It is a
build-time dependency of Vite and Tailwind and is not in the shipped bundle.

**These exceptions wait on maintainer decision #210, and the desktop is not released until it
is answered.**
