# scull design reference (v0.6)

Ink + magenta duotone · soft bento · rounded · no shadows · DM Sans / DM Mono. Decisions D-16…D-23 in `docs/DECISIONS.md`.

## What's here

| Folder | What | Use it for |
|---|---|---|
| `html/` | **Standalone, interactive HTML** for all 12 screens + `index.html` gallery. Open `html/index.html` in a browser. | The primary reference. Click through the sidebar, run Quick Clean, Rescan, drill into folders, switch Settings tabs. |
| `screens/` | PNG screenshots: every screen plus key animation end-states and named states (`*b-*`, `*c-*`, ...). | Quick visual reference; evidence images for GitHub issues. |
| `source/` | Original `.dc.html` artboards + `canvas.json` from the design canvas. All styling is inline; logic is in the `<script data-dc-script>` block. | Exact values (colors, spacing, copy, animation timings, mock data). |
| `preview/` | `support.js` (tiny runtime that renders `.dc.html`) + `shoot.js` (Playwright screenshot script). | Re-rendering screenshots after design changes. |
| `tokens.json` | Colors, tier fills, type scale, radii, spacing, motion. | Seed for the Tailwind theme / CSS variables in the Tauri app. |

## Screens

| # | Screen | Key interactions / animations |
|---|---|---|
| 01 | Overview (`Main`) | Storage wheel (click segments, tier filter toolbar), radar-sweep Rescan, Quick Clean with particle burst, forecast alert, live drives widget, logo wink |
| 02 | First-run onboarding | Detection-first 5 questions, live summary, animated first scan |
| 03 | Cleanup Review | Selectable plan rows, rows sweep out while cleaning, check-draw success + measured result |
| 04 | Git Worktrees | Per-worktree checks and verdict, animated removal + toast |
| 05 | Insights | Forecast chart, regrowth, duplication, agent footprint, accuracy, ecosystem mix |
| 06 | Projects | Ecosystem filters, tier bars, per-project clean with shrink animation |
| 07 | Storage Explorer | Drives root ("This Mac"), drill-down treemap with zoom, breadcrumbs |
| 08 | Caches | Prune/Wipe per tool, sequential cleaning with per-row progress, burst |
| 09 | History | Operation timeline, audit log, animated Restore |
| 10 | Settings | 7 tabs (General, Drives, Safety, Alerts, AI agents, Data & privacy, About); sliding switches; "Saved" pill |
| 11 | Menu bar widget | Ring, reclaimable, one-tap Quick Clean, alerts, Check now |
| 12 | MCP approval (`McpApproval`) | Cleanup Review in agent request mode (PRD §13.3, D-7): who asked, live 15-min countdown, plan fingerprint, per-item review, typed confirmation code, Approve/Deny, revoke, agent run with per-item progress and measured result; expired, denied and plan-changed states; OS notification (macOS, Windows) and the `scull approve` terminal equivalent |

Screens with several states take a URL hash, so each state can be opened directly, e.g. `McpApproval.html#state=expired`. The states are listed in a small "design states" row at the bottom of the page.

## Implementation notes for the Tauri/Vue app

- **The HTML is a reference, not production code.** Rebuild the screens as Vue 3 components with Tailwind using `tokens.json`. Keep copy, layout proportions, tier encodings and animation timings.
- **Data in the mocks is illustrative** (one imagined Mac with 186.0 GB of development data). Numbers are internally consistent across screens; wire real data from the core.
- **Tier encoding must never rely on color alone:** keep the stripe and hatch patterns.
- **Accessibility:** real `<button>`, `<a>`, `<input>` + labels; 44px targets; WCAG 2.2 AA contrast (checked for the main text/fill pairs); `prefers-reduced-motion` disables animation.
- **Not designed yet:** Overview drive switcher, dark theme, empty/error states, Windows-specific chrome.

## Re-rendering screenshots

```bash
cd design/preview
npm init -y && npm install playwright @fontsource/dm-sans @fontsource/dm-mono
node shoot.js ../screens   # reads ../source by default; override with SRC=/path
```
