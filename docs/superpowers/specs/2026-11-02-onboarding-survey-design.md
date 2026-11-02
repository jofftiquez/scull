# Onboarding survey design

## Intent and agreed flow

Replace the first-run dashboard of question tiles with a guided survey. The user approved these six steps on 2026-11-02:

1. What do you build?
2. Where do your projects live?
3. Do you use coding agents?
4. What would you like scull to help with?
5. How should suggestions and reminders behave?
6. Review your choices, then start the first scan.

The first useful outcome is a completed scan of folders the user deliberately selected. The native macOS folder chooser replaces mandatory path typing.

## Presentation and interaction

- Use the existing fonts, colors, buttons, spacing tokens, and full-window onboarding route without a sidebar.
- Present a single, roughly 640-pixel-wide content column. Show `Step N of 6`, one question, a short explanation, the answer controls, and Back/Continue navigation. Avoid an outer card and grids of decorative cards.
- Preserve answers when going back. Move focus to the new question heading and announce the current step. Use checkboxes for multiple selections and radio groups for exclusive choices.
- Keep suggested answers editable; do not describe a prefilled answer as a completed user response. Detection failure must leave the survey usable.
- Step 1 permits multiple build types; require one choice, including Something else. Steps 3–5 have explicit defaults. Step 2 requires at least one selected, confirmed folder before continuing.
- Keep a quiet `Set up later` action available before scanning. It exits without starting a scan or silently committing a draft; it does not mark onboarding complete.
- Review uses a plain labeled list with Edit actions returning to the corresponding step. Only the review step offers `Start first scan`.
- Use short state transitions and respect reduced-motion preferences. Keep actions and long paths usable at the app's 960-pixel minimum window width.

## Folder selection

- `Choose folders…` opens the operating system's directory chooser with multiple selection. Paths are selected without enumerating file contents in the webview.
- Display folder names and readable absolute/shortened paths in a simple list, with clear selection/removal controls. Retain any detected suggestions.
- Cancelling the chooser leaves the list unchanged. Prevent duplicate rows, disable repeated opens while a dialog is pending, and show errors inline with a retry action.
- Use the actual home directory from the native session for broad-root checks, not a guess from cache paths.
- Home, filesystem roots, and ancestors of home are never selected by default. Preserve explicit broad-root confirmation for both picked and suggested folders, including several broad folders in one selection.
- Native selection is the primary desktop interaction. The browser mock uses an injected deterministic picker; it must not pretend a browser can provide usable absolute native scan roots.

## Persistence and honest copy: proposed implementation boundary

The current desktop `saveProfile` is a stub. The following is the proposed smallest scope, not a claim that all existing personalization features already work:

- Keep a validated, versioned app-local record of the survey answers and selected roots, using the existing local-storage approach used by monitor preferences. Save only when the user starts the scan.
- Carry separately confirmed broad roots in that record so they are not silently inferred from a stored path. Validate again when loading and before scanning.
- First scan and later desktop rescans use the saved selected roots. When no saved desktop setup exists, retain the existing core configuration behavior.
- Restore saved answers when onboarding is deliberately reopened. Never let late detection replace an answer the user has already changed.
- Other answers are remembered preferences. Do not claim they changed category order, cleanup selection, idle windows, notification schedules, or client connections unless a working consumer is added in a separately agreed change.
- Remove automatic agent connection controls from this survey; step 3 records usage only. Never claim a client is connected based on detection.
- If local storage fails, explain that choices cannot be remembered and offer an explicit one-time scan or retry. Avoid claiming successful saving.
- Do not write `config.toml`, `profile.toml`, or agent configuration in this change. App-local setup does not sync with command-line setup; a shared core setup writer remains follow-up work.

## Scan and completion

Reuse the existing scan operation, cancellation, error conversion, and result model. After review, show a dedicated scanning state with live entries/activity and Stop. Use indeterminate progress until the engine supplies a real total; do not invent a completion percentage. Distinguish success, cancellation with partial results, and failure. Retry preserves folders and answers. Only a complete successful scan suppresses automatic first-run onboarding on later launches, as today.

## Approaches considered

- **Six sequential screens — selected:** matches the user's requested survey and makes the next action obvious.
- **One scrolling form with stronger hierarchy:** less navigation, but still presents all questions together and does not meet the requested interaction.
- **A shortened two-question setup:** reaches scanning faster, but the user chose to retain all five questions and add review.

## Safety and scope

No scan begins during detection, folder picking, or navigation. Selection never initiates cleanup. Keep existing backend root validation and cleanup safety checks. Add only a directory-selection command and a home-directory context command, available to the main window's own page. Keep generic filesystem, process, save-dialog, and remote-origin permissions unavailable.

This work does not redesign the overview or Settings, install agents, schedule background jobs, or build a general configuration editor. Existing completed scans and cleanup history are preserved. Implementation must be isolated through the user's issue-worktree/herdr workflow and must not interrupt their running scan.

## Technical references

The official [Tauri dialog plugin](https://v2.tauri.app/plugin/dialog/) uses `rfd`, but also depends on a filesystem plugin forbidden by this project's native safety guard. Implementation therefore uses [rfd's native directory dialog](https://docs.rs/rfd/0.17.2/rfd/struct.FileDialog.html) directly behind the restricted app command. It supports multiple folders, disabling directory creation on macOS, and attaching a parent window. Blocking picker calls stay off the main event thread. No filesystem-plugin guard was removed.
