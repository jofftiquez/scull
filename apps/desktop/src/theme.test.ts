import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { darkDeclarations, kebab, renderTheme, THEME_PATH, TOKENS_PATH } from '../scripts/gen-theme.mjs';

const tokens = JSON.parse(readFileSync(TOKENS_PATH, 'utf8'));
const css = readFileSync(THEME_PATH, 'utf8');

describe('theme generated from design/tokens.json', () => {
  it('is up to date (run `pnpm --filter @scull/desktop tokens`)', () => {
    expect(css).toBe(renderTheme(tokens));
  });

  it('carries every color, radius and type size', () => {
    for (const v of Object.values<string>(tokens.color)) expect(css).toContain(v);
    for (const v of Object.values<number>(tokens.radius)) expect(css).toContain(`${v}px`);
    for (const v of Object.values<number>(tokens.font.scale)) expect(css).toContain(`${v}px`);
  });

  it('keeps the tier patterns (never color alone)', () => {
    expect(css).toContain('--tier-s2-fill: repeating-linear-gradient');
    expect(css).toContain('--tier-s3-fill: repeating-linear-gradient');
  });

  it('emits every variable, used by a utility or not (inline styles read them)', () => {
    expect(css).toContain('@theme static {');
  });

  it('removes the default palette and every shadow (D-16, D-17)', () => {
    expect(css).toContain('--color-*: initial;');
    expect(css).toContain('--shadow-*: initial;');
    expect(css).toContain('--drop-shadow-*: initial;');
  });
});

describe('dark theme (P5-UI-01)', () => {
  const darkBlock = (from: string) => css.slice(css.indexOf(from));

  it('every light color has a dark value, and nothing else', () => {
    expect(Object.keys(tokens.dark.color).sort()).toEqual(Object.keys(tokens.color).sort());
  });

  it('follows the system unless Settings forces a theme (html[data-theme])', () => {
    expect(css).toContain("@media (prefers-color-scheme: dark) {\n  :root:not([data-theme='light']) {");
    expect(css).toContain("[data-theme='dark']:root {");
    expect(css).toMatch(/:root \{\n {2}color-scheme: light;/);
  });

  it('both dark blocks carry every dark color, the dark tier fills and color-scheme: dark', () => {
    for (const from of ['@media (prefers-color-scheme: dark)', "[data-theme='dark']:root"]) {
      const block = darkBlock(from);
      expect(block).toContain('color-scheme: dark;');
      for (const [k, v] of Object.entries<string>(tokens.dark.color)) {
        expect(block).toContain(`--color-${kebab(k)}: ${v};`);
      }
      expect(block).toContain(`--tier-s3-fill: ${tokens.dark.tier.S3_protected.fill};`);
      expect(block).toContain(`--color-tier-s3-swatch: ${tokens.dark.tier.S3_protected.swatch};`);
    }
  });

  it('dark tiers keep their patterns (never color alone)', () => {
    const block = darkBlock("[data-theme='dark']:root");
    expect(block).toContain('--tier-s2-fill: repeating-linear-gradient');
    expect(block).toContain('--tier-s3-fill: repeating-linear-gradient');
  });

  it('a token without a dark value fails the generator (and so `pnpm lint`)', () => {
    const broken = structuredClone(tokens);
    broken.color.newThing = '#123456';
    expect(() => darkDeclarations(broken)).toThrow(/missing: newThing/);
    expect(() => renderTheme({ ...tokens, dark: undefined })).toThrow(/dark\.color/);
  });

  it('is not the rejected dark purple theme (D-16): the ground is a neutral charcoal', () => {
    const [r, g, b] = [1, 3, 5].map((i) => parseInt(tokens.dark.color.ground.slice(i, i + 2), 16));
    expect(Math.max(r!, g!, b!) - Math.min(r!, g!, b!)).toBeLessThanOrEqual(4);
    expect(b!).toBeLessThanOrEqual(r!);
  });
});

describe('no raw colors in sources (every color follows the theme)', () => {
  it('components use theme variables, not hex/rgb literals', () => {
    const files = import.meta.glob('./**/*.{vue,ts,css}', { query: '?raw', import: 'default', eager: true });
    const raw = /#[0-9a-f]{3,8}\b|\brgba?\(|\bhsla?\(/i;
    const offenders: string[] = [];
    for (const [name, text] of Object.entries(files)) {
      if (name.endsWith('theme.css') || /\.test\.ts$/.test(name) || name.includes('/fixtures/')) continue;
      String(text)
        .split('\n')
        .forEach((l, i) => {
          const code = l.trim();
          if (/^(\/\/|\*|\/\*|<!--)/.test(code)) return;
          // Issue references (`#218`) are not colors.
          if (raw.test(code.replace(/#\d+\b/g, ''))) offenders.push(`${name}:${i + 1}: ${code.slice(0, 80)}`);
        });
    }
    expect(offenders).toEqual([]);
  });
});

describe('no shadows in sources', () => {
  it('no component uses a shadow utility or box-shadow', async () => {
    const files = import.meta.glob('./**/*.{vue,css}', { query: '?raw', import: 'default', eager: true });
    const offenders = Object.entries(files)
      .filter(([, text]) => /\bshadow-|box-shadow|drop-shadow/.test(String(text)))
      .map(([name]) => name)
      .filter((name) => !name.endsWith('theme.css'));
    expect(offenders).toEqual([]);
  });
});
