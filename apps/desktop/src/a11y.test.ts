// Accessibility guarantees (P3-A11Y-01, WCAG 2.2 AA) that unit tests can hold on to. The full
// per-route audit (axe-core, target sizes, keyboard pass, reduced motion) is a11y/audit.sh;
// docs/ACCESSIBILITY.md has its report.
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import App from './App.vue';
import { BRIDGE_KEY } from './bridge';
import { createMockBridge } from './bridge/mock';
import { routes } from './navigation';
import { THEME_PATH, TOKENS_PATH } from '../scripts/gen-theme.mjs';

const tokens = JSON.parse(readFileSync(TOKENS_PATH, 'utf8'));
const theme = readFileSync(THEME_PATH, 'utf8');
const mainCss = readFileSync(resolve(__dirname, 'styles/main.css'), 'utf8');

function luminance(hex: string): number {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r! + 0.7152 * g! + 0.0722 * b!;
}
function contrast(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi! + 0.05) / (lo! + 0.05);
}

type Tier = { fill: string; solid?: string; stripe?: string; swatch?: string; text: string; fg: string };
type TierKey = 'S0_regenerable' | 'S1_costly' | 'S2_uncertain' | 'S3_protected';
interface Palette {
  c: Record<string, string>;
  t: Record<TierKey, Tier>;
}
/** Light, and dark merged over light (as scripts/gen-theme.mjs emits it; P5-UI-01). */
const PALETTES: Record<'light' | 'dark', Palette> = {
  light: { c: tokens.color, t: tokens.tier },
  dark: {
    c: { ...tokens.color, ...tokens.dark.color },
    t: Object.fromEntries(Object.entries<Tier>(tokens.tier).map(([k, v]) => [k, { ...v, ...(tokens.dark.tier?.[k] ?? {}) }])) as Palette['t'],
  },
};
const solid = (t: Tier) => t.solid ?? t.fill;
/** The two colors of a tier's pattern (one for a solid tier). */
const bands = (t: Tier) => [solid(t), ...(t.stripe ? [t.stripe] : [])];

for (const [mode, { c, t }] of Object.entries(PALETTES)) {
  describe(`${mode} theme: contrast of the token pairs used for text (1.4.3, 4.5:1)`, () => {
    const pairs: [string, string, string][] = [
      ['ink on ground', c.ink!, c.ground!],
      ['ink on surface', c.ink!, c.surface!],
      ['ink on raised', c.ink!, c.surfaceRaised!],
      ['ink on category ring (treemap folders)', c.ink!, c.categoryRing!],
      ['ink on violet tint (worktree verdict)', c.ink!, c.violetTint!],
      ['ink on amber tint (worktree verdict)', c.ink!, c.amberTint!],
      ['secondary text on surface', c.textSecondary!, c.surface!],
      ['secondary text on ground', c.textSecondary!, c.ground!],
      ['secondary text on raised', c.textSecondary!, c.surfaceRaised!],
      ['tertiary text on surface', c.textTertiary!, c.surface!],
      ['tertiary text on raised (placeholders)', c.textTertiary!, c.surfaceRaised!],
      ['hint text on surface', c.textHint!, c.surface!],
      ['strong muted on surface', c.textStrongMuted!, c.surface!],
      ['strong muted on accent tint', c.textStrongMuted!, c.accentTint!],
      ['accent text on accent tint', c.accentText!, c.accentTint!],
      ['accent text on surface (center label)', c.accentText!, c.surface!],
      ['accent text on ground (center label)', c.accentText!, c.ground!],
      ['violet text on violet tint', c.violetText!, c.violetTint!],
      ['violet text on surface', c.violetText!, c.surface!],
      ['violet figure on surface (reclaim hero)', c.violetFigure!, c.surface!],
      ['amber text on amber tint (avatars)', c.amberText!, c.amberTint!],
      ['on-ink on ink', c.onInk!, c.ink!],
      ['on-ink on ink raised (running Quick Clean)', c.onInk!, c.inkRaised!],
      ['on-ink soft on ink (audit log)', c.onInkSoft!, c.ink!],
      ['on-ink muted on ink (Quick Clean hero)', c.onInkMuted!, c.ink!],
      ['on-ink subtle on ink (log times)', c.onInkSubtle!, c.ink!],
      ['on-ink violet on ink (toasts, log)', c.onInkViolet!, c.ink!],
      ['on-ink magenta on ink (log)', c.onInkMagenta!, c.ink!],
      ['on-ink amber on ink (log)', c.onInkAmber!, c.ink!],
      ['on-fill on violet (Insights review button)', c.onFill!, solid(t.S0_regenerable)],
      ['on-fill on violet hover', c.onFill!, c.violetHover!],
      ['drive tile label (explorer)', c.onDriveTile!, c.driveTile!],
      ['protected label on category ring (history WT icon, avatars)', t.S3_protected.fg, c.categoryRing!],
    ];
    // Tier names in tier color (center label, legends) on the page.
    for (const [k, tier] of Object.entries(t)) {
      pairs.push([`${k} label color on ground`, tier.fg, c.ground!], [`${k} label color on surface`, tier.fg, c.surface!]);
      // Tier badges: the text against both colors of each pattern.
      for (const band of bands(tier)) pairs.push([`${k} text on its fill`, tier.text, band]);
    }
    for (const [name, fg, bg] of pairs) {
      it(`${name}: ${fg} on ${bg}`, () => {
        expect(contrast(fg, bg)).toBeGreaterThanOrEqual(4.5);
      });
    }

    it('large figures in light violet (36px stat tiles) meet 3:1 (1.4.3 large text)', () => {
      expect(contrast(c.violetLight!, c.surface!)).toBeGreaterThanOrEqual(3);
    });
  });

  describe(`${mode} theme: non-text contrast (1.4.11, 3:1)`, () => {
    const marks: [string, string][] = [
      ['focus ring (accent)', c.accent!],
      ['regenerable (S0) segments and bars', solid(t.S0_regenerable)],
      ['protected (S3) swatch', t.S3_protected.swatch ?? solid(t.S3_protected)],
      ['forecast line and cleanup markers', c.violetFigure!],
    ];
    for (const [name, color] of marks) {
      it(`${name} against surface and ground`, () => {
        expect(contrast(color, c.surface!)).toBeGreaterThanOrEqual(3);
        expect(contrast(color, c.ground!)).toBeGreaterThanOrEqual(3);
      });
    }
  });
}

describe('light theme: the hint gray', () => {
  it('is not used on the raised field (it is 4.1:1 there)', () => {
    expect(contrast(tokens.color.textHint, tokens.color.surfaceRaised)).toBeLessThan(4.5);
    expect(mainCss).toMatch(/input::placeholder \{\s*color: var\(--color-text-tertiary\)/);
  });
});

// Colour-vision deficiency: Machado, Oliveira & Fernandes (2009) simulation matrices at full
// severity, applied in linear RGB, compared in CIELAB (ΔE*76).
const CVD: Record<string, number[][]> = {
  protanopia: [
    [0.152286, 1.052583, -0.204868],
    [0.114503, 0.786281, 0.099216],
    [-0.003882, -0.048116, 1.051998],
  ],
  deuteranopia: [
    [0.367322, 0.860646, -0.227968],
    [0.280085, 0.672501, 0.047413],
    [-0.01182, 0.04294, 0.968881],
  ],
  tritanopia: [
    [1.255528, -0.076749, -0.178779],
    [-0.078411, 0.930809, 0.147602],
    [0.004733, 0.691367, 0.3039],
  ],
};
const linear = (hex: string) =>
  [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
function lab([r, g, b]: number[]): number[] {
  const f = (x: number) => (x > 0.008856 ? Math.cbrt(x) : 7.787 * x + 16 / 116);
  const X = (0.4124 * r! + 0.3576 * g! + 0.1805 * b!) / 0.95047;
  const Y = 0.2126 * r! + 0.7152 * g! + 0.0722 * b!;
  const Z = (0.0193 * r! + 0.1192 * g! + 0.9505 * b!) / 1.08883;
  return [116 * f(Y) - 16, 500 * (f(X) - f(Y)), 200 * (f(Y) - f(Z))];
}
const simulate = (hex: string, m: number[][]) =>
  m.map((row) => Math.min(1, Math.max(0, row.reduce((a, k, j) => a + k * linear(hex)[j]!, 0))));
const deltaE = (a: number[], b: number[]) => Math.hypot(...a.map((v, i) => v - b[i]!));
const lightness = (hex: string) => lab(linear(hex))[0]!;

describe('tiers stay apart for colour-blind viewers (S0–S3, both themes)', () => {
  for (const [mode, { t }] of Object.entries(PALETTES)) {
    const keys = Object.keys(t) as TierKey[];
    for (let i = 0; i < keys.length; i++) {
      for (let j = i + 1; j < keys.length; j++) {
        const [a, b] = [t[keys[i]!], t[keys[j]!]];
        it(`${mode}: ${keys[i]} vs ${keys[j]}`, () => {
          // Dichromats: clearly different colors (ΔE ≥ 20; "just noticeable" is ~2.3).
          for (const [kind, m] of Object.entries(CVD)) {
            expect(deltaE(lab(simulate(solid(a), m)), lab(simulate(solid(b), m))), kind).toBeGreaterThanOrEqual(20);
          }
          // No color at all (achromatopsia, grayscale print): a lightness step, or one of the
          // two carries a pattern the other doesn't (stripes / hatch).
          const patterned = (x: Tier) => x.fill.startsWith('repeating-');
          const step = Math.abs(lightness(solid(a)) - lightness(solid(b)));
          expect(step >= 10 || patterned(a) !== patterned(b), `lightness step ${step.toFixed(1)}`).toBe(true);
        });
      }
    }
  }
});

describe('targets and motion', () => {
  it('the spacing unit is 4 px, so min-h-11 / min-h-touch are 44 px (2.5.8, scull: 44 px)', () => {
    expect(theme).toContain('--spacing: 4px;');
    expect(theme).toContain(`--spacing-touch: ${tokens.minTouchTarget}px;`);
    expect(tokens.minTouchTarget).toBe(44);
  });

  it('prefers-reduced-motion collapses every CSS animation and transition (2.3.3)', () => {
    const block = mainCss.slice(mainCss.indexOf('@media (prefers-reduced-motion: reduce)'));
    expect(block).toContain('animation-duration: 1ms !important');
    expect(block).toContain('animation-iteration-count: 1 !important');
    expect(block).toMatch(/transition-duration: [0-9.]+m?s !important/);
  });
});

async function mountApp(path: string) {
  const router = createRouter({ history: createMemoryHistory(), routes });
  router.push(path);
  await router.isReady();
  const w = mount(App, { attachTo: document.body, global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge() } } });
  await flushPromises();
  return { w, router };
}

describe('shell navigation (2.4.1, 2.4.2, 2.4.3)', { timeout: 20_000 }, () => {
  it('the first Tab stop skips to the screen, which takes focus', async () => {
    const { w } = await mountApp('/insights');
    const skip = w.get('[data-testid="skip-link"]');
    expect(skip.attributes('href')).toBe('#content');
    // It is the first focusable element of the shell.
    const first = w.element.querySelector('a[href], button, input, [tabindex="0"]');
    expect(first).toBe(skip.element);
    await skip.trigger('click');
    expect(document.activeElement?.id).toBe('content');
    w.unmount();
  });

  it('each screen has its own title and navigating moves focus to the new screen', async () => {
    const { w, router } = await mountApp('/insights');
    expect(document.title).toBe('Insights · scull');
    await router.push('/worktrees');
    await vi.waitUntil(async () => {
      await flushPromises();
      return document.activeElement?.id === 'content';
    }, { timeout: 10_000 });
    expect(document.title).toBe('Git Worktrees · scull');
    w.unmount();
  });
});
