# Onboarding Survey Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans to implement this plan task by task in the current session. Delegation requires a separately chosen execution method. Steps use checkbox syntax for tracking.

**Goal:** Replace onboarding's question grid with the approved six-step survey and a working native macOS folder chooser.

**Architecture:** Keep the existing onboarding route, answer model, and scan bridge. Add two narrowly scoped native onboarding commands and a validated app-local setup record. Reuse existing components where they fit; do not introduce a wizard framework or a core configuration writer.

**Tech Stack:** Vue 3, TypeScript, Tauri 2, Rust, Vitest, Playwright, and Rust's `rfd` native directory dialog library.

**Spec:** [Onboarding survey design](../specs/2026-11-02-onboarding-survey-design.md). The user approved the six-step flow and app-local persistence boundary before implementation.

## Global constraints

- Keep the existing fonts, colors, buttons, spacing tokens, and full-window onboarding route without a sidebar.
- Home, filesystem roots, and ancestors of home are never selected by default.
- No scan begins during detection, folder picking, or navigation. Selection never initiates cleanup.
- Do not write `config.toml`, `profile.toml`, or agent configuration in this change.
- App-local setup does not sync with command-line setup; a shared core setup writer remains follow-up work.
- Other answers are remembered preferences; do not promise unimplemented personalization.
- Keep generic filesystem, process, save-dialog, and remote-origin permissions unavailable.
- Implementation must be isolated through the user's issue-worktree/herdr workflow and must not interrupt their running scan.

## Review focus

1. Several broad directories selected together must each receive explicit confirmation (Task 2).
2. A saved directory may disappear or lose permission; show the backend error and allow editing/retry (Tasks 3–4).
3. Missing, malformed, or unavailable local storage must not enable an unsafe root or falsely report saving (Task 1).
4. Late detection or dialog completion must not reset manual answers or silently advance the survey (Tasks 2–3).
5. A running scan must not be started twice or changed by navigation; cancellation remains a partial outcome (Tasks 3–4).

## File map

- `src/components/onboarding/model.ts`: existing answer/root helpers plus six-step navigation rules and honest summary text.
- New `src/components/onboarding/prefs.ts` and `prefs.test.ts`: local setup validation, load/save, and scan-root selection.
- `src/components/onboarding/bridge.ts`: retain detection; replace pending save behavior with local setup persistence and inject native folder/context operations.
- `src/bridge/tauri.ts`: `ONBOARDING_COMMANDS` and injectable transport wrapper; keep Tauri imports here.
- New `src-tauri/src/onboarding.rs`: native context and directory-dialog commands; no config writes.
- `src-tauri/src/lib.rs`, `build.rs`, `Cargo.toml`, `Cargo.lock`, and `capabilities/default.json`: register dialog support and main-window-only commands.
- `bridge/tests/commands.rs`, `bridge/tests/safe10.rs`, and `src-tauri/tests/safe10.rs`: command parity and the exact permitted native dependency; preserve safety assertions.
- `src/screens/Onboarding/OnboardingScreen.vue`, `ProjectFolders.vue`, and `SetupSummary.vue`: sequential survey, native selection, review, scan, and completion presentation.
- `QuestionTile.vue`, `ChoiceCard.vue`, and `FoundOnMachine.vue`: check all callers; remove only components made unused or adapt existing choice controls to proper form semantics.
- `src/state/core.ts` and `core.test.ts`: rescan uses saved desktop roots when present.
- `src/components/onboarding/onboarding.test.ts`, `src/bridge/tauri.test.ts`, new `src-tauri/tests/onboarding_commands.rs`, and `e2e/onboarding.spec.ts`: behavioral checks.
- `apps/desktop/README.md`: update onboarding behavior and document the local-only persistence boundary.

Paths above are relative to `apps/desktop/`.

## Task 1: Define survey state and app-local persistence

**Interfaces:** Retain `Answers`, `Detection`, `Profile`, `checkRoot`, and `scanRoots`. Add `OnboardingStep = 1 | 2 | 3 | 4 | 5 | 6` and `canContinue(step, answers, home): boolean` in `model.ts`. In `prefs.ts`, export `SavedSetup = { version: 1; answers: Answers; home: string }`, `loadSetup(store?): SavedSetup | null`, `saveSetup(setup, store?): { saved: boolean; reason?: string }`, and `savedScanParams(setup): ScanParams | null`. Use the key `scull.onboarding.setup`; keep browser mock/test storage separate from native state through injected stores.

- [ ] Add failing tests: one build type required on step 1; no selected/confirmed folder blocks step 2; steps 3–5 accept defaults; six is review. Validate record version, enum values, booleans, absolute paths, and confirmation paths. Confirm malformed storage produces `null`, storage exceptions produce visible save failure, and an unconfirmed stored broad root never reaches scan parameters.
- [ ] Run `pnpm --filter @scull/desktop exec vitest run src/components/onboarding/prefs.test.ts src/components/onboarding/onboarding.test.ts`; verify new assertions fail for missing behavior.
- [ ] Implement the interfaces using existing root helpers and the monitor preference storage pattern. Validate data as unknown at the storage boundary. Preserve only confirmations for selected broad paths. Never silently replace a corrupt record with a home-folder default.
- [ ] Update summary labels to describe selected folders and recorded preferences; remove claims about connected clients, scheduled checks, idle thresholds, automatic cleanup, and unapplied category order. Keep stored `connectAgents` false.
- [ ] Rerun the targeted tests; require all pass.

## Task 2: Add native folder selection and reliable home context

**Interfaces:** Export `ONBOARDING_COMMANDS = { context: 'onboarding_context', pickFolders: 'pick_project_folders' }` and `createTauriOnboardingCommands(transport = realTransport)` in `tauri.ts`. Return `{ context(): Promise<{ home: string | null }>; pickFolders(): Promise<string[] | null> }`. Mirror these operations in the screen-local onboarding bridge with injected deterministic browser mock results. Rust commands return corresponding serialized values or `ScullError`.

- [ ] Add failing transport tests for exact command names, cancellation returning `null`, multiple absolute paths, and error propagation. Add native capability tests that permit the main window's local page and refuse the menu bar widget and remote origins.
- [ ] Add component tests for cancellation leaving selections untouched, duplicate selection producing one row, repeated opens being disabled, picker rejection showing an error, and several broad selections being confirmed one by one. Declining one broad folder must not discard unrelated safe choices. Unmounting during a pending picker must not trigger navigation or a scan.
- [ ] Add `rfd = "0.17.2"` to the app crate; preserve the ban on filesystem plugins (see the design’s implementation ruling). Implement both commands in `src-tauri/src/onboarding.rs`, with context taken from `Session::setup().home`. Parent the directory chooser to the main window; select folders only, disable new-directory creation on macOS, and run blocking selection through the existing blocking-pool helper. Convert supported native file paths losslessly; reject unsupported paths rather than converting them to misleading text.
- [ ] Register both commands in the app manifest/handler and grant only their app permissions to the main window. Grant no generic open/save permissions. Include the same commands in the testable main-window handler.
- [ ] Update command-parity tests for `ONBOARDING_COMMANDS` and the exact dialog dependency allowlist; retain all filesystem/process and origin restrictions. Generate permission files through the normal Tauri build and retain repository conventions for tracked generated files.
- [ ] Replace the default path-entry form in `ProjectFolders.vue` with `Choose folders…`, selected rows, and a queue of broad-folder confirmations. Keep existing backend validation authoritative.
- [ ] Run focused frontend tests, `cargo test -p scull-desktop-bridge --test commands --test safe10`, and `cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml --locked --test onboarding_commands --test safe10`; require all pass. After adding the dependency, fetch the app lockfile's dependencies before offline graph checks.

## Task 3: Render the six-step survey and integrate scanning

**Interfaces:** `OnboardingScreen.vue` owns one reactive `Answers` instance and the current `OnboardingStep`. `ProjectFolders.vue` receives injected `pickFolders` rather than importing native APIs. `prefs.ts` remains the only survey-storage implementation; `core.rescan` consumes `savedScanParams(loadSetup())` only when valid setup is present, otherwise preserves its existing `{}` request.

- [ ] Replace current tile-based screen tests with assertions for exactly one question at a time, Step N of 6, Back/Continue, saved answers, and Edit links from review. Confirm single-choice controls expose radio semantics and multiple-choice controls expose checkbox semantics.
- [ ] Add failing tests proving detection does not overwrite user edits, no transition triggers `bridge.scan`, and only review's `Start first scan` submits the selected folders. Double submission produces one call. `Set up later` exits without scanning or saving a draft.
- [ ] Build the single-column layout with existing tokens. Use one stable footer action area and heading focus after step changes. Keep detection as contextual information rather than a permanent sidebar of tiles. Do not display guessed machine findings when detection fails.
- [ ] Load saved setup before applying detected defaults; merge only untouched fields. Fetch actual native home context independently of projects/caches. If home context cannot be obtained, show retry and block native folder confirmation/scanning rather than weakening home-root checks.
- [ ] Save locally on review submission. On failure keep answers visible, offer retry and explicit one-time scan, and never show saved-success copy. Remove the agent-connect checkbox and pending command implementation message; step 3 records usage only.
- [ ] Refactor `SetupSummary.vue` into a plain review list and scan/result presentation. Use an indeterminate running indicator and real activity/counts; remove synthetic completion percentages and celebratory bounce. Keep Stop, failure retry, partial-result labeling, and Open overview. Disable setup navigation while scanning.
- [ ] Use saved roots for later desktop rescans without changing core configuration or broad-root validation. Test saved roots, no-record fallback, a selected directory becoming unavailable, and cancellation preserving the last complete report.
- [ ] Run `pnpm --filter @scull/desktop exec vitest run src/components/onboarding src/bridge/tauri.test.ts src/state/core.test.ts`; require all pass. Remove unused old tile components/helpers only after checking their callers.

## Task 4: Verify the complete flow and document limits

- [ ] Update `e2e/onboarding.spec.ts` to traverse all six steps, go back, edit from review, use an injected picker result, scan, and open Overview. Add blocked-empty-folder, cancelled-picker, broad-root decline, scan failure/retry, and returning-user cases. Keep the existing no-auto-scan assertion.
- [ ] Run `pnpm --filter @scull/desktop lint`, `pnpm --filter @scull/desktop test`, and `pnpm --filter @scull/desktop build`; require successful exits. Run `apps/desktop/e2e/run.sh -- onboarding` when its container runtime is available; if unavailable, report that concrete limit and run the same onboarding spec against the local mock server.
- [ ] Run the native app tests from Task 2 and `cargo build --manifest-path apps/desktop/src-tauri/Cargo.toml --locked`; require successful exits after the final changes.
- [ ] Verify the isolated macOS app manually: actual directory dialog, multi-select/cancel/reopen, home-directory warning, long paths, keyboard-only navigation, focus order, reduced motion, minimum window size, save/relaunch, rescan roots, unavailable folder, cancellation, and complete scan. Browser tests do not count as native dialog verification.
- [ ] Update desktop onboarding documentation with the six steps and the explicit boundary: saved app-local preferences, actual selected scan roots, no agent registration/config writes or promised scheduling changes. Preserve existing scans/history; do not reset the user's active installation to demonstrate onboarding.
- [ ] Review the resulting diff, summarize verified behavior and remaining limits, and ship only when requested through `/ship-fix`. Do not manually create a pull request or commit/push outside the user's issue lifecycle workflow.

## Implementation status

Implemented in the herdr-managed worktree `feat/133-onboarding-survey` for issue #133. Shipping and merge are tracked in the associated pull request. No deployment is part of this change. The original running app and scan data have not been changed.

Verification completed: full desktop unit suite, all 36 browser flow tests including reduced motion, desktop lint/type-check and frontend build, all 18 native app tests, bridge command parity plus all 12 bridge safety checks, and packaged macOS application build. Independent code and rendered-screen review found no remaining issues after fixing suggestion removal state and the brand-link target size.

Live native checks completed against isolated data: real directory chooser opens, cancellation preserves selections, home requires confirmation and remains unchecked when declined, two-folder selection works, a real fixture scan finds two projects, rescanning uses the saved folders, and relaunch goes to Overview with the same folders. Windows/Linux hardware and VoiceOver were not exercised.

Implementation rulings: use `rfd` directly because the dialog plugin pulls in the forbidden filesystem plugin; retain app-local preferences without adding a shared core configuration writer; inject picker failures at the component boundary instead of adding production-global browser test hooks. Local browser checks used the package-matched Chromium; the optional container image was not downloaded.
