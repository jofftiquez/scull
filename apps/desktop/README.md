# scull desktop

Tauri 2 + Vue 3 app. The design reference is `design/html/index.html`. The tokens come from
`design/tokens.json`, and the layout follows D-16…D-21: a full-height left sidebar, ink +
magenta duotone, no shadows, DM Sans and DM Mono.

**The desktop deletes only through the core Executor (SAFE-10).** On `phase2` the app cleans
(P3-EXEC-01): Cleanup Review, Quick Clean and Restore call Tauri commands that run reviewed
plans through `scull_cleaner::Executor`, the same engine as `scull clean`: the execution lock,
revalidation of every item right before it is touched, D-28 actions (delete S0 only, quarantine
or Trash S1/S2, never S3), the refusal to run elevated, and the audit (method `desktop`). Only
`bridge/src/cleanup.rs` uses `scull-cleaner`, and only through the Executor's API; no desktop
source calls a filesystem API itself; the commands are granted to the main window only (not the
menu bar widget) and are never reachable from MCP. Its windows have no filesystem, shell or
process permissions. Tests enforce all of it (see [Safety checks](#safety-checks)).

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

### Insights screen

Insights (`#/insights`, design 05) reads the four history commands for the chosen range (7, 30
or 90 days), plus six months of rollups for the mix, the last scan's worktrees and `status()`.
`src/components/insights/model.ts` derives every figure from those samples:

- Cleanups are rises in free space of at least 1 GiB.
- The forecast (tile, dashed line and band) is the core's, from the `forecasts` command: the
  same days and confidence as `scull check`, the digest and the menu bar (`docs/FORECAST.md`).
- "Driven by" is the category that grew most.
- Regrowth counts rebuilds per path, not restores.

The charts follow D-37: `d3-scale` and `d3-shape` compute the geometry, Vue renders the SVG,
each chart has a hidden data table, and arrow keys move along the forecast.

Duplication and cleanup accuracy have no data source yet, so they show an empty state. With the
mock bridge, `#/insights?demo=design` fills them with the design's illustrative values.
"Stop suggesting it" and "Export" go through `src/components/insights/actions.ts`
(`neverSuggest`, `exportHistory`). Inside Tauri these stubs reject until the commands exist.
The mock's history (`src/bridge/fixtures/*-history.json` and `regrowth.json`) is recorded by
`bridge/tests/insights.rs` from a real history database. The mock shifts it so the last sample
is now.

### Menu bar widget

The tray icon (macOS menu bar, Windows notification area, Linux app indicator) toggles a
380 × 660 widget window (design 11, D-23). The widget is its own page, `menubar.html` →
`src/menubar.ts` → `src/screens/MenuBar/MenuBarWidget.vue`, with no router and no shell.
`src/components/menubar/model.ts` derives its figures:

- Free space is read live from `drives()` (the drive holding home). Without one it falls back to
  the latest `/` sample in `status()`.
- The forecast is the core's (`forecasts`), for the drive holding home. It is shown only when
  the core calls it actionable (at least medium confidence), like a status line.
- "Safe to reclaim" is `status().review_plan`, the plan `scull check` prepared (PRO-05). Without
  a plan it falls back to the last scan's S0 bytes.
- Alerts are `status().alerts`: one day of `scull check` alerts, the newest per key, at most 5.

The widget is read-only. Review, the alerts, the gear and "Open scull" call the
`open_main_window` command, which allows only a fixed list of routes. That command shows the main
window and sends it `scull://navigate`. Escape and losing focus hide the widget. "Check now"
re-reads the bridge. Quick Clean is the Overview's `quickCleanPreview` stub: the mock simulates
it, and inside Tauri it is refused. `src-tauri/src/tray.rs` wires the tray. Where the widget
opens and which routes it may show are decided in `bridge/src/menubar.rs`, which has its own
tests.

Linux app indicators report no clicks, so on Linux the widget opens from the tray menu ("Show
scull widget"). The tray needs `libayatana-appindicator3` at run time. Without it the app
starts with no tray icon. The widget window is only created the first time it opens, and closing
the main window closes the widget too, so nothing stays running in the background.

Screenshots: in a browser, `menubar.html?demo=design` shows design 11's machine. On Linux, a
debug build started with `SCULL_DEV_OPEN_WIDGET=1` opens the widget at launch, so you can
capture it without a tray host.

### Tray monitoring and the weekly digest

While the app runs, a background thread in `src-tauri/src/monitor.rs` checks for trouble (PRD
§14.6, INS-04). Each check reads live free space, the free-space history, and the alerts
`scull check` recorded in the last day. It then decides which OS notification to show, through
Tauri's notification plugin. The plugin is used from Rust only: no window gets the notification
permission. A check never scans, never writes the history database, and never runs a cleanup.
A low-space notification leads to Cleanup Review. The thread sleeps between checks, and it stops
when the app quits. No daemon is installed: `scull schedule` covers checks while the app is
closed.

The defaults are quiet. Only warnings and critical alerts notify, at most once a day per alert
unless one gets worse. Several alerts make one notification. The weekly digest is off until
the user turns it on. When on, it goes out every Monday at 09:00 local time, and turning it on
never sends one at once. Settings → Alerts → "Menu bar monitoring" sets how often the app looks
and how long an alert stays quiet. It can also pause alerts and send this week's digest now. The
tray menu has "Check now" and "Pause alerts for 1 hour".

The decisions live in `bridge/src/monitor.rs` (`Monitor`) and in `scull_proactive::monitor`,
tested with a fake clock and a temp database. The digest text comes from
`scull_insights::digest`. The main window keeps the settings and the monitor's memory in its
local storage (`src/monitor/prefs.ts`). It sends them with `configure_monitor` at start and
keeps the `scull://monitor` snapshots, so a restart doesn't repeat notifications. In a browser,
`src/monitor/mock.ts` stands in for the monitor and shows no notifications. The Insights page
shows the same digest as a "This week" card.

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
| `freeSpaceHistory(since, volumeId?)` | `free_space_history` | `FreeSpaceHistory`: one volume's free-space samples (Insights forecast) |
| `rollupHistory(since)` | `rollup_history` | `RollupHistory`: bytes per category per usable scan (growth, mix) |
| `regrowth(since)` | `regrowth` | `RegrowthHistory`: artifacts that came back after they had gone |
| `footprintHistory(since)` | `footprint_history` | `FootprintHistory`: scull's own disk use ("What scull knows") |
| `forecasts()` | `forecasts` | `ForecastsView`: every sampled volume's disk-full forecast, the one source of days left (`src/lib/forecast.ts`, `docs/FORECAST.md`) |
| `weeklyDigest()` | `weekly_digest` | `DigestView`: the last 7 days in a few lines (Insights "This week", the digest notification) |
| `monitorStatus()` | `monitor_status` | `MonitorStatus`: tray monitoring's settings, pause, last and next check and digest |
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
  database as the CLI's. `status` and the four history reads never create the database; a
  missing one reads `no_database`.

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

## Plan bridge (Cleanup Review, Git Worktrees)

The screens talk to a separate `PlanBridge` (`src/bridge/plan.ts`), which keeps `ScullBridge`
read-only. The desktop's **own** plans run through the core Executor (P3-EXEC-01, `phase2`;
`bridge/src/cleanup.rs`); Quick Clean (`quick_clean`) and History's Restore
(`restore_quarantined`) likewise. In the browser build the mock answers instead and touches
nothing. **AI agent plans** (MCP, PRD §13.3) are connected to the core's approval broker
(P4-MCP-02): the human approves, denies or revokes here, and the agent executes an approved
plan itself through `scull mcp serve`.

| Method | What it does |
|---|---|
| `preparePlan({items?})` | `prepare_desktop_plan`: a recorded desktop plan (D-25: 1 h) from the app's last complete scan, with the SAFE-09 watermark taken before that scan; S3 and tool/Git actions left out |
| `executePlan({plan_id, hash, items}, {onEvent, signal})` | `execute_desktop_plan`: exactly the items the human confirmed, through the core Executor (lock, revalidation, D-28, audit with method `desktop`); refused when elevated |
| `approvalRequests()` | Agent plans waiting for the human (`approval_requests`): the notification |
| `approvalRequest(planId)` | Open one (`open_approval_request`): the plan exactly as recorded and a fresh confirmation code **for display** |
| `approvePlan` / `denyPlan` / `revokeApproval` | Record the human's decision (`approve_agent_plan` with the reviewed hash and the typed code, `deny_agent_plan`, `revoke_agent_approval`) |
| `watchApproval(planId, onEvent)` | Polls `agent_plan_status`: approved/denied/revoked elsewhere, agent started, per-item outcomes, finished, voided (D-34), expired |
| `onOpenRequest(onOpen)` | `scull://approve/<plan-id>` links (`take_deep_link`, `scull://open-request`) |

Inside Tauri `usePlanBridge()` returns `createTauriPlanBridge` (`src/bridge/tauriPlans.ts`;
backend: `apps/desktop/bridge/src/approvals.rs`, commands in `src-tauri/src/lib.rs`). What it
guarantees:

- The broker's challenge (plan as shown, confirmation code) lives in the app's backend memory.
  The webview receives the code only to display it; the backend compares the typed code, checks
  the hash the human reviewed and any denial recorded meanwhile, and records the approval
  through the desktop channel. Opening a request again draws a new code. The code is never
  written, logged or sent to the agent. Three wrong codes close the request: that is UX and
  audit, not a security control (the code is on the same screen).
- Only the main window's own page can call the approval commands: `build.rs` puts every
  command under the ACL, `capabilities/default.json` grants them to `main`, and
  `capabilities/menubar.json` gives the menu bar widget everything else. A remote origin is
  refused (`src-tauri/tests/approval_commands.rs`, on the app's real capabilities).
- Web view debugging can't be turned on from outside: `run` first clears every variable that
  enables remote debugging, an inspector server or extra browser arguments
  (`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`, `WEBKIT_INSPECTOR_SERVER`, ...; `BROWSER_ENV`), the
  browser arguments are set explicitly, and no build enables Tauri's `devtools`. What remains is
  the same-user desktop class (driving the GUI, injecting into the process), out of scope like
  an agent with unrestricted shell access (PRD §13.4).
- A link only opens Cleanup Review on the request. On Windows and Linux it starts the app (a
  second window if one is already open; `tauri-plugin-single-instance` is left out because its
  D-Bus crates fail cargo-deny's advisories). Packaging registers the `scull` scheme.
- The core's plans don't record which MCP client asked, so the dialog says "the agent" and shows
  no process line. Items the app's last scan found unchanged show its evidence and flags; the
  others show only what the plan recorded (flags unknown, never "no").

Next to the mock core bridge, `usePlanBridge()` returns the mock (`src/bridge/planMock.ts`: the
design's illustrative plans, timers instead of execution, nothing on disk). Its agent request is
announced to the notification only with `?agent=request` (so the card doesn't cover other
screens in development and E2E); opening it by ID always works. In development the mock is also
reachable as `globalThis.__scullMockPlans` (e.g. `simulateTargetChanged(planId)` for the "plan
changed" state).

Cleanup Review has two modes: `/cleanup` (the user's own plan, design 03) and
`/cleanup?request=<plan-id>` (agent request, design 12; the notification and
`scull://approve/<plan-id>` open it). Selection rules (D-28: S0 preselected, S1/S2 opt-in, S2
only quarantine/Trash, S3 never selectable) are in `src/components/review/plan.ts`; the approval
state machine in `src/components/review/approval.ts`. Both are unit-tested.

Git Worktrees removes a Removable worktree (`git worktree remove`, branch kept) through a
one-item plan: `preparePlan({items: [candidate]})` then `executePlan`. Verdicts are the core's
tiers; removal is offered only for linked worktrees inside the scan roots whose tier allows a
Git action (D-28: S0/S1). In the browser the mock core bridge adds the design's six
illustrative worktrees (`src/bridge/worktreesMock.ts`, real `WorktreeReport` shape, validated
by the parity test) to the recorded ones, like `MOCK_DRIVES`.

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

## End-to-end tests (Playwright)

`e2e/` runs the key flows in Chromium against the browser build with the mock bridge:

| Spec | Flow |
|---|---|
| `onboarding.spec.ts` | First run → onboarding → first scan → overview |
| `cleanup.spec.ts` | Overview tier filter and selection → add to cleanup → Cleanup Review → confirm → done |
| `agent-approval.spec.ts` | Agent request: typed code (wrong, then right → approved → executed), expired, denied, plan changed (D-34: voided) |
| `worktrees.spec.ts` | Remove a Removable worktree, branch kept; a protected one is blocked |
| `caches.spec.ts` | Quick Clean with an orphaned cache confirmed ("Include it") and another kept |
| `history.spec.ts` | Restore a quarantined item beside the occupied original (`.scull-restored-1`) |
| `settings.spec.ts` | Change a setting → Saved pill; destructive MCP is locked |
| `keyboard.spec.ts` | Skip link → wheel (arrows, Enter, Shift+Enter) → Cleanup Review → confirm, keyboard only |
| `reduced-motion.spec.ts` | `prefers-reduced-motion: reduce`: end states with no animation running |

```bash
pnpm install --frozen-lockfile                  # once, from the repo root
apps/desktop/e2e/run.sh                         # build, serve and run everything in the container
apps/desktop/e2e/run.sh --dev                   # the Vite dev server instead of a build
apps/desktop/e2e/run.sh --shots .evidence/107   # also copy the flows' screenshots there
apps/desktop/e2e/run.sh --repeat 3              # each test 3 times (flakiness check)
apps/desktop/e2e/run.sh -- cleanup --headed     # anything after -- goes to `playwright test`
```

`run.sh` starts the official `mcr.microsoft.com/playwright:v1.64.1-noble` image, which brings its
own browsers, and runs `e2e/in-container.sh` there. The CI job
(`.github/workflows/desktop-e2e.yml`, Linux only) runs the same script inside that image. The
script builds with `VITE_SCULL_E2E=1` into `e2e/.dist` (`dist/` is untouched), serves it with
`vite preview` and runs `playwright test`. The flag only exposes the mock plan bridge as
`globalThis.__scullMockPlans`, as the dev server already does, so the "plan changed" test can
simulate a target changing. A normal build drops that code. The HTML report goes to
`e2e/report/`, failure traces to `e2e/results/` and screenshots to `e2e/shots/`. All three are
gitignored, and screenshots are attached to issues, never committed.

`@playwright/test` is a devDependency pinned to the image's version (1.64.1). It is Apache-2.0,
has no dependencies besides `playwright` and `playwright-core`, and downloads no browsers when
installed. Pinning it in the lockfile gives the specs types and lint, and lets CI install it with
everything else. `run.sh` refuses to start if the version and the image drift apart.

Rules for the specs:

- **Wait on conditions, never sleep.** Use locators, `expect` polling,
  `fixtures.settle()` (finite animations done) and `page.clock` for time (the expiry test
  fast-forwards 15 minutes). Retries are off, so a flaky wait fails the run.
- **Safety (PRD §10).** `open()` asserts the page isn't inside Tauri. The auto `guard`
  fixture aborts and fails on any request outside the app's origin, and on page or console
  errors. The mock bridges only animate, so nothing on disk can change.
- `?scenario=first-run` makes the mock bridge a never-scanned machine (`state: "no_scan"` until
  the first complete scan), so the real first-run redirect to onboarding is exercised.
  `?scenario=design` / `?demo=design` load the design's illustrative data, as for screenshots.

## Licenses

`src-tauri/deny.toml` uses the root policy, plus exceptions for five MPL-2.0 crates that Tauri 2
pulls in. Four of them are used only at build time, by Tauri's codegen; `option-ext` is linked at
runtime. On the npm side, the only non-permissive package is `lightningcss` (MPL-2.0). It is a
build-time dependency of Vite and Tailwind and is not in the shipped bundle.

**These exceptions wait on maintainer decision #210, and the desktop is not released until it
is answered.**
