import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { renderTheme, THEME_PATH, TOKENS_PATH } from '../scripts/gen-theme.mjs';

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
