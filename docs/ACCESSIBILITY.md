# Accessibility audit: desktop app (P3-A11Y-01)

Target: **WCAG 2.2 AA** for every screen (PRD §14.4, UX-01), plus scull's own rules: 44 px
touch targets (`design/tokens.json` `minTouchTarget`), tiers never shown by color alone, and
every animation honoring `prefers-reduced-motion`.

Audited on Nov 1, 2026: `main` at c109da6 (with Storage Explorer, the Overview drive switcher,
History and Settings) plus P3-UI-10 (Insights). Run on the mock bridge in Playwright's Chromium at
1440 × 900.

## How it was audited

`apps/desktop/a11y/audit.sh` runs `a11y/audit.mjs` in the Playwright container against a
running dev server. For each of 12 routes and states (Overview, Projects and its design demo,
Storage Explorer, Git Worktrees, Caches, Cleanup Review, Insights and its design demo,
History, Settings, Onboarding) it runs four checks:

1. **axe-core 4.14** with the `wcag2a`, `wcag2aa`, `wcag21a`, `wcag21aa`, `wcag22aa` and
   `best-practice` rules. Nodes axe can't decide ("incomplete", e.g. text over a gradient)
   are listed for a manual check.
2. **Targets.** Every visible, enabled button, link, field and focusable element must be at
   least 44 × 44 px, measured from the rendered box. A field inside a `<label>` counts the
   label's box. A hit area enlarged by an absolutely positioned `::before`/`::after` with
   negative insets (Storage Explorer's toolbar, Settings' segmented controls) counts the
   enlarged area. Inline links in running text and
   disabled controls are exempt, as in WCAG 2.5.8.
3. **Keyboard.** The audit tabs through the whole page from the top. Every stop must show a
   focus indicator: an outline, an SVG focus stroke, or a `:focus-within` ring on the field's
   wrapper. The order must wrap around without a trap.
4. **Reduced motion.** The page is loaded again with `prefers-reduced-motion: reduce`, and no
   finite animation may still be running once the page has loaded.

```bash
SCULL_DEV_PORT=5291 pnpm --filter @scull/desktop dev &
AXE_NODE_MODULES=<dir> PW_NODE_MODULES=<dir> apps/desktop/a11y/audit.sh http://localhost:5291 out/
```

**axe-core is MPL-2.0.** scull's license policy forbids MPL code in what we ship (D-2,
`deny.toml`). axe-core is therefore not a dependency of the repository at all, not even a
dev dependency. Like Playwright, it is installed in a separate tooling directory
(`npm install axe-core@4.14.0`) and mounted into the container. It never enters
`package.json`, the lockfile or a bundle.

Unit tests hold the parts that can be checked without a browser (`src/a11y.test.ts`): token
contrast pairs, the 44 px spacing unit, the reduced-motion rule, the skip link, page titles
and focus after navigation. The Insights tests cover the forecast chart's keyboard use and its
data table.

## Results

| | Before | After |
|---|---:|---:|
| axe violations (all routes) | 6 | **0** |
| Targets under 44 px (not inline) | 84 | **0** |
| Tab stops without a visible indicator | 11 | **0** |
| Finite animations running under reduced motion | 0 | **0** |
| Keyboard traps | 0 | **0** |

### Fixed

| Issue | WCAG | Where | Fix |
|---|---|---|---|
| Most controls rendered 38.5 px tall: `min-h-11` used Tailwind's `0.25rem` unit while the root font size is the 14 px body size. All rem-based spacing was 12.5% smaller than the design's pixels. | 2.5.8 (and scull's 44 px) | every screen | Fixed centrally in the tokens-derived theme: the spacing unit is now `4px` (`scripts/gen-theme.mjs`), so `min-h-11` is 44 px and `px-6` is the design's 24 px. `min-h-touch` / `min-w-touch` (44 px, `tokens.minTouchTarget`) remain the explicit target utilities. |
| Sidebar "Support" / "I have a key" were 32 px; the Overview header actions 35 px; Quick Clean chips 34 px; the "All" project filter 40 px wide; "Manage" was an 18 px text link. | 2.5.8 | shell, Overview, Projects | `min-h-11` / `min-h-touch` / `min-w-touch`; "Manage" is a 44 px target with negative margins, so the drives header keeps its height. |
| The drive switcher's `aria-label` ("All drives: 20 items, …") didn't contain the visible text ("All drives 338.7 GiB"). | 2.5.3 | Overview | No `aria-label`: the name is the visible text plus visually hidden details ("… 92% used, 16 items, 186.0 GiB"). |
| The drive kind ("· external") was drawn at 75% opacity: 4.3:1. | 1.4.3 | Overview | Full opacity (5.7:1). |
| Settings' segmented controls ("1 h", "6 h", "24 h") were 39–43 px wide. | 2.5.8 | Settings | `min-w-touch`. |
| No level-one heading on Overview. | 1.3.1, 2.4.6 | Overview | Visually hidden `<h1>Overview</h1>`. |
| `role="list"` contained bare `<h3>` section titles, and the heading levels skipped from h1 to h3. | 1.3.1 | Cleanup Review (desktop and agent modes) | The titles sit in `role="listitem"` wrappers, and the desktop review uses `<h2>`. |
| Onboarding had no `<main>` landmark. | 1.3.1 | Onboarding | Its root is `<main>`. |
| The search field removed its outline and showed no focus. | 2.4.7, 2.4.11 | sidebar | A `:focus-within` ring on the field. |
| Placeholder text used the hint gray: 4.1:1 on the raised field. | 1.4.3 | sidebar search | Placeholders use the tertiary text color (4.7:1). |
| No way to skip the nine navigation links. | 2.4.1 | shell | "Skip to main content" is the first Tab stop. It is visible on focus and moves focus to the screen's `<main id="content">`. |
| Every screen had the same document title. | 2.4.2 | shell | "Insights · scull", "Git Worktrees · scull", … |
| After navigating, focus stayed on the sidebar link. | 2.4.3 | shell | Focus moves to the new screen's `<main>`. |
| Design 05's "until less than 10 GB free" was #D8D3CB on the pink tint (1.4:1). | 1.4.3 | Insights (P3-UI-10) | Uses the strong muted text color (8.4:1). |

### Checked by hand

- **Text over patterns.** axe can't compute contrast on the striped and hatched tier badges or
  on the wheel's labels. The text was checked against both colors of each pattern: ink on the
  amber stripes is 10.6:1 and 12.9:1, white on the plum hatch 12.9:1 and 8.1:1. The wheel's
  labels are ink on the category ring (15.6:1). These pairs are in `src/a11y.test.ts`.
- **Tiers are never color alone.** Every tier is shown with its label and, for areas, the
  stripe (S2) or hatch (S3) pattern: wheel, badges, toolbar, Insights mix (ML stripes).
- **Charts.** The Overview wheel is one tab stop. Arrow keys move between segments, Enter
  selects, and each segment has a label. The Insights forecast is one tab stop: Left/Right
  move between days, Home/End jump, and the value is read out through a live region. Every
  Insights chart has a summary label (`role="img"`) or a visually hidden data table. Charts
  follow D-37 (`docs/CHARTS.md`).
- **Reduced motion.** `src/styles/main.css` collapses every CSS animation and transition to
  1 ms under `prefers-reduced-motion: reduce`. Decorative JavaScript motion (the radar sweep,
  Quick Clean, the onboarding scan's minimum duration) reads the same media query and jumps to
  its end state (`src/components/overview/motion.ts`, `OnboardingScreen.vue`). Progress fills
  that track work (cleanups, removals, cache cleans) keep updating as the work advances, but
  without transitions. The audit found no running animation in any route under reduced motion.
- **Focus order** follows the visual order on every route: the skip link, sidebar navigation,
  search, supporter card, drives, then the screen's header actions and content from top left
  to bottom right. Cleanup Review alternates each row's checkbox and its "Show details" button.

### Known gaps (follow-ups)

- **Windows High Contrast / forced colors.** Not audited (needs Windows). The tier patterns
  are SVG/CSS backgrounds and may disappear in forced-colors mode. Badges keep their text
  labels.
- **Screen readers.** The app was checked through the accessibility tree (axe, Playwright
  ARIA snapshots), not with VoiceOver, NVDA or Orca. A manual pass belongs in the v1.0 beta
  usability tests.
- **Zoom / reflow (1.4.10).** The layouts wrap down to 860 px, but 400% zoom (320 CSS px) was
  not audited.
- **New screens** should run `a11y/audit.sh` before they land; it takes about two minutes for
  all routes.
