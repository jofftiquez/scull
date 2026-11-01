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

describe('color contrast of the token pairs used for text (1.4.3, 4.5:1)', () => {
  const c = tokens.color;
  const pairs: [string, string, string][] = [
    ['ink on ground', c.ink, c.ground],
    ['secondary text on surface', c.textSecondary, c.surface],
    ['secondary text on ground', c.textSecondary, c.ground],
    ['secondary text on raised', c.textSecondary, c.surfaceRaised],
    ['tertiary text on surface', c.textTertiary, c.surface],
    ['tertiary text on raised (placeholders)', c.textTertiary, c.surfaceRaised],
    ['hint text on surface', c.textHint, c.surface],
    ['strong muted on accent tint', c.textStrongMuted, c.accentTint],
    ['accent text on accent tint', c.accentText, c.accentTint],
    ['violet text on violet tint', c.violetText, c.violetTint],
    ['on-ink on ink', c.onInk, c.ink],
    ['white on violet (Insights review button)', '#FFFFFF', tokens.tier.S0_regenerable.fill],
    // Tier badges: the text against both colors of each pattern.
    ['S0 text on S0', tokens.tier.S0_regenerable.text, tokens.tier.S0_regenerable.fill],
    ['S1 text on S1', tokens.tier.S1_costly.text, tokens.tier.S1_costly.fill],
    ['S2 text on stripe 1', tokens.tier.S2_uncertain.text, '#FFB547'],
    ['S2 text on stripe 2', tokens.tier.S2_uncertain.text, '#FFD08A'],
    ['S3 text on hatch 1', tokens.tier.S3_protected.text, '#3B2A4F'],
    ['S3 text on hatch 2', tokens.tier.S3_protected.text, '#5A4772'],
  ];
  for (const [name, fg, bg] of pairs) {
    it(`${name}: ${fg} on ${bg}`, () => {
      expect(contrast(fg, bg)).toBeGreaterThanOrEqual(4.5);
    });
  }

  it('hint gray is not used on the raised field (it is 4.1:1 there)', () => {
    expect(contrast(tokens.color.textHint, tokens.color.surfaceRaised)).toBeLessThan(4.5);
    expect(mainCss).toMatch(/input::placeholder \{\s*color: var\(--color-text-tertiary\)/);
  });
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
